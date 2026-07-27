use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::interval;

use crate::apt::{HostInfo, PendingConffile};
use crate::cache::{self, Cache};
use crate::config::{Config, HostConfig, SidebarRow};
use crate::ssh::shell_quote;

pub const TASK_OUTPUT_CAP: usize = 5_000;

/// Maximum number of simultaneous SSH operations (gathers and apt tasks
/// combined). Triggering an action on a large group or "all" hosts still
/// queues the rest instead of opening a connection per host at once.
pub const MAX_CONCURRENT_SSH_OPS: usize = 8;

/// Environment every apt invocation runs under: no debconf questions, stable
/// English output for the parsers in `apt.rs`.
const APT_ENV: &str = "DEBIAN_FRONTEND=noninteractive LC_ALL=C";

/// `DEBIAN_FRONTEND` covers debconf only — dpkg's own conffile prompt is
/// separate and reads from stdin, which would hang the task forever over SSH.
/// Take the package's default action where it offers one, otherwise keep the
/// installed file; both leave the running config untouched.
const DPKG_CONF_OPTS: &str =
    r#"-o "Dpkg::Options::=--force-confdef" -o "Dpkg::Options::=--force-confold""#;

// ── Messages flowing from background tasks to the app ────────────────────────

#[derive(Debug)]
pub enum AppMessage {
    GatherDone {
        host_idx: usize,
        result: Result<HostInfo, String>,
    },
    TaskLine {
        host_idx: usize,
        line: String,
    },
    TaskDone {
        host_idx: usize,
        exit_code: i32,
    },
    TaskFailed {
        host_idx: usize,
        error: String,
    },
    ConffileDiff {
        host_idx: usize,
        pending_path: String,
        result: Result<Vec<String>, String>,
    },
}

// ── Per-host status ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum HostStatus {
    Unknown,
    Connecting,
    Gathering,
    Ready,
    Error(String),
}

// ── Task kinds (operations triggered by the user) ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum TaskKind {
    Update,
    Upgrade,
    UpgradeSecurity(Vec<String>),
    FullUpgrade,
    AutoRemove,
    PurgeRc,
    /// Execute a batch of reviewed conffile decisions in one SSH task.
    ResolveConffiles(Vec<ConffileDecision>),
    Reboot,
}

/// What the user decided to do with one pending conffile in the review modal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConffileAction {
    /// Delete the pending file, leaving the live config untouched.
    Discard,
    /// Move the pending file into place, backing the live config up to
    /// `.dpkg-old` — the same convention dpkg's own prompt follows.
    Apply,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConffileDecision {
    pub file: PendingConffile,
    pub action: ConffileAction,
}

impl TaskKind {
    pub fn label(&self) -> String {
        match self {
            TaskKind::Update => "apt-get update".to_string(),
            TaskKind::Upgrade => "apt-get upgrade".to_string(),
            TaskKind::UpgradeSecurity(pkgs) => {
                format!("apt-get upgrade (security, {} pkg(s))", pkgs.len())
            }
            TaskKind::FullUpgrade => "apt-get full-upgrade".to_string(),
            TaskKind::AutoRemove => "apt-get autoremove --purge".to_string(),
            TaskKind::PurgeRc => "purge RC packages".to_string(),
            TaskKind::ResolveConffiles(decisions) => {
                let applies = decisions
                    .iter()
                    .filter(|d| d.action == ConffileAction::Apply)
                    .count();
                format!(
                    "resolve config files ({} discard, {applies} apply)",
                    decisions.len() - applies
                )
            }
            TaskKind::Reboot => "reboot".to_string(),
        }
    }

    pub fn command(&self, use_sudo: bool) -> String {
        let sudo = if use_sudo { "sudo -n " } else { "" };
        match self {
            TaskKind::Update => {
                format!("{APT_ENV} {sudo}apt-get update </dev/null 2>&1")
            }
            TaskKind::Upgrade => {
                format!("{APT_ENV} {sudo}apt-get -y {DPKG_CONF_OPTS} upgrade </dev/null 2>&1")
            }
            TaskKind::UpgradeSecurity(pkgs) => {
                let names = pkgs.join(" ");
                format!(
                    "{APT_ENV} {sudo}apt-get install --only-upgrade -y {DPKG_CONF_OPTS} {names} </dev/null 2>&1"
                )
            }
            TaskKind::FullUpgrade => {
                format!("{APT_ENV} {sudo}apt-get -y {DPKG_CONF_OPTS} full-upgrade </dev/null 2>&1")
            }
            TaskKind::AutoRemove => {
                format!(
                    "{APT_ENV} {sudo}apt-get -y {DPKG_CONF_OPTS} autoremove --purge </dev/null 2>&1"
                )
            }
            TaskKind::PurgeRc => format!(
                r#"pkgs=$(LC_ALL=C dpkg -l | awk '/^rc/{{print $2}}'); [ -n "$pkgs" ] && echo "$pkgs" | xargs {sudo}dpkg --purge 2>&1 || echo "No RC packages to purge""#
            ),
            // Each decision runs in isolation: a failure is reported and sets
            // the exit code, but never stops the remaining files from being
            // resolved. Paths are quoted everywhere they appear, including
            // inside the progress `echo`s — a filename is allowed to contain
            // `$` and backticks, and these strings are built from remote
            // `find` output.
            TaskKind::ResolveConffiles(decisions) => {
                let mut parts: Vec<String> = vec!["fail=0".to_string()];
                for d in decisions {
                    let pending = shell_quote(&d.file.pending_path);
                    match d.action {
                        ConffileAction::Discard => parts.push(format!(
                            "{sudo}rm -f -- {pending} && echo discarded {pending} \
                             || {{ echo FAILED: {pending}; fail=1; }}"
                        )),
                        ConffileAction::Apply => {
                            let live = shell_quote(&d.file.live_path);
                            let backup = shell_quote(&format!("{}.dpkg-old", d.file.live_path));
                            // Back up first, inside `set -e`, so a failed copy
                            // can never leave the host without its original
                            // config — the move simply doesn't happen.
                            parts.push(format!(
                                "( set -e; if [ -e {live} ]; then {sudo}cp -a -- {live} {backup}; \
                                 echo 'backed up' {live} '->' {backup}; fi; \
                                 {sudo}mv -- {pending} {live} ) \
                                 && echo installed {live} || {{ echo FAILED: {live}; fail=1; }}"
                            ));
                        }
                    }
                }
                parts.push("exit $fail".to_string());
                format!("{{ {}; }} 2>&1", parts.join("; "))
            }
            TaskKind::Reboot => format!("{sudo}reboot 2>&1"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskStatus {
    Running,
    Done(i32),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct TaskState {
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub output: VecDeque<String>,
    pub scroll_offset: u16,
    pub auto_scroll: bool,
}

impl TaskState {
    pub fn new(kind: TaskKind) -> Self {
        Self {
            kind,
            status: TaskStatus::Running,
            output: VecDeque::new(),
            scroll_offset: 0,
            auto_scroll: true,
        }
    }

    pub fn push_line(&mut self, line: String) {
        if self.output.len() >= TASK_OUTPUT_CAP {
            self.output.pop_front();
        }
        self.output.push_back(line);
    }
}

// ── Per-host application state ────────────────────────────────────────────────

#[derive(Debug)]
pub struct HostState {
    pub cfg: HostConfig,
    pub status: HostStatus,
    pub info: Option<HostInfo>,
    pub task: Option<TaskState>,
    /// True when `info` was loaded from the on-disk cache and hasn't been
    /// confirmed by a live gather yet this run.
    pub is_stale: bool,
}

impl HostState {
    pub fn new(cfg: HostConfig) -> Self {
        Self {
            cfg,
            status: HostStatus::Unknown,
            info: None,
            task: None,
            is_stale: false,
        }
    }
}

// ── Pending conffile review modal state ───────────────────────────────────────

/// Diff of one pending conffile against the live file it would replace.
#[derive(Debug, Clone)]
pub enum DiffState {
    Loading,
    Loaded(Vec<String>),
    Failed(String),
}

#[derive(Debug)]
pub struct ConffileReviewState {
    pub host_idx: usize,
    /// Snapshot taken when the modal opened; the live gather may move on.
    pub files: Vec<PendingConffile>,
    /// Marks parallel to `files`; `None` means undecided. Nothing touches the
    /// host until the batch is executed with Enter.
    pub decisions: Vec<Option<ConffileAction>>,
    pub selected: usize,
    /// Diffs keyed by pending path, fetched lazily as files are selected.
    pub diffs: std::collections::HashMap<String, DiffState>,
    pub scroll: u16,
    /// True once the user has pressed Enter with marks set; the next key
    /// confirms or cancels executing the batch.
    pub confirm_execute: bool,
    /// Transient footer message, e.g. why an action was refused.
    pub notice: Option<String>,
}

impl ConffileReviewState {
    pub fn selected_file(&self) -> Option<&PendingConffile> {
        self.files.get(self.selected)
    }

    pub fn selected_diff(&self) -> Option<&DiffState> {
        self.diffs.get(&self.selected_file()?.pending_path)
    }

    /// Marked files paired with their action, in list order.
    pub fn marked_decisions(&self) -> Vec<ConffileDecision> {
        self.files
            .iter()
            .zip(&self.decisions)
            .filter_map(|(file, d)| {
                d.map(|action| ConffileDecision {
                    file: file.clone(),
                    action,
                })
            })
            .collect()
    }

    /// (discards, applies) currently marked.
    pub fn marked_counts(&self) -> (usize, usize) {
        let discards = self
            .decisions
            .iter()
            .filter(|d| **d == Some(ConffileAction::Discard))
            .count();
        let applies = self
            .decisions
            .iter()
            .filter(|d| **d == Some(ConffileAction::Apply))
            .count();
        (discards, applies)
    }

    /// Toggle a mark on the selected file: same action again clears it, a
    /// different action replaces it.
    fn toggle_mark(&mut self, action: ConffileAction) {
        if let Some(slot) = self.decisions.get_mut(self.selected) {
            *slot = if *slot == Some(action) {
                None
            } else {
                Some(action)
            };
        }
    }
}

// ── Reboot confirmation modal state ──────────────────────────────────────────

#[derive(Debug)]
pub struct RebootConfirmState {
    pub host_idx: usize,
    pub input: String,
    /// True when the user pressed Enter with an incorrect hostname.
    pub mismatch: bool,
}

// ── Application state ─────────────────────────────────────────────────────────

const SIDEBAR_MIN_WIDTH: u16 = 10;
const SIDEBAR_MAX_WIDTH: u16 = 100;

pub struct App {
    pub hosts: Vec<HostState>,
    pub sidebar_rows: Vec<SidebarRow>,
    pub selected_row: usize,
    pub tx: UnboundedSender<AppMessage>,
    pub tick: u64,
    /// If Some, the user is viewing the task output overlay for this host.
    pub viewing_task: Option<usize>,
    /// When true, the sidebar is hidden and the detail panel fills the screen.
    pub detail_zoom: bool,
    /// Width of the sidebar panel in columns.
    pub sidebar_width: u16,
    /// Whether the user is currently dragging the sidebar divider.
    pub dragging_sidebar: bool,
    /// When Some, the reboot confirmation modal is active.
    pub reboot_confirm: Option<RebootConfirmState>,
    /// When Some, the pending-conffile review modal is active.
    pub conffile_review: Option<ConffileReviewState>,
    /// When true, the quit confirmation modal is active (shown when tasks are still running).
    pub quit_confirm: bool,
    /// Current sidebar search/filter text. Empty means no filter is applied.
    pub filter: String,
    /// When true, keystrokes are being captured into `filter` instead of
    /// triggering normal keybindings.
    pub filter_editing: bool,
    /// On-disk gather cache, kept in memory and rewritten after each
    /// successful gather.
    pub cache: Cache,
    /// Bounds the number of SSH connections/operations in flight at once.
    pub ssh_semaphore: Arc<Semaphore>,
}

impl App {
    pub fn new(config: &Config, tx: UnboundedSender<AppMessage>) -> Self {
        let host_cfgs = config.resolved_hosts();
        let sidebar_rows = config.sidebar_rows(&host_cfgs);
        let cache = cache::load();
        let mut hosts: Vec<HostState> = host_cfgs.into_iter().map(HostState::new).collect();
        for h in &mut hosts {
            if let Some(entry) = cache.get(&cache::host_key(&h.cfg)) {
                h.info = Some(entry.info.clone());
                h.is_stale = true;
            }
        }
        Self {
            hosts,
            sidebar_rows,
            selected_row: 0,
            tx,
            tick: 0,
            viewing_task: None,
            detail_zoom: false,
            sidebar_width: {
                let max = crossterm::terminal::size()
                    .map(|(w, _)| w.saturating_sub(SIDEBAR_MIN_WIDTH))
                    .unwrap_or(SIDEBAR_MAX_WIDTH);
                crossterm::terminal::size()
                    .map(|(w, _)| w / 2)
                    .unwrap_or(40)
                    .clamp(SIDEBAR_MIN_WIDTH, max)
            },
            dragging_sidebar: false,
            reboot_confirm: None,
            conffile_review: None,
            quit_confirm: false,
            filter: String::new(),
            filter_editing: false,
            cache,
            ssh_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_SSH_OPS)),
        }
    }

    /// Number of hosts with a task currently in progress.
    #[cfg(test)]
    pub fn running_task_count(&self) -> usize {
        self.hosts
            .iter()
            .filter(|h| {
                matches!(
                    h.task.as_ref().map(|t| &t.status),
                    Some(TaskStatus::Running)
                )
            })
            .count()
    }

    /// Number of hosts with any active SSH work: apt tasks or gather/refresh.
    pub fn active_operation_count(&self) -> usize {
        self.hosts
            .iter()
            .filter(|h| {
                matches!(
                    h.task.as_ref().map(|t| &t.status),
                    Some(TaskStatus::Running)
                ) || matches!(h.status, HostStatus::Connecting | HostStatus::Gathering)
            })
            .count()
    }

    /// Indices of all hosts under the currently selected sidebar row.
    pub fn selected_host_indices(&self) -> Vec<usize> {
        match self.sidebar_rows.get(self.selected_row) {
            Some(SidebarRow::Host { host_idx }) => vec![*host_idx],
            Some(SidebarRow::Group { .. }) => {
                // Collect Host rows immediately following this Group row, stopping
                // at the next Group row. Matching by position (not name) avoids
                // incorrectly merging duplicate-named groups. Also stop at an
                // ungrouped host: since `sidebar_rows()` appends top-level hosts
                // directly after the last group with no separating marker, they
                // would otherwise be swept into that last group's selection.
                let mut idxs = Vec::new();
                for row in self.sidebar_rows.iter().skip(self.selected_row + 1) {
                    match row {
                        SidebarRow::Group { .. } => break,
                        SidebarRow::Host { host_idx }
                            if self.hosts[*host_idx].cfg.group.is_some() =>
                        {
                            idxs.push(*host_idx);
                        }
                        SidebarRow::Host { .. } => break,
                    }
                }
                idxs
            }
            None => vec![],
        }
    }

    /// Indices into `sidebar_rows` that should be visible given the current
    /// filter text. A group row is included if its name matches, or if any
    /// of its hosts match (in which case only the matching hosts are
    /// included alongside it). An empty filter shows every row.
    pub fn filtered_row_indices(&self) -> Vec<usize> {
        let q = self.filter.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.sidebar_rows.len()).collect();
        }

        let mut result = Vec::new();
        let mut i = 0;
        while i < self.sidebar_rows.len() {
            match &self.sidebar_rows[i] {
                SidebarRow::Group { name } => {
                    let group_row_idx = i;
                    let mut j = i + 1;
                    // Stop at the next group, or at an ungrouped host — top-level
                    // hosts are appended right after the last group with no
                    // separating marker, so they must not be treated as its
                    // children (see the same fix in `selected_host_indices`).
                    while j < self.sidebar_rows.len() {
                        match &self.sidebar_rows[j] {
                            SidebarRow::Host { host_idx }
                                if self.hosts[*host_idx].cfg.group.is_some() =>
                            {
                                j += 1;
                            }
                            _ => break,
                        }
                    }
                    let group_matches = name.to_lowercase().contains(&q);
                    if group_matches {
                        result.extend(group_row_idx..j);
                    } else {
                        let matched_children: Vec<usize> = (i + 1..j)
                            .filter(|&k| match &self.sidebar_rows[k] {
                                SidebarRow::Host { host_idx } => self.hosts[*host_idx]
                                    .cfg
                                    .hostname
                                    .to_lowercase()
                                    .contains(&q),
                                _ => false,
                            })
                            .collect();
                        if !matched_children.is_empty() {
                            result.push(group_row_idx);
                            result.extend(matched_children);
                        }
                    }
                    i = j;
                }
                SidebarRow::Host { host_idx } => {
                    if self.hosts[*host_idx]
                        .cfg
                        .hostname
                        .to_lowercase()
                        .contains(&q)
                    {
                        result.push(i);
                    }
                    i += 1;
                }
            }
        }
        result
    }

    /// Move the sidebar selection by `delta` positions among the currently
    /// visible (filtered) rows.
    pub fn move_selection(&mut self, delta: i32) {
        let filtered = self.filtered_row_indices();
        if filtered.is_empty() {
            return;
        }
        let current_pos = filtered
            .iter()
            .position(|&r| r == self.selected_row)
            .unwrap_or(0);
        let new_pos = (current_pos as i32 + delta).clamp(0, filtered.len() as i32 - 1) as usize;
        self.selected_row = filtered[new_pos];
    }

    /// If the current selection is hidden by the active filter, jump to the
    /// first visible row instead.
    fn ensure_selection_visible(&mut self) {
        let filtered = self.filtered_row_indices();
        if !filtered.contains(&self.selected_row)
            && let Some(&first) = filtered.first()
        {
            self.selected_row = first;
        }
    }

    /// Trigger a gather refresh for one host. Queues behind `ssh_semaphore`
    /// so that refreshing a large group or "all" hosts doesn't open a
    /// connection per host simultaneously.
    pub fn start_refresh(&self, host_idx: usize) {
        let cfg = self.hosts[host_idx].cfg.clone();
        let tx = self.tx.clone();
        let semaphore = self.ssh_semaphore.clone();
        tokio::spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("ssh_semaphore is never closed");
            let result = tokio::task::spawn_blocking(move || {
                crate::gather::gather(&cfg).map_err(|e| format!("{e:#}"))
            })
            .await
            .unwrap_or_else(|_| Err("gather task panicked".to_string()));
            let _ = tx.send(AppMessage::GatherDone { host_idx, result });
        });
    }

    /// Trigger an apt task on one host. Does nothing if a task is already
    /// running. Queues behind `ssh_semaphore` so that triggering an action on
    /// a large group or "all" hosts doesn't open a connection per host
    /// simultaneously.
    pub fn start_task(&mut self, host_idx: usize, kind: TaskKind) {
        if matches!(
            self.hosts[host_idx].task.as_ref().map(|t| &t.status),
            Some(TaskStatus::Running)
        ) {
            return;
        }
        let cfg = self.hosts[host_idx].cfg.clone();
        let tx = self.tx.clone();
        let cmd = kind.command(cfg.use_sudo);
        self.hosts[host_idx].task = Some(TaskState::new(kind));
        let semaphore = self.ssh_semaphore.clone();
        tokio::spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("ssh_semaphore is never closed");
            let _ = tokio::task::spawn_blocking(move || {
                let sess = match crate::ssh::SshSession::connect(&cfg) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(AppMessage::TaskLine {
                            host_idx,
                            line: format!("Connection failed: {e:#}"),
                        });
                        let _ = tx.send(AppMessage::TaskFailed {
                            host_idx,
                            error: format!("{e:#}"),
                        });
                        return;
                    }
                };
                let tx_cb = tx.clone();
                let exit = sess.exec_streaming(&cmd, move |line| {
                    let _ = tx_cb.send(AppMessage::TaskLine { host_idx, line });
                });
                let exit_code = match exit {
                    Ok(code) => code,
                    Err(e) => {
                        let _ = tx.send(AppMessage::TaskLine {
                            host_idx,
                            line: format!("Error: {e:#}"),
                        });
                        -1
                    }
                };
                let _ = tx.send(AppMessage::TaskDone {
                    host_idx,
                    exit_code,
                });
            })
            .await;
        });
    }

    /// Trigger a security-only upgrade on one host, using whatever security
    /// packages were found in its last gather. Does nothing if there are
    /// none, or if a task is already running.
    pub fn start_security_upgrade(&mut self, host_idx: usize) {
        let pkgs = self.hosts[host_idx]
            .info
            .as_ref()
            .map(|i| i.security_package_names())
            .unwrap_or_default();
        if pkgs.is_empty() {
            return;
        }
        self.start_task(host_idx, TaskKind::UpgradeSecurity(pkgs));
    }

    /// Open the pending-conffile review modal for one host. Does nothing when
    /// the host has no pending files (or hasn't been gathered yet).
    pub fn open_conffile_review(&mut self, host_idx: usize) {
        let files = self.hosts[host_idx]
            .info
            .as_ref()
            .map(|i| i.pending_conffiles.clone())
            .unwrap_or_default();
        if files.is_empty() {
            return;
        }
        self.conffile_review = Some(ConffileReviewState {
            host_idx,
            decisions: vec![None; files.len()],
            files,
            selected: 0,
            diffs: std::collections::HashMap::new(),
            scroll: 0,
            confirm_execute: false,
            notice: None,
        });
        self.request_selected_diff();
    }

    /// Fetch the diff for the modal's current selection, unless it is already
    /// loading or loaded.
    fn request_selected_diff(&mut self) {
        let Some(state) = self.conffile_review.as_mut() else {
            return;
        };
        let Some(file) = state.files.get(state.selected).cloned() else {
            return;
        };
        if state.diffs.contains_key(&file.pending_path) {
            return;
        }
        state
            .diffs
            .insert(file.pending_path.clone(), DiffState::Loading);

        let host_idx = state.host_idx;
        let cfg = self.hosts[host_idx].cfg.clone();
        let tx = self.tx.clone();
        let semaphore = self.ssh_semaphore.clone();
        let sudo = if cfg.use_sudo { "sudo -n " } else { "" };
        let live = shell_quote(&file.live_path);
        let pending = shell_quote(&file.pending_path);
        // diff exits 1 when files differ, which is the normal case here, so the
        // exit status is not a useful error signal — `|| true` keeps the shell
        // from reporting it. The line cap stops a pathological file from
        // filling memory.
        let cmd = format!(
            "LC_ALL=C {sudo}diff -u --label {live} --label {pending} -- {live} {pending} 2>&1 | head -n 2000 || true"
        );
        let pending_path = file.pending_path.clone();
        tokio::spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("ssh_semaphore is never closed");
            let result = tokio::task::spawn_blocking(move || {
                let sess = crate::ssh::SshSession::connect(&cfg).map_err(|e| format!("{e:#}"))?;
                let out = sess.exec(&cmd).map_err(|e| format!("{e:#}"))?;
                Ok(out.lines().map(str::to_string).collect::<Vec<String>>())
            })
            .await
            .unwrap_or_else(|_| Err("diff task panicked".to_string()));
            let _ = tx.send(AppMessage::ConffileDiff {
                host_idx,
                pending_path,
                result,
            });
        });
    }

    /// Execute every marked decision as one task. The mutations run through
    /// the normal task machinery so their output lands in the task pane and
    /// the follow-up gather refreshes the pending count; the modal closes
    /// because only one task per host can run at a time.
    fn execute_conffile_decisions(&mut self) {
        let Some(state) = self.conffile_review.as_ref() else {
            return;
        };
        let host_idx = state.host_idx;
        let decisions = state.marked_decisions();
        if decisions.is_empty() {
            return;
        }
        if matches!(
            self.hosts[host_idx].task.as_ref().map(|t| &t.status),
            Some(TaskStatus::Running)
        ) {
            if let Some(state) = self.conffile_review.as_mut() {
                state.confirm_execute = false;
                state.notice = Some("a task is already running on this host".to_string());
            }
            return;
        }
        self.conffile_review = None;
        self.start_task(host_idx, TaskKind::ResolveConffiles(decisions));
    }

    pub fn handle_message(&mut self, msg: AppMessage) {
        match msg {
            AppMessage::GatherDone { host_idx, result } => match result {
                Ok(info) => {
                    let key = cache::host_key(&self.hosts[host_idx].cfg);
                    self.cache.insert(
                        key,
                        cache::CacheEntry {
                            info: info.clone(),
                            fetched_at_unix: cache::now_unix(),
                        },
                    );
                    let _ = cache::save(&self.cache);

                    let h = &mut self.hosts[host_idx];
                    h.info = Some(info);
                    h.status = HostStatus::Ready;
                    h.is_stale = false;
                }
                Err(e) => {
                    self.hosts[host_idx].status = HostStatus::Error(e);
                }
            },
            AppMessage::TaskLine { host_idx, line } => {
                if let Some(task) = self.hosts[host_idx].task.as_mut() {
                    task.push_line(line);
                }
            }
            AppMessage::TaskDone {
                host_idx,
                exit_code,
            } => {
                if let Some(task) = self.hosts[host_idx].task.as_mut() {
                    task.status = TaskStatus::Done(exit_code);
                    task.auto_scroll = true;
                }
                // Re-gather host info after task completes
                self.hosts[host_idx].status = HostStatus::Gathering;
                self.start_refresh(host_idx);
            }
            AppMessage::TaskFailed { host_idx, error } => {
                if let Some(task) = self.hosts[host_idx].task.as_mut() {
                    task.status = TaskStatus::Failed(error);
                    task.auto_scroll = true;
                }
            }
            AppMessage::ConffileDiff {
                host_idx,
                pending_path,
                result,
            } => {
                // Drop diffs that arrive after the modal closed or moved host.
                if let Some(state) = self.conffile_review.as_mut()
                    && state.host_idx == host_idx
                {
                    let entry = match result {
                        Ok(lines) => DiffState::Loaded(lines),
                        Err(e) => DiffState::Failed(e),
                    };
                    state.diffs.insert(pending_path, entry);
                }
            }
        }
    }

    /// Handle a key press. Returns `true` to keep the app running, or `false`
    /// to tell the main loop in `run()` to quit.
    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // ── Quit confirmation modal (highest priority) ──
        if self.quit_confirm {
            return self.handle_key_quit_confirm(code, modifiers);
        }

        // ── Reboot confirmation modal ──
        if self.reboot_confirm.is_some() {
            return self.handle_key_reboot_confirm(code, modifiers);
        }

        // ── Pending conffile review modal ──
        if self.conffile_review.is_some() {
            return self.handle_key_conffile_review(code, modifiers);
        }

        // ── Sidebar search/filter editing ──
        if self.filter_editing {
            return self.handle_key_filter_edit(code, modifiers);
        }

        // ── Task output overlay ──
        if let Some(host_idx) = self.viewing_task {
            return self.handle_key_task_view(host_idx, code);
        }

        match (code, modifiers) {
            // Navigate sidebar
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.move_selection(-1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.move_selection(1);
            }
            // Enter sidebar search/filter mode
            (KeyCode::Char('/'), _) => {
                self.filter_editing = true;
            }
            // apt-get install --only-upgrade (security packages only, selected)
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                for idx in self.selected_host_indices() {
                    self.start_security_upgrade(idx);
                }
            }
            // apt-get install --only-upgrade (security packages only, all)
            (KeyCode::Char('S'), _) => {
                for idx in 0..self.hosts.len() {
                    self.start_security_upgrade(idx);
                }
            }
            // apt-get update + refresh (selected)
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                for idx in self.selected_host_indices() {
                    self.start_task(idx, TaskKind::Update);
                }
            }
            // apt-get update + refresh (all)
            (KeyCode::Char('R'), _) => {
                for idx in 0..self.hosts.len() {
                    self.start_task(idx, TaskKind::Update);
                }
            }
            // Reboot — opens confirmation modal for single-host selection
            (KeyCode::Char('b'), _) if self.selected_host_indices().len() == 1 => {
                let idx = self.selected_host_indices()[0];
                self.reboot_confirm = Some(RebootConfirmState {
                    host_idx: idx,
                    input: String::new(),
                    mismatch: false,
                });
            }
            // apt-get upgrade (selected)
            (KeyCode::Char('u'), KeyModifiers::NONE) => {
                for idx in self.selected_host_indices() {
                    self.start_task(idx, TaskKind::Upgrade);
                }
            }
            // apt-get upgrade (all)
            (KeyCode::Char('U'), _) => {
                for idx in 0..self.hosts.len() {
                    self.start_task(idx, TaskKind::Upgrade);
                }
            }
            // apt-get full-upgrade (selected)
            (KeyCode::Char('f'), KeyModifiers::NONE) => {
                for idx in self.selected_host_indices() {
                    self.start_task(idx, TaskKind::FullUpgrade);
                }
            }
            // apt-get full-upgrade (all)
            (KeyCode::Char('F'), _) => {
                for idx in 0..self.hosts.len() {
                    self.start_task(idx, TaskKind::FullUpgrade);
                }
            }
            // apt-get autoremove --purge (selected)
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                for idx in self.selected_host_indices() {
                    self.start_task(idx, TaskKind::AutoRemove);
                }
            }
            // apt-get autoremove --purge (all)
            (KeyCode::Char('A'), _) => {
                for idx in 0..self.hosts.len() {
                    self.start_task(idx, TaskKind::AutoRemove);
                }
            }
            // Purge RC packages
            (KeyCode::Char('p'), KeyModifiers::NONE) => {
                for idx in self.selected_host_indices() {
                    self.start_task(idx, TaskKind::PurgeRc);
                }
            }
            // Review pending config files — single-host selection only, since
            // each file is resolved individually against its own diff.
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                if let [idx] = self.selected_host_indices()[..] {
                    self.open_conffile_review(idx);
                }
            }
            // View task output
            (KeyCode::Char('t'), _) | (KeyCode::Enter, _) => {
                // Find a host with an active or completed task in the selection
                for idx in self.selected_host_indices() {
                    if self.hosts[idx].task.is_some() {
                        self.viewing_task = Some(idx);
                        break;
                    }
                }
            }
            // Zoom detail panel (hide sidebar for clean text selection)
            (KeyCode::Char('z'), KeyModifiers::NONE) => {
                self.detail_zoom = !self.detail_zoom;
            }
            // Quit — if a task is running, confirm first since quitting drops the
            // SSH connection(s) and may interrupt the remote operation.
            (KeyCode::Char('q'), _)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Esc, _) => {
                if self.active_operation_count() > 0 {
                    self.quit_confirm = true;
                } else {
                    return false;
                }
            }
            _ => {}
        }
        true
    }

    /// Handles a key press while the quit-confirmation modal is open.
    /// 'y'/'Y' and Ctrl-C confirm quitting; any other key dismisses the modal.
    fn handle_key_quit_confirm(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return false,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return false,
            _ => self.quit_confirm = false,
        }
        true
    }

    fn handle_key_reboot_confirm(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        match (code, modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.reboot_confirm = None;
            }
            (KeyCode::Backspace, _) => {
                if let Some(state) = self.reboot_confirm.as_mut() {
                    state.input.pop();
                    state.mismatch = false;
                }
            }
            (KeyCode::Enter, _) => {
                // Extract what we need before mutating, to satisfy the borrow checker.
                let check = self.reboot_confirm.as_ref().map(|state| {
                    let expected = self.hosts[state.host_idx].cfg.hostname.clone();
                    (state.host_idx, state.input == expected)
                });
                if let Some((host_idx, matches)) = check {
                    if matches {
                        self.reboot_confirm = None;
                        self.start_task(host_idx, TaskKind::Reboot);
                    } else if let Some(state) = self.reboot_confirm.as_mut() {
                        state.mismatch = true;
                    }
                }
            }
            // Accept printable characters (but not control/alt combinations).
            (KeyCode::Char(c), m) if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                if let Some(state) = self.reboot_confirm.as_mut() {
                    state.input.push(c);
                    state.mismatch = false;
                }
            }
            _ => {}
        }
        true
    }

    /// Handles a key press while the pending-conffile review modal is open.
    /// `d`/`a` only mark files; nothing touches the host until Enter, and
    /// executing the batch takes a second keypress to confirm.
    fn handle_key_conffile_review(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // While the execute confirmation is pending, no other binding is
        // reachable.
        if self
            .conffile_review
            .as_ref()
            .is_some_and(|s| s.confirm_execute)
        {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.execute_conffile_decisions(),
                _ => {
                    if let Some(state) = self.conffile_review.as_mut() {
                        state.confirm_execute = false;
                    }
                }
            }
            return true;
        }

        let mut selection_moved = false;
        if let Some(state) = self.conffile_review.as_mut() {
            let diff_len = match state.selected_diff() {
                Some(DiffState::Loaded(lines)) => lines.len() as u16,
                _ => 0,
            };
            match (code, modifiers) {
                (KeyCode::Esc, _)
                | (KeyCode::Char('q'), _)
                | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    self.conffile_review = None;
                    return true;
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    state.selected = state.selected.saturating_sub(1);
                    selection_moved = true;
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    if state.selected + 1 < state.files.len() {
                        state.selected += 1;
                        selection_moved = true;
                    }
                }
                (KeyCode::PageUp, _) => state.scroll = state.scroll.saturating_sub(10),
                (KeyCode::PageDown, _) => {
                    state.scroll = (state.scroll + 10).min(diff_len.saturating_sub(1));
                }
                (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => state.scroll = 0,
                (KeyCode::Char('d'), KeyModifiers::NONE) => {
                    state.notice = None;
                    state.toggle_mark(ConffileAction::Discard);
                    // Advance to the next file, batch-triage style.
                    if state.selected + 1 < state.files.len() {
                        state.selected += 1;
                        selection_moved = true;
                    }
                }
                (KeyCode::Char('a'), KeyModifiers::NONE) => {
                    state.notice = None;
                    state.toggle_mark(ConffileAction::Apply);
                    if state.selected + 1 < state.files.len() {
                        state.selected += 1;
                        selection_moved = true;
                    }
                }
                (KeyCode::Char('u'), KeyModifiers::NONE) | (KeyCode::Char(' '), _) => {
                    if let Some(slot) = state.decisions.get_mut(state.selected) {
                        *slot = None;
                    }
                }
                (KeyCode::Enter, _) => {
                    if state.marked_decisions().is_empty() {
                        state.notice =
                            Some("nothing marked — d marks discard, a marks apply".to_string());
                    } else {
                        state.notice = None;
                        state.confirm_execute = true;
                    }
                }
                _ => {}
            }
            if selection_moved {
                state.scroll = 0;
                state.notice = None;
            }
        }
        if selection_moved {
            self.request_selected_diff();
        }
        true
    }

    /// Handles a key press while sidebar search/filter editing is active.
    /// Printable characters are appended to the filter; arrow keys navigate
    /// the (already filtered) sidebar without leaving edit mode.
    fn handle_key_filter_edit(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        match code {
            KeyCode::Esc | KeyCode::Enter => {
                self.filter_editing = false;
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.ensure_selection_visible();
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char(c)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.ensure_selection_visible();
            }
            _ => {}
        }
        true
    }

    fn handle_key_task_view(&mut self, host_idx: usize, code: KeyCode) -> bool {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.viewing_task = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(task) = self.hosts[host_idx].task.as_mut() {
                    task.auto_scroll = false;
                    task.scroll_offset = task.scroll_offset.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(task) = self.hosts[host_idx].task.as_mut() {
                    task.auto_scroll = false;
                    task.scroll_offset = task.scroll_offset.saturating_add(1);
                }
            }
            KeyCode::PageUp => {
                if let Some(task) = self.hosts[host_idx].task.as_mut() {
                    task.auto_scroll = false;
                    task.scroll_offset = task.scroll_offset.saturating_sub(20);
                }
            }
            KeyCode::PageDown => {
                if let Some(task) = self.hosts[host_idx].task.as_mut() {
                    task.auto_scroll = false;
                    task.scroll_offset = task.scroll_offset.saturating_add(20);
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if let Some(task) = self.hosts[host_idx].task.as_mut() {
                    task.auto_scroll = true;
                }
            }
            _ => {}
        }
        true
    }

    pub fn handle_mouse(&mut self, kind: MouseEventKind, col: u16) {
        // Block mouse interaction while the confirmation modal is visible.
        if self.reboot_confirm.is_some() {
            return;
        }
        match kind {
            MouseEventKind::Down(MouseButton::Left)
                if !self.detail_zoom && col == self.sidebar_width.saturating_sub(1) =>
            {
                self.dragging_sidebar = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_sidebar => {
                let max = crossterm::terminal::size()
                    .map(|(w, _)| w.saturating_sub(SIDEBAR_MIN_WIDTH))
                    .unwrap_or(SIDEBAR_MAX_WIDTH);
                self.sidebar_width = (col + 1).clamp(SIDEBAR_MIN_WIDTH, max);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging_sidebar = false;
            }
            _ => {}
        }
    }
}

// ── Process exit ─────────────────────────────────────────────────────────────

/// Terminates the process immediately using POSIX `_exit`, bypassing C atexit
/// handlers. This is necessary because libssh2 links OpenSSL, which registers
/// an atexit cleanup handler. If any SSH background thread is mid-operation
/// holding an OpenSSL lock when `exit()` runs, the atexit handler deadlocks —
/// the process hangs until something else (e.g. an external SIGINT) kills it.
/// `_exit` skips all that and terminates instantly.
fn exit_now(code: i32) -> ! {
    #[cfg(unix)]
    unsafe {
        unsafe extern "C" {
            fn _exit(status: i32) -> !;
        }
        _exit(code);
    }
    #[cfg(not(unix))]
    std::process::exit(code)
}

// ── Terminal guard ────────────────────────────────────────────────────────────

/// Undo the terminal setup done at the start of `run()`: leave the alternate
/// screen, turn off mouse capture, and restore normal (non-raw) input mode.
/// Without this, the user's shell would be left in a broken state (no visible
/// input, alternate screen still active) after the program exits.
fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Ensures the terminal is restored even if `run()` exits early via `?`
/// (e.g. an error) or a panic.
///
/// This is the RAII pattern: `TerminalGuard` itself holds no data, but as
/// soon as it goes out of scope, Rust calls its `Drop` impl below — on every
/// exit path, including early returns and panics (but *not* on
/// `std::process::exit`, which is why the deliberate-quit path in `run()`
/// calls `restore_terminal()` directly before exiting).
pub struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

// ── Main event loop ───────────────────────────────────────────────────────────

pub async fn run(mut app: App, mut rx: UnboundedReceiver<AppMessage>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // Kick off initial gather for all hosts
    for idx in 0..app.hosts.len() {
        app.hosts[idx].status = HostStatus::Connecting;
        app.start_refresh(idx);
    }

    let mut events = EventStream::new();
    let mut tick_interval = interval(Duration::from_millis(100));

    // Restore the terminal and exit cleanly if SIGTERM arrives (e.g. `kill <pid>`).
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            let _ = s.recv().await;
            restore_terminal();
            exit_now(143);
        }
    });

    loop {
        if terminal.draw(|f| crate::ui::render(f, &mut app)).is_err() {
            restore_terminal();
            exit_now(1);
        }

        tokio::select! {
            _ = tick_interval.tick() => {
                app.tick = app.tick.wrapping_add(1);
            }
            Some(msg) = rx.recv() => {
                app.handle_message(msg);
            }
            Some(Ok(event)) = events.next() => {
                match event {
                    Event::Key(key)
                        if key.kind == KeyEventKind::Press
                            && !app.handle_key(key.code, key.modifiers) =>
                    {
                        // `handle_key` returned `false`, meaning the user confirmed they
                        // want to quit. We restore the terminal ourselves and call
                        // `std::process::exit(0)` instead of just returning `Ok(())`.
                        //
                        // Why not just return? Each running apt task is a background
                        // thread (spawned via `tokio::task::spawn_blocking` in
                        // `start_task`/`start_refresh`) doing blocking SSH network I/O.
                        // If we returned normally, `main`'s `#[tokio::main]` runtime would
                        // be dropped, and Tokio's `Drop for Runtime` waits for *all* such
                        // threads to finish before the process can exit — which could take
                        // as long as the remote apt command does. `process::exit(0)` ends
                        // the whole process immediately, taking every thread down with it,
                        // so quitting feels instant.
                        //
                        // `process::exit` skips Rust's normal `Drop` cleanup (so
                        // `TerminalGuard`'s `Drop` impl below won't run), which is why we
                        // call `restore_terminal()` explicitly first.
                        restore_terminal();
                        exit_now(0);
                    }
                    Event::Mouse(mouse) => {
                        app.handle_mouse(mouse.kind, mouse.column);
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, RawConfig, RawGroup, RawHost};

    fn make_app(raw: RawConfig) -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config { raw };
        App::new(&config, tx)
    }

    fn raw_host(hostname: &str) -> RawHost {
        RawHost {
            hostname: hostname.to_string(),
            user: Some("alice".to_string()),
            port: None,
            use_sudo: None,
            identity_file: None,
        }
    }

    // ── TaskKind::command ─────────────────────────────────────────────────────

    #[test]
    fn task_kind_command_update_with_sudo() {
        let cmd = TaskKind::Update.command(true);
        assert!(cmd.contains("sudo -n"));
        assert!(cmd.contains("apt-get update"));
    }

    #[test]
    fn task_kind_command_update_without_sudo() {
        let cmd = TaskKind::Update.command(false);
        assert!(!cmd.contains("sudo"));
        assert!(cmd.contains("apt-get update"));
    }

    #[test]
    fn task_kind_command_upgrade_with_sudo() {
        let cmd = TaskKind::Upgrade.command(true);
        assert!(cmd.contains("sudo -n"));
        assert!(cmd.contains("apt-get") && cmd.contains("upgrade"));
    }

    #[test]
    fn task_kind_command_upgrade_without_sudo() {
        let cmd = TaskKind::Upgrade.command(false);
        assert!(!cmd.contains("sudo"));
        assert!(cmd.contains("apt-get") && cmd.contains("upgrade"));
    }

    #[test]
    fn task_kind_full_upgrade_label() {
        assert_eq!(TaskKind::FullUpgrade.label(), "apt-get full-upgrade");
    }

    #[test]
    fn task_kind_command_full_upgrade_with_sudo() {
        let cmd = TaskKind::FullUpgrade.command(true);
        assert!(cmd.contains("sudo -n"));
        assert!(cmd.contains("apt-get") && cmd.contains("full-upgrade"));
    }

    #[test]
    fn task_kind_command_full_upgrade_without_sudo() {
        let cmd = TaskKind::FullUpgrade.command(false);
        assert!(!cmd.contains("sudo"));
        assert!(cmd.contains("apt-get") && cmd.contains("full-upgrade"));
    }

    #[test]
    fn task_kind_command_full_upgrade_is_noninteractive() {
        let cmd = TaskKind::FullUpgrade.command(false);
        assert!(cmd.contains("DEBIAN_FRONTEND=noninteractive"));
    }

    #[test]
    fn task_kind_command_full_upgrade_not_same_as_upgrade() {
        let upgrade = TaskKind::Upgrade.command(false);
        let full_upgrade = TaskKind::FullUpgrade.command(false);
        assert_ne!(upgrade, full_upgrade);
    }

    #[test]
    fn task_kind_autoremove_label() {
        assert_eq!(TaskKind::AutoRemove.label(), "apt-get autoremove --purge");
    }

    #[test]
    fn task_kind_command_autoremove_with_sudo() {
        let cmd = TaskKind::AutoRemove.command(true);
        assert!(cmd.contains("sudo -n"));
        assert!(cmd.contains("autoremove") && cmd.contains("--purge"));
    }

    #[test]
    fn task_kind_command_autoremove_without_sudo() {
        let cmd = TaskKind::AutoRemove.command(false);
        assert!(!cmd.contains("sudo"));
        assert!(cmd.contains("autoremove") && cmd.contains("--purge"));
    }

    #[test]
    fn task_kind_command_autoremove_is_noninteractive() {
        let cmd = TaskKind::AutoRemove.command(false);
        assert!(cmd.contains("DEBIAN_FRONTEND=noninteractive"));
    }

    /// A changed conffile makes dpkg prompt on stdin, which hangs the task
    /// forever over SSH. Every dpkg-invoking command must defuse that.
    #[test]
    fn dpkg_invoking_commands_never_prompt_on_conffiles() {
        let cmds = [
            TaskKind::Upgrade.command(false),
            TaskKind::UpgradeSecurity(vec!["openssh-server".to_string()]).command(false),
            TaskKind::FullUpgrade.command(false),
            TaskKind::AutoRemove.command(false),
        ];
        for cmd in cmds {
            assert!(cmd.contains("--force-confdef"), "missing confdef: {cmd}");
            assert!(cmd.contains("--force-confold"), "missing confold: {cmd}");
            assert!(cmd.contains("</dev/null"), "stdin still open: {cmd}");
        }
    }

    // ── Pending conffiles ─────────────────────────────────────────────────────

    fn pending_conffile(live: &str) -> PendingConffile {
        PendingConffile {
            pending_path: format!("{live}.dpkg-dist"),
            live_path: live.to_string(),
            package: None,
        }
    }

    fn discard(live: &str) -> ConffileDecision {
        ConffileDecision {
            file: pending_conffile(live),
            action: ConffileAction::Discard,
        }
    }

    fn apply(live: &str) -> ConffileDecision {
        ConffileDecision {
            file: pending_conffile(live),
            action: ConffileAction::Apply,
        }
    }

    #[test]
    fn resolve_discard_removes_only_the_pending_file() {
        let cmd = TaskKind::ResolveConffiles(vec![discard("/etc/ssh/sshd_config")]).command(true);
        assert!(cmd.contains("sudo -n rm -f -- '/etc/ssh/sshd_config.dpkg-dist'"));
        // The live config must not appear as an argument to rm.
        assert!(!cmd.contains("rm -f -- '/etc/ssh/sshd_config'"));
    }

    #[test]
    fn resolve_apply_backs_up_before_replacing() {
        let cmd = TaskKind::ResolveConffiles(vec![apply("/etc/ssh/sshd_config")]).command(true);
        let backup = cmd
            .find("cp -a --")
            .expect("apply must copy the live file first");
        let install = cmd.find("mv --").expect("apply must move the new file in");
        assert!(backup < install, "backup has to happen before the move");
        assert!(cmd.contains("'/etc/ssh/sshd_config.dpkg-old'"));
        // `set -e` inside the subshell is what makes a failed backup abort
        // the move for that file.
        assert!(cmd.contains("( set -e;"));
    }

    /// One failed file must not stop the rest of the batch, but must still
    /// fail the task overall.
    #[test]
    fn resolve_batch_isolates_failures_and_reports_them() {
        let cmd = TaskKind::ResolveConffiles(vec![
            apply("/etc/a.conf"),
            discard("/etc/b.conf"),
            apply("/etc/c.conf"),
        ])
        .command(true);
        // Every decision appears, in order.
        let a = cmd.find("'/etc/a.conf'").expect("a missing");
        let b = cmd.find("'/etc/b.conf.dpkg-dist'").expect("b missing");
        let c = cmd.find("'/etc/c.conf'").expect("c missing");
        assert!(a < b && b < c, "decisions out of order");
        // Failure handling: each step records rather than aborts, and the
        // batch exits non-zero if anything failed.
        assert_eq!(cmd.matches("fail=1").count(), 3);
        assert!(cmd.contains("exit $fail"));
        // A bare `set -e` outside the per-file subshells would abort the
        // whole batch on the first failure.
        assert!(!cmd.contains("{ set -e"));
    }

    /// Paths come from `find` on the remote host, so a filename containing a
    /// quote must not be able to break out into a second command.
    #[test]
    fn resolve_commands_quote_hostile_paths() {
        let nasty = PendingConffile {
            pending_path: "/etc/x'; rm -rf /; '.dpkg-dist".to_string(),
            live_path: "/etc/x'; rm -rf /; '".to_string(),
            package: None,
        };
        for action in [ConffileAction::Discard, ConffileAction::Apply] {
            let cmd = TaskKind::ResolveConffiles(vec![ConffileDecision {
                file: nasty.clone(),
                action,
            }])
            .command(false);
            assert!(
                !cmd.contains("; rm -rf /; '.dpkg-dist'"),
                "path escaped quoting: {cmd}"
            );
            assert!(cmd.contains(r#"'\''"#), "expected escaped quotes in {cmd}");
            // Every occurrence of the path — including inside progress echoes
            // — has to be quoted, so the raw form must never appear.
            assert!(
                !cmd.contains("/etc/x'; rm"),
                "unquoted path reached the command: {cmd}"
            );
        }
    }

    #[test]
    fn resolve_label_counts_each_action() {
        let kind = TaskKind::ResolveConffiles(vec![
            discard("/etc/a.conf"),
            apply("/etc/b.conf"),
            discard("/etc/c.conf"),
        ]);
        assert_eq!(kind.label(), "resolve config files (2 discard, 1 apply)");
    }

    fn app_with_pending_conffiles(files: Vec<PendingConffile>) -> App {
        let mut app = make_app(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            // Opening the modal kicks off a real diff fetch in the background.
            // `.invalid` is guaranteed never to resolve (RFC 6761), so that
            // connection fails immediately instead of stalling runtime
            // shutdown on a DNS timeout.
            hosts: vec![raw_host("web01.invalid")],
            ..Default::default()
        });
        app.hosts[0].info = Some(HostInfo {
            pending_conffiles: files,
            ..Default::default()
        });
        app.hosts[0].status = HostStatus::Ready;
        app
    }

    #[tokio::test]
    async fn c_opens_review_modal_when_files_are_pending() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/hosts")]);
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(app.conffile_review.is_some());
    }

    /// `c` opening the review modal must not shadow Ctrl-C quitting.
    #[tokio::test]
    async fn ctrl_c_still_quits_rather_than_opening_the_modal() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/hosts")]);
        let keep_running = app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!keep_running);
        assert!(app.conffile_review.is_none());
    }

    #[tokio::test]
    async fn c_does_nothing_when_nothing_is_pending() {
        let mut app = app_with_pending_conffiles(vec![]);
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(app.conffile_review.is_none());
    }

    #[tokio::test]
    async fn review_modal_navigates_between_files() {
        let mut app = app_with_pending_conffiles(vec![
            pending_conffile("/etc/a.conf"),
            pending_conffile("/etc/b.conf"),
        ]);
        app.open_conffile_review(0);
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.conffile_review.as_ref().unwrap().selected, 1);
        // Selection stops at the end rather than wrapping onto a missing file.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.conffile_review.as_ref().unwrap().selected, 1);
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.conffile_review.as_ref().unwrap().selected, 0);
    }

    /// `d`/`a` only mark; nothing may touch the host until the batch is
    /// confirmed with Enter + y.
    #[tokio::test]
    async fn marking_never_starts_a_task() {
        let mut app = app_with_pending_conffiles(vec![
            pending_conffile("/etc/a.conf"),
            pending_conffile("/etc/b.conf"),
        ]);
        app.open_conffile_review(0);

        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);

        let state = app.conffile_review.as_ref().unwrap();
        assert_eq!(state.decisions[0], Some(ConffileAction::Discard));
        assert_eq!(state.decisions[1], Some(ConffileAction::Apply));
        assert!(app.hosts[0].task.is_none(), "marking must not act");
    }

    /// Marking auto-advances so a batch can be triaged with repeated
    /// keypresses.
    #[tokio::test]
    async fn marking_advances_to_the_next_file() {
        let mut app = app_with_pending_conffiles(vec![
            pending_conffile("/etc/a.conf"),
            pending_conffile("/etc/b.conf"),
        ]);
        app.open_conffile_review(0);
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(app.conffile_review.as_ref().unwrap().selected, 1);
    }

    #[tokio::test]
    async fn marking_same_action_again_unmarks() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/a.conf")]);
        app.open_conffile_review(0);
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(app.conffile_review.as_ref().unwrap().decisions[0], None);
    }

    #[tokio::test]
    async fn unmark_clears_a_decision() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/a.conf")]);
        app.open_conffile_review(0);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(app.conffile_review.as_ref().unwrap().decisions[0], None);
    }

    /// Executing mutates live configs, so a single stray Enter must not be
    /// enough to do it.
    #[tokio::test]
    async fn execute_requires_confirmation() {
        let mut app = app_with_pending_conffiles(vec![
            pending_conffile("/etc/a.conf"),
            pending_conffile("/etc/b.conf"),
        ]);
        app.open_conffile_review(0);
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.conffile_review.as_ref().unwrap().confirm_execute);
        assert!(app.hosts[0].task.is_none(), "no task before confirming");

        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(!app.conffile_review.as_ref().unwrap().confirm_execute);
        assert!(app.hosts[0].task.is_none(), "cancelling must not act");

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.conffile_review.is_none(), "modal closes once executed");
        match app.hosts[0].task.as_ref().map(|t| &t.kind) {
            Some(TaskKind::ResolveConffiles(decisions)) => {
                assert_eq!(decisions.len(), 2);
                assert_eq!(decisions[0].action, ConffileAction::Discard);
                assert_eq!(decisions[1].action, ConffileAction::Apply);
            }
            other => panic!("expected ResolveConffiles task, got {other:?}"),
        }
    }

    /// Undecided files are simply left out of the batch — they stay pending
    /// for a later review.
    #[tokio::test]
    async fn execute_skips_unmarked_files() {
        let mut app = app_with_pending_conffiles(vec![
            pending_conffile("/etc/a.conf"),
            pending_conffile("/etc/b.conf"),
        ]);
        app.open_conffile_review(0);
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        match app.hosts[0].task.as_ref().map(|t| &t.kind) {
            Some(TaskKind::ResolveConffiles(decisions)) => {
                assert_eq!(decisions.len(), 1);
                assert_eq!(decisions[0].file.live_path, "/etc/a.conf");
            }
            other => panic!("expected ResolveConffiles task, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enter_with_nothing_marked_shows_a_notice() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/a.conf")]);
        app.open_conffile_review(0);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let state = app.conffile_review.as_ref().unwrap();
        assert!(!state.confirm_execute);
        assert!(state.notice.is_some());
    }

    #[tokio::test]
    async fn esc_closes_review_modal_without_acting() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/hosts")]);
        app.open_conffile_review(0);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.conffile_review.is_none());
        assert!(app.hosts[0].task.is_none());
    }

    /// Only one task per host can run, so a batch executed while one is in
    /// flight would be silently dropped — the modal says so instead, keeping
    /// the marks so nothing has to be re-triaged.
    #[tokio::test]
    async fn review_refuses_to_execute_while_a_task_runs() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/hosts")]);
        app.open_conffile_review(0);
        app.hosts[0].task = Some(TaskState::new(TaskKind::Update));

        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        let state = app
            .conffile_review
            .as_ref()
            .expect("modal stays open on refusal");
        assert!(state.notice.is_some());
        assert_eq!(
            state.decisions[0],
            Some(ConffileAction::Discard),
            "marks survive the refusal"
        );
        assert!(matches!(
            app.hosts[0].task.as_ref().map(|t| &t.kind),
            Some(TaskKind::Update)
        ));
    }

    #[tokio::test]
    async fn diff_for_a_closed_modal_is_discarded() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/hosts")]);
        app.handle_message(AppMessage::ConffileDiff {
            host_idx: 0,
            pending_path: "/etc/hosts.dpkg-dist".to_string(),
            result: Ok(vec!["-a".to_string(), "+b".to_string()]),
        });
        assert!(app.conffile_review.is_none());
    }

    #[tokio::test]
    async fn diff_result_lands_on_the_open_modal() {
        let mut app = app_with_pending_conffiles(vec![pending_conffile("/etc/hosts")]);
        app.open_conffile_review(0);
        app.handle_message(AppMessage::ConffileDiff {
            host_idx: 0,
            pending_path: "/etc/hosts.dpkg-dist".to_string(),
            result: Ok(vec!["-a".to_string(), "+b".to_string()]),
        });
        let state = app.conffile_review.as_ref().unwrap();
        assert!(matches!(state.selected_diff(), Some(DiffState::Loaded(l)) if l.len() == 2));
    }

    #[test]
    fn task_kind_command_purge_rc_with_sudo() {
        let cmd = TaskKind::PurgeRc.command(true);
        assert!(cmd.contains("sudo -n"));
        assert!(cmd.contains("dpkg --purge"));
    }

    #[test]
    fn task_kind_command_purge_rc_without_sudo() {
        let cmd = TaskKind::PurgeRc.command(false);
        assert!(!cmd.contains("sudo"));
        assert!(cmd.contains("dpkg --purge"));
    }

    // ── handle_mouse ──────────────────────────────────────────────────────────

    fn bare_app() -> App {
        make_app(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            hosts: vec![raw_host("h1.example.com")],
            ..Default::default()
        })
    }

    #[test]
    fn handle_mouse_down_on_border_starts_drag() {
        let mut app = bare_app();
        app.sidebar_width = 28;
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 27);
        assert!(app.dragging_sidebar);
    }

    #[test]
    fn handle_mouse_down_off_border_does_not_start_drag() {
        let mut app = bare_app();
        app.sidebar_width = 28;
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 10);
        assert!(!app.dragging_sidebar);
    }

    #[test]
    fn handle_mouse_drag_updates_sidebar_width() {
        let mut app = bare_app();
        app.dragging_sidebar = true;
        app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 39);
        assert_eq!(app.sidebar_width, 40);
    }

    #[test]
    fn handle_mouse_drag_clamps_to_minimum() {
        let mut app = bare_app();
        app.dragging_sidebar = true;
        app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 0);
        assert_eq!(app.sidebar_width, SIDEBAR_MIN_WIDTH);
    }

    #[test]
    fn handle_mouse_drag_clamps_to_maximum() {
        let mut app = bare_app();
        app.dragging_sidebar = true;
        app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 200);
        let expected_max = crossterm::terminal::size()
            .map(|(w, _)| w.saturating_sub(SIDEBAR_MIN_WIDTH))
            .unwrap_or(SIDEBAR_MAX_WIDTH);
        assert_eq!(app.sidebar_width, expected_max);
    }

    #[test]
    fn handle_mouse_up_stops_drag() {
        let mut app = bare_app();
        app.dragging_sidebar = true;
        app.handle_mouse(MouseEventKind::Up(MouseButton::Left), 0);
        assert!(!app.dragging_sidebar);
    }

    #[test]
    fn handle_mouse_down_on_border_blocked_when_detail_zoom() {
        let mut app = bare_app();
        app.sidebar_width = 28;
        app.detail_zoom = true;
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 27);
        assert!(!app.dragging_sidebar);
    }

    // ── selected_host_indices ─────────────────────────────────────────────────

    #[test]
    fn selected_host_indices_single_host() {
        let app = make_app(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            hosts: vec![raw_host("h1.example.com")],
            ..Default::default()
        });
        // sidebar_rows = [Host{0}]; selected_row defaults to 0
        assert_eq!(app.selected_host_indices(), vec![0]);
    }

    #[test]
    fn selected_host_indices_group_returns_all_children() {
        let app = make_app(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            groups: vec![RawGroup {
                name: "web".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: None,
                hosts: vec![raw_host("web1.example.com"), raw_host("web2.example.com")],
            }],
            ..Default::default()
        });
        // sidebar_rows = [Group, Host{0}, Host{1}]; selected_row=0 => Group
        assert_eq!(app.selected_host_indices(), vec![0, 1]);
    }

    #[test]
    fn selected_host_indices_duplicate_group_names_are_independent() {
        // Two groups with the same name — selecting the first should only return its hosts
        let app = make_app(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            groups: vec![
                RawGroup {
                    name: "web".to_string(),
                    user: None,
                    port: None,
                    use_sudo: None,
                    identity_file: None,
                    hosts: vec![raw_host("web1.example.com")],
                },
                RawGroup {
                    name: "web".to_string(),
                    user: None,
                    port: None,
                    use_sudo: None,
                    identity_file: None,
                    hosts: vec![raw_host("web2.example.com")],
                },
            ],
            ..Default::default()
        });
        // rows: [Group"web", Host{0}, Group"web", Host{1}]
        // selecting row 0 (first "web" group) should only return [0]
        assert_eq!(app.selected_host_indices(), vec![0]);
    }

    #[test]
    fn selected_host_indices_empty_group_returns_empty() {
        let mut app = make_app(RawConfig {
            groups: vec![RawGroup {
                name: "empty".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: None,
                hosts: vec![],
            }],
            ..Default::default()
        });
        app.selected_row = 0; // Group "empty"
        assert_eq!(app.selected_host_indices(), Vec::<usize>::new());
    }

    #[test]
    fn selected_host_indices_group_excludes_trailing_ungrouped_hosts() {
        // A group followed directly by top-level (ungrouped) hosts, with no
        // further group in between — sidebar_rows has no marker separating
        // them, so the ungrouped host must not leak into the group's selection.
        let app = make_app(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            groups: vec![RawGroup {
                name: "web".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: None,
                hosts: vec![raw_host("web1.example.com")],
            }],
            hosts: vec![raw_host("db1.example.com")],
        });
        // sidebar_rows = [Group"web", Host{0=web1}, Host{1=db1}]; selecting the group
        assert_eq!(app.selected_host_indices(), vec![0]);
    }

    #[test]
    fn selected_host_indices_out_of_bounds_returns_empty() {
        let mut app = bare_app();
        app.selected_row = 999;
        assert_eq!(app.selected_host_indices(), Vec::<usize>::new());
    }

    // ── Quit confirmation modal ───────────────────────────────────────────────

    #[test]
    fn q_with_no_running_tasks_quits_immediately() {
        let mut app = bare_app();
        assert!(!app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.quit_confirm);
    }

    #[test]
    fn esc_with_no_running_tasks_quits_immediately() {
        let mut app = bare_app();
        assert!(!app.handle_key(KeyCode::Esc, KeyModifiers::NONE));
    }

    #[test]
    fn ctrl_c_with_no_running_tasks_quits_immediately() {
        let mut app = bare_app();
        assert!(!app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    #[test]
    fn q_with_running_task_opens_quit_confirm_modal() {
        let mut app = bare_app();
        app.hosts[0].task = Some(TaskState::new(TaskKind::Upgrade));
        assert!(app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(app.quit_confirm);
    }

    #[test]
    fn quit_confirm_y_confirms_quit() {
        let mut app = bare_app();
        app.hosts[0].task = Some(TaskState::new(TaskKind::Upgrade));
        app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE));
    }

    #[test]
    fn quit_confirm_other_key_cancels() {
        let mut app = bare_app();
        app.hosts[0].task = Some(TaskState::new(TaskKind::Upgrade));
        app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.handle_key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.quit_confirm);
    }

    #[test]
    fn running_task_count_counts_only_running_tasks() {
        let mut app = bare_app();
        assert_eq!(app.running_task_count(), 0);
        app.hosts[0].task = Some(TaskState::new(TaskKind::Upgrade));
        assert_eq!(app.running_task_count(), 1);
        app.hosts[0].task.as_mut().unwrap().status = TaskStatus::Done(0);
        assert_eq!(app.running_task_count(), 0);
    }

    #[test]
    fn active_operation_count_includes_gather_states() {
        let mut app = bare_app();
        assert_eq!(app.active_operation_count(), 0);
        app.hosts[0].status = HostStatus::Connecting;
        assert_eq!(app.active_operation_count(), 1);
        app.hosts[0].status = HostStatus::Gathering;
        assert_eq!(app.active_operation_count(), 1);
        app.hosts[0].status = HostStatus::Ready;
        assert_eq!(app.active_operation_count(), 0);
    }

    #[test]
    fn q_with_mid_refresh_host_opens_quit_confirm_modal() {
        let mut app = bare_app();
        app.hosts[0].status = HostStatus::Connecting;
        assert!(app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(app.quit_confirm);
    }

    // ── Reboot confirmation modal ─────────────────────────────────────────────

    fn one_host_app() -> App {
        make_app(RawConfig {
            hosts: vec![raw_host("myserver")],
            ..Default::default()
        })
    }

    #[test]
    fn b_key_opens_reboot_modal() {
        let mut app = one_host_app();
        app.selected_row = 0;
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        let state = app.reboot_confirm.as_ref().expect("modal should be open");
        assert_eq!(state.host_idx, 0);
        assert!(state.input.is_empty());
        assert!(!state.mismatch);
    }

    #[test]
    fn esc_closes_reboot_modal() {
        let mut app = one_host_app();
        app.selected_row = 0;
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(app.reboot_confirm.is_some());
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.reboot_confirm.is_none());
    }

    #[test]
    fn ctrl_c_closes_reboot_modal() {
        let mut app = one_host_app();
        app.selected_row = 0;
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(app.reboot_confirm.is_some());
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.reboot_confirm.is_none());
    }

    #[test]
    fn ctrl_c_quits_even_when_quit_confirm_modal_is_open() {
        let mut app = bare_app();
        app.hosts[0].task = Some(TaskState::new(TaskKind::Upgrade));
        // 'q' shows the modal (task running)
        app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.quit_confirm);
        // Ctrl-C quits immediately without requiring 'y'
        assert!(!app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    #[test]
    fn typing_in_modal_updates_input() {
        let mut app = one_host_app();
        app.selected_row = 0;
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('m'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        let state = app.reboot_confirm.as_ref().unwrap();
        assert_eq!(state.input, "my");
    }

    #[test]
    fn backspace_removes_char_and_clears_mismatch() {
        let mut app = one_host_app();
        app.selected_row = 0;
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        // Force mismatch state
        app.reboot_confirm.as_mut().unwrap().mismatch = true;
        app.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        let state = app.reboot_confirm.as_ref().unwrap();
        assert!(state.input.is_empty());
        assert!(!state.mismatch);
    }

    #[test]
    fn enter_with_wrong_hostname_sets_mismatch_keeps_modal_open() {
        let mut app = one_host_app();
        app.selected_row = 0;
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('w'), KeyModifiers::NONE); // "w" != "myserver"
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let state = app
            .reboot_confirm
            .as_ref()
            .expect("modal should remain open");
        assert!(state.mismatch);
    }

    #[tokio::test]
    async fn enter_with_correct_hostname_closes_modal() {
        let mut app = one_host_app();
        app.selected_row = 0;
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        for c in "myserver".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.reboot_confirm.is_none());
    }

    #[test]
    fn handle_mouse_blocked_when_modal_active() {
        let mut app = one_host_app();
        app.selected_row = 0;
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        let original_width = app.sidebar_width;
        // Simulate a drag that would normally change sidebar_width.
        app.dragging_sidebar = true;
        app.handle_mouse(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            50,
        );
        assert_eq!(
            app.sidebar_width, original_width,
            "mouse drag should be blocked by modal"
        );
    }

    // ── Security-only upgrade ────────────────────────────────────────────────

    fn security_pkg(name: &str) -> crate::apt::Package {
        crate::apt::Package {
            name: name.to_string(),
            new_version: "2.0".to_string(),
            current_version: Some("1.0".to_string()),
            is_security: true,
        }
    }

    fn non_security_pkg(name: &str) -> crate::apt::Package {
        crate::apt::Package {
            name: name.to_string(),
            new_version: "2.0".to_string(),
            current_version: Some("1.0".to_string()),
            is_security: false,
        }
    }

    #[test]
    fn task_kind_command_upgrade_security_with_sudo() {
        let cmd = TaskKind::UpgradeSecurity(vec!["curl".to_string(), "openssl".to_string()])
            .command(true);
        assert!(cmd.contains("sudo -n"));
        assert!(cmd.contains("install --only-upgrade"));
        assert!(cmd.contains("curl openssl"));
    }

    #[test]
    fn task_kind_command_upgrade_security_without_sudo() {
        let cmd = TaskKind::UpgradeSecurity(vec!["curl".to_string()]).command(false);
        assert!(!cmd.contains("sudo"));
        assert!(cmd.contains("curl"));
    }

    #[test]
    fn task_kind_label_upgrade_security_includes_count() {
        let label =
            TaskKind::UpgradeSecurity(vec!["curl".to_string(), "openssl".to_string()]).label();
        assert!(label.contains("2 pkg"));
    }

    #[test]
    fn start_security_upgrade_does_nothing_without_security_packages() {
        let mut app = one_host_app();
        app.hosts[0].info = Some(HostInfo {
            upgradable: vec![non_security_pkg("vim")],
            ..Default::default()
        });
        app.start_security_upgrade(0);
        assert!(app.hosts[0].task.is_none());
    }

    #[test]
    fn start_security_upgrade_does_nothing_without_gathered_info() {
        let mut app = one_host_app();
        app.start_security_upgrade(0);
        assert!(app.hosts[0].task.is_none());
    }

    #[tokio::test]
    async fn start_security_upgrade_triggers_task_with_security_packages_only() {
        let mut app = one_host_app();
        app.hosts[0].info = Some(HostInfo {
            upgradable: vec![security_pkg("openssl"), non_security_pkg("vim")],
            ..Default::default()
        });
        app.start_security_upgrade(0);
        match &app.hosts[0].task.as_ref().unwrap().kind {
            TaskKind::UpgradeSecurity(pkgs) => assert_eq!(pkgs, &vec!["openssl".to_string()]),
            other => panic!("expected UpgradeSecurity task, got {other:?}"),
        }
    }

    // ── Sidebar search/filter ─────────────────────────────────────────────────

    fn grouped_app() -> App {
        make_app(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            groups: vec![RawGroup {
                name: "webservers".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: None,
                hosts: vec![raw_host("web1.example.com"), raw_host("web2.example.com")],
            }],
            hosts: vec![raw_host("db1.example.com")],
        })
    }

    #[test]
    fn filtered_row_indices_empty_filter_returns_everything() {
        let app = grouped_app();
        // sidebar_rows = [Group, Host(web1), Host(web2), Host(db1)]
        assert_eq!(app.filtered_row_indices(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn filtered_row_indices_matches_host_includes_parent_group() {
        let mut app = grouped_app();
        app.filter = "web1".to_string();
        assert_eq!(app.filtered_row_indices(), vec![0, 1]);
    }

    #[test]
    fn filtered_row_indices_matches_group_name_includes_all_children() {
        let mut app = grouped_app();
        app.filter = "webservers".to_string();
        assert_eq!(app.filtered_row_indices(), vec![0, 1, 2]);
    }

    #[test]
    fn filtered_row_indices_matches_ungrouped_host() {
        let mut app = grouped_app();
        app.filter = "db1".to_string();
        assert_eq!(app.filtered_row_indices(), vec![3]);
    }

    #[test]
    fn filtered_row_indices_no_match_returns_empty() {
        let mut app = grouped_app();
        app.filter = "nonexistent".to_string();
        assert!(app.filtered_row_indices().is_empty());
    }

    #[test]
    fn filtered_row_indices_is_case_insensitive() {
        let mut app = grouped_app();
        app.filter = "WEB1".to_string();
        assert_eq!(app.filtered_row_indices(), vec![0, 1]);
    }

    #[test]
    fn move_selection_skips_hidden_rows() {
        let mut app = grouped_app();
        app.filter = "web".to_string(); // matches group name -> [0, 1, 2]
        app.selected_row = 0;
        app.move_selection(1);
        assert_eq!(app.selected_row, 1);
        app.move_selection(1);
        assert_eq!(app.selected_row, 2);
        // Clamped at the end of the filtered set
        app.move_selection(1);
        assert_eq!(app.selected_row, 2);
    }

    #[test]
    fn move_selection_does_nothing_when_filter_matches_nothing() {
        let mut app = grouped_app();
        app.selected_row = 1;
        app.filter = "nonexistent".to_string();
        app.move_selection(1);
        assert_eq!(app.selected_row, 1);
    }

    #[test]
    fn slash_key_enters_filter_editing_mode() {
        let mut app = grouped_app();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(app.filter_editing);
    }

    #[test]
    fn typing_while_filtering_appends_to_filter() {
        let mut app = grouped_app();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('w'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(app.filter, "w1");
    }

    #[test]
    fn backspace_while_filtering_removes_last_char() {
        let mut app = grouped_app();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('w'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.filter, "");
    }

    #[test]
    fn enter_exits_filter_editing_and_keeps_filter() {
        let mut app = grouped_app();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('w'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.filter_editing);
        assert_eq!(app.filter, "w");
    }

    #[test]
    fn esc_exits_filter_editing_and_keeps_filter() {
        let mut app = grouped_app();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('w'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.filter_editing);
        assert_eq!(app.filter, "w");
    }

    #[test]
    fn ctrl_c_quits_while_filter_editing() {
        let mut app = grouped_app();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(!app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    #[test]
    fn q_does_not_quit_while_filter_editing_but_is_typed() {
        let mut app = grouped_app();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(app.filter, "q");
    }

    #[test]
    fn typing_selects_first_match_automatically() {
        let mut app = grouped_app();
        app.selected_row = 3; // db1, unrelated to the filter we're about to type
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('w'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('e'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        // "web" matches the group -> rows [0, 1, 2]; selection (3) was hidden,
        // so it should have jumped to the first visible row.
        assert_eq!(app.selected_row, 0);
    }
}
