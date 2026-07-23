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

use crate::apt::HostInfo;
use crate::cache::{self, Cache};
use crate::config::{Config, HostConfig, SidebarRow};

pub const TASK_OUTPUT_CAP: usize = 5_000;

/// Maximum number of simultaneous SSH operations (gathers and apt tasks
/// combined). Triggering an action on a large group or "all" hosts still
/// queues the rest instead of opening a connection per host at once.
pub const MAX_CONCURRENT_SSH_OPS: usize = 8;

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
    Reboot,
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
            TaskKind::Reboot => "reboot".to_string(),
        }
    }

    pub fn command(&self, use_sudo: bool) -> String {
        let sudo = if use_sudo { "sudo -n " } else { "" };
        match self {
            TaskKind::Update => {
                format!("DEBIAN_FRONTEND=noninteractive LC_ALL=C {sudo}apt-get update 2>&1")
            }
            TaskKind::Upgrade => {
                format!("DEBIAN_FRONTEND=noninteractive LC_ALL=C {sudo}apt-get -y upgrade 2>&1")
            }
            TaskKind::UpgradeSecurity(pkgs) => {
                let names = pkgs.join(" ");
                format!(
                    "DEBIAN_FRONTEND=noninteractive LC_ALL=C {sudo}apt-get install --only-upgrade -y {names} 2>&1"
                )
            }
            TaskKind::FullUpgrade => {
                format!(
                    "DEBIAN_FRONTEND=noninteractive LC_ALL=C {sudo}apt-get -y full-upgrade 2>&1"
                )
            }
            TaskKind::AutoRemove => {
                format!(
                    "DEBIAN_FRONTEND=noninteractive LC_ALL=C {sudo}apt-get -y autoremove --purge 2>&1"
                )
            }
            TaskKind::PurgeRc => format!(
                r#"pkgs=$(LC_ALL=C dpkg -l | awk '/^rc/{{print $2}}'); [ -n "$pkgs" ] && echo "$pkgs" | xargs {sudo}dpkg --purge 2>&1 || echo "No RC packages to purge""#
            ),
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
