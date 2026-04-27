use std::collections::VecDeque;
use std::io;
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
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::interval;

use crate::apt::HostInfo;
use crate::config::{Config, HostConfig, SidebarRow};

pub const TASK_OUTPUT_CAP: usize = 5_000;

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
    PurgeRc,
    Reboot,
}

impl TaskKind {
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Update => "apt-get update",
            TaskKind::Upgrade => "apt-get upgrade",
            TaskKind::PurgeRc => "purge RC packages",
            TaskKind::Reboot => "reboot",
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
}

impl HostState {
    pub fn new(cfg: HostConfig) -> Self {
        Self {
            cfg,
            status: HostStatus::Unknown,
            info: None,
            task: None,
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
}

impl App {
    pub fn new(config: &Config, tx: UnboundedSender<AppMessage>) -> Self {
        let host_cfgs = config.resolved_hosts();
        let sidebar_rows = config.sidebar_rows(&host_cfgs);
        let hosts: Vec<HostState> = host_cfgs.into_iter().map(HostState::new).collect();
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
        }
    }

    /// Indices of all hosts under the currently selected sidebar row.
    pub fn selected_host_indices(&self) -> Vec<usize> {
        match self.sidebar_rows.get(self.selected_row) {
            Some(SidebarRow::Host { host_idx }) => vec![*host_idx],
            Some(SidebarRow::Group { .. }) => {
                // Collect Host rows immediately following this Group row, stopping
                // at the next Group row. Matching by position (not name) avoids
                // incorrectly merging duplicate-named groups.
                let mut idxs = Vec::new();
                for row in self.sidebar_rows.iter().skip(self.selected_row + 1) {
                    match row {
                        SidebarRow::Group { .. } => break,
                        SidebarRow::Host { host_idx } => idxs.push(*host_idx),
                    }
                }
                idxs
            }
            None => vec![],
        }
    }

    /// Trigger a gather refresh for one host.
    pub fn start_refresh(&self, host_idx: usize) {
        let cfg = self.hosts[host_idx].cfg.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = crate::gather::gather(&cfg).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppMessage::GatherDone { host_idx, result });
        });
    }

    /// Trigger an apt task on one host.
    pub fn start_task(&mut self, host_idx: usize, kind: TaskKind) {
        let cfg = self.hosts[host_idx].cfg.clone();
        let tx = self.tx.clone();
        let cmd = kind.command(cfg.use_sudo);
        self.hosts[host_idx].task = Some(TaskState::new(kind));
        tokio::task::spawn_blocking(move || {
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
        });
    }

    pub fn handle_message(&mut self, msg: AppMessage) {
        match msg {
            AppMessage::GatherDone { host_idx, result } => {
                let h = &mut self.hosts[host_idx];
                match result {
                    Ok(info) => {
                        h.info = Some(info);
                        h.status = HostStatus::Ready;
                    }
                    Err(e) => {
                        h.status = HostStatus::Error(e);
                    }
                }
            }
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

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // ── Reboot confirmation modal (highest priority) ──
        if self.reboot_confirm.is_some() {
            return self.handle_key_reboot_confirm(code, modifiers);
        }

        // ── Task output overlay ──
        if let Some(host_idx) = self.viewing_task {
            return self.handle_key_task_view(host_idx, code);
        }

        match (code, modifiers) {
            // Navigate sidebar
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE)
                if self.selected_row > 0 =>
            {
                self.selected_row -= 1;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE)
                if self.selected_row + 1 < self.sidebar_rows.len() =>
            {
                self.selected_row += 1;
            }
            // Refresh selection
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                for idx in self.selected_host_indices() {
                    self.hosts[idx].status = HostStatus::Gathering;
                    self.start_refresh(idx);
                }
            }
            // Refresh all
            (KeyCode::Char('R'), _) => {
                for idx in 0..self.hosts.len() {
                    self.hosts[idx].status = HostStatus::Gathering;
                    self.start_refresh(idx);
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
            // apt-get update
            (KeyCode::Char('u'), KeyModifiers::NONE) => {
                for idx in self.selected_host_indices() {
                    self.start_task(idx, TaskKind::Update);
                }
            }
            // apt-get upgrade
            (KeyCode::Char('U'), _) => {
                for idx in self.selected_host_indices() {
                    self.start_task(idx, TaskKind::Upgrade);
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
            // Quit
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                return false;
            }
            (KeyCode::Esc, _) => return false,
            _ => {}
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

// ── Terminal guard ────────────────────────────────────────────────────────────

pub struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
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

    loop {
        terminal.draw(|f| crate::ui::render(f, &mut app))?;

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
                        break;
                    }
                    Event::Mouse(mouse) => {
                        app.handle_mouse(mouse.kind, mouse.column);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
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
        assert_eq!(app.selected_host_indices(), vec![]);
    }

    #[test]
    fn selected_host_indices_out_of_bounds_returns_empty() {
        let mut app = bare_app();
        app.selected_row = 999;
        assert_eq!(app.selected_host_indices(), vec![]);
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
}
