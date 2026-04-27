use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::interval;

use crate::apt::HostInfo;
use crate::config::{Config, HostConfig, SidebarRow};

pub const TASK_OUTPUT_CAP: usize = 5_000;

// ── Messages flowing from background tasks to the app ────────────────────────

#[derive(Debug)]
pub enum AppMessage {
    GatherDone { host_idx: usize, result: Result<HostInfo, String> },
    TaskLine { host_idx: usize, line: String },
    TaskDone { host_idx: usize, exit_code: i32 },
    TaskFailed { host_idx: usize, error: String },
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
}

impl TaskKind {
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Update => "apt-get update",
            TaskKind::Upgrade => "apt-get upgrade",
            TaskKind::PurgeRc => "purge RC packages",
        }
    }

    pub fn command(&self, use_sudo: bool) -> String {
        let sudo = if use_sudo { "sudo -n " } else { "" };
        match self {
            TaskKind::Update => format!(
                "DEBIAN_FRONTEND=noninteractive LC_ALL=C {sudo}apt-get update 2>&1"
            ),
            TaskKind::Upgrade => format!(
                "DEBIAN_FRONTEND=noninteractive LC_ALL=C {sudo}apt-get -y upgrade 2>&1"
            ),
            TaskKind::PurgeRc => format!(
                r#"pkgs=$(LC_ALL=C dpkg -l | awk '/^rc/{{print $2}}'); [ -n "$pkgs" ] && echo "$pkgs" | xargs {sudo}dpkg --purge 2>&1 || echo "No RC packages to purge""#
            ),
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
    pub status: HostStatus,    pub info: Option<HostInfo>,
    pub task: Option<TaskState>,
}

impl HostState {
    pub fn new(cfg: HostConfig) -> Self {
        Self { cfg, status: HostStatus::Unknown, info: None, task: None }
    }
}

// ── Application state ─────────────────────────────────────────────────────────

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
}

impl App {
    pub fn new(config: &Config, tx: UnboundedSender<AppMessage>) -> Self {
        let host_cfgs = config.resolved_hosts();
        let sidebar_rows = config.sidebar_rows(&host_cfgs);
        let hosts: Vec<HostState> = host_cfgs.into_iter().map(HostState::new).collect();
        Self { hosts, sidebar_rows, selected_row: 0, tx, tick: 0, viewing_task: None, detail_zoom: false }
    }

    /// Indices of all hosts under the currently selected sidebar row.
    pub fn selected_host_indices(&self) -> Vec<usize> {
        match self.sidebar_rows.get(self.selected_row) {
            Some(SidebarRow::Host { host_idx }) => vec![*host_idx],
            Some(SidebarRow::Group { name }) => {
                // Collect all Host rows that follow this Group row until next Group
                let mut idxs = Vec::new();
                let mut in_group = false;
                for row in &self.sidebar_rows {
                    match row {
                        SidebarRow::Group { name: n } => {
                            if n == name {
                                in_group = true;
                            } else {
                                in_group = false;
                            }
                        }
                        SidebarRow::Host { host_idx } => {
                            if in_group {
                                idxs.push(*host_idx);
                            }
                        }
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
            let _ = tx.send(AppMessage::TaskDone { host_idx, exit_code });
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
            AppMessage::TaskDone { host_idx, exit_code } => {
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
        // ── Task output overlay ──
        if let Some(host_idx) = self.viewing_task {
            return self.handle_key_task_view(host_idx, code);
        }

        match (code, modifiers) {
            // Navigate sidebar
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if self.selected_row + 1 < self.sidebar_rows.len() {
                    self.selected_row += 1;
                }
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
}

// ── Terminal guard ────────────────────────────────────────────────────────────

pub struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

// ── Main event loop ───────────────────────────────────────────────────────────

pub async fn run(mut app: App, mut rx: UnboundedReceiver<AppMessage>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

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
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        if !app.handle_key(key.code, key.modifiers) {
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
