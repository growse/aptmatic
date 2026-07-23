use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::app::{App, HostStatus, TaskStatus};
use crate::apt::HoldReason;
use crate::config::SidebarRow;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(f: &mut Frame, app: &mut App) {
    if let Some(host_idx) = app.viewing_task {
        render_task_view(f, app, host_idx);
    } else {
        let area = f.area();

        if app.detail_zoom {
            // Full-screen borderless detail: no title bar, no status bar, no block border,
            // so Shift+select copies only content.
            render_detail(f, app, area, true);
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(0),
                    Constraint::Length(2),
                ])
                .split(area);

            render_title_bar(f, chunks[0]);

            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(vec![
                    Constraint::Length(app.sidebar_width),
                    Constraint::Min(0),
                ])
                .split(chunks[1]);

            render_sidebar(f, app, body[0]);
            render_detail(f, app, body[1], false);
            render_status_bar(f, app, chunks[2]);
        }
    }

    if let Some(state) = &app.reboot_confirm {
        let hostname = app.hosts[state.host_idx].cfg.hostname.clone();
        render_reboot_confirm_modal(f, &hostname, &state.input, state.mismatch);
    }

    if app.quit_confirm {
        render_quit_confirm_modal(f, app.active_operation_count());
    }
}

fn render_title_bar(f: &mut Frame, area: Rect) {
    let title = Paragraph::new(" aptmatic — apt manager").style(
        Style::default()
            .bg(Color::Blue)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(title, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let running = app
        .hosts
        .iter()
        .filter(|h| matches!(h.status, HostStatus::Connecting | HostStatus::Gathering))
        .count();
    let refresh_status = if running > 0 {
        format!(" [{running} refreshing…]  ")
    } else {
        String::new()
    };
    let line1 = format!(
        "{refresh_status}r:update+refresh  R:update+refresh all  u:upgrade  U:upgrade all  f:full-upgrade  F:full-upgrade all  s:sec-upgrade  S:sec-upgrade all"
    );
    let line2 = " a:autoremove  A:autoremove all  p:purge-rc  b:reboot  t:task output  z:zoom  /:search  q:quit";
    let text = vec![
        Line::from(Span::raw(format!(" {line1}"))),
        Line::from(Span::raw(line2)),
    ];
    let bar = Paragraph::new(text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(bar, area);
}

fn render_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let title = if app.filter_editing {
        format!("Hosts  /{}", app.filter)
    } else if !app.filter.is_empty() {
        format!("Hosts  (filter: {})", app.filter)
    } else {
        "Hosts".to_string()
    };
    let block = Block::default().borders(Borders::RIGHT).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let filtered = app.filtered_row_indices();

    if filtered.is_empty() {
        let p = Paragraph::new(" no matches").style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, inner);
        return;
    }

    let items: Vec<ListItem> = filtered
        .iter()
        .map(|&row_idx| match &app.sidebar_rows[row_idx] {
            SidebarRow::Group { name } => ListItem::new(Line::from(vec![Span::styled(
                format!(" ▸ {name}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )])),
            SidebarRow::Host { host_idx } => {
                let h = &app.hosts[*host_idx];
                let indicator = host_indicator(h, app.tick);
                let name_style = match &h.status {
                    HostStatus::Error(_) => Style::default().fg(Color::Red),
                    HostStatus::Ready => Style::default().fg(Color::Green),
                    _ => Style::default().fg(Color::Gray),
                };
                let kernel_version = h
                    .info
                    .as_ref()
                    .map(|i| i.running_kernel.as_str())
                    .unwrap_or("");
                let kernel_badge = if !kernel_version.is_empty() {
                    Span::styled(
                        format!(" [{kernel_version}]"),
                        Style::default().fg(Color::Yellow),
                    )
                } else {
                    Span::raw("")
                };
                let upgrades = h.info.as_ref().map(|i| i.upgradable.len()).unwrap_or(0);
                let update_count_badge = if upgrades > 0 {
                    Span::styled(format!(" [{upgrades}]"), Style::default().fg(Color::Cyan))
                } else {
                    Span::raw("")
                };
                let security_count = h.info.as_ref().map(|i| i.security_count()).unwrap_or(0);
                let security_badge = if security_count > 0 {
                    Span::styled(
                        format!(" [{security_count} sec]"),
                        Style::default().fg(Color::Red),
                    )
                } else {
                    Span::raw("")
                };
                let reboot_required = h.info.as_ref().map(|i| i.reboot_required).unwrap_or(false);
                let reboot_needed_badge = if reboot_required {
                    Span::raw(" [R]")
                } else {
                    Span::raw("")
                };
                ListItem::new(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(indicator, Style::default().fg(Color::Gray)),
                    Span::raw(" "),
                    Span::styled(h.cfg.hostname.clone(), name_style),
                    kernel_badge,
                    update_count_badge,
                    security_badge,
                    reboot_needed_badge,
                ]))
            }
        })
        .collect();

    let mut state = ListState::default();
    state.select(filtered.iter().position(|&r| r == app.selected_row));

    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, inner, &mut state);
}

fn host_indicator(h: &crate::app::HostState, tick: u64) -> String {
    match &h.status {
        HostStatus::Unknown => "?".to_string(),
        HostStatus::Connecting | HostStatus::Gathering => {
            SPINNER[(tick / 2) as usize % SPINNER.len()].to_string()
        }
        HostStatus::Ready => {
            if h.task
                .as_ref()
                .map(|t| matches!(t.status, TaskStatus::Running))
                .unwrap_or(false)
            {
                SPINNER[(tick / 2) as usize % SPINNER.len()].to_string()
            } else {
                "●".to_string()
            }
        }
        HostStatus::Error(_) => "✗".to_string(),
    }
}

fn render_detail(f: &mut Frame, app: &App, area: Rect, borderless: bool) {
    let indices = app.selected_host_indices();
    let inner = if borderless {
        area
    } else {
        let block = Block::default().borders(Borders::ALL).title(" Detail ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        inner
    };

    if indices.is_empty() {
        let p = Paragraph::new("No selection");
        f.render_widget(p, inner);
        return;
    }

    if indices.len() > 1 {
        // Group selected — show summary
        render_group_detail(f, app, &indices, inner);
    } else {
        render_host_detail(f, app, indices[0], inner);
    }
}

fn render_group_detail(f: &mut Frame, app: &App, indices: &[usize], area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    for &idx in indices {
        let h = &app.hosts[idx];
        let indicator = host_indicator(h, app.tick);
        let status_str = host_status_str(h);
        let upgrades = h.info.as_ref().map(|i| i.upgradable.len()).unwrap_or(0);
        let line_style = match &h.status {
            HostStatus::Error(_) => Style::default().fg(Color::Red),
            HostStatus::Ready if upgrades > 0 => Style::default().fg(Color::Cyan),
            HostStatus::Ready => Style::default().fg(Color::Green),
            _ => Style::default().fg(Color::Gray),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{indicator} "), Style::default()),
            Span::styled(format!("{:<30}", h.cfg.hostname), line_style),
            Span::raw("  "),
            Span::styled(status_str, Style::default().fg(Color::Gray)),
        ]));
    }
    let p = Paragraph::new(lines);
    f.render_widget(p, area);
}

fn render_host_detail(f: &mut Frame, app: &App, host_idx: usize, area: Rect) {
    let h = &app.hosts[host_idx];

    let task_height = if h.task.is_some() {
        Constraint::Fill(3)
    } else {
        Constraint::Length(3)
    };

    let panes = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // host header
            Constraint::Length(6), // kernel
            Constraint::Fill(1),   // upgradable + autoremovable side-by-side
            task_height,           // live task output
        ])
        .split(area);

    render_host_header(f, h, panes[0]);
    render_kernel_panel(f, h, panes[1]);

    // Upgradable and autoremovable side-by-side
    let pkg_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(2), Constraint::Fill(1)])
        .split(panes[2]);
    render_upgradable_panel(f, h, pkg_row[0]);
    render_autoremovable_panel(f, h, pkg_row[1]);

    render_task_panel(f, app, host_idx, panes[3]);
}

fn render_host_header(f: &mut Frame, h: &crate::app::HostState, area: Rect) {
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", h.cfg.hostname),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  user: {}  port: {}  sudo: {}",
                    h.cfg.user, h.cfg.port, h.cfg.use_sudo
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Status: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(host_status_str(h), status_color(&h.status)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_kernel_panel(f: &mut Frame, h: &crate::app::HostState, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Kernel ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(info) = &h.info {
        lines.push(Line::from(vec![
            Span::raw(" Running: "),
            Span::styled(&info.running_kernel, Style::default().fg(Color::White)),
        ]));
        if let Some(latest) = &info.latest_kernel {
            let latest_ver = latest.trim_start_matches("linux-image-");
            let pending = latest_ver != info.running_kernel;
            if pending {
                lines.push(Line::from(vec![
                    Span::raw(" Latest:  "),
                    Span::styled(latest_ver, Style::default().fg(Color::Yellow)),
                    Span::styled(
                        " ← reboot to activate",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" Latest:  "),
                    Span::styled(latest_ver, Style::default().fg(Color::Green)),
                ]));
            }
        }
        if info.reboot_required {
            lines.push(Line::from(Span::styled(
                " ⚠ Reboot required",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            " gathering…",
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_upgradable_panel(f: &mut Frame, h: &crate::app::HostState, area: Rect) {
    let info = match &h.info {
        Some(i) => i,
        None => {
            let block = Block::default().borders(Borders::ALL).title(" Upgradable ");
            f.render_widget(block, area);
            return;
        }
    };

    let security_count = info.security_count();
    let title = if info.upgradable.is_empty() {
        " Upgradable: none ".to_string()
    } else if security_count > 0 {
        format!(
            " Upgradable ({}, {security_count} security) ",
            info.upgradable.len()
        )
    } else {
        format!(" Upgradable ({}) ", info.upgradable.len())
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    for pkg in &info.upgradable {
        let from = pkg
            .current_version
            .as_deref()
            .map(|v| format!(" ({v} →)"))
            .unwrap_or_default();
        let security_badge = if pkg.is_security {
            Span::styled(" [security]", Style::default().fg(Color::Red))
        } else {
            Span::raw("")
        };
        lines.push(Line::from(vec![
            Span::raw(format!(" {}{from} ", pkg.name)),
            Span::styled(&pkg.new_version, Style::default().fg(Color::Cyan)),
            security_badge,
        ]));
    }

    if !info.held_packages.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(section_header(&format!(
            "Held / kept back ({})",
            info.held_packages.len()
        )));
        for pkg in &info.held_packages {
            let reason = match pkg.reason {
                HoldReason::ManualHold => {
                    Span::styled("[manual hold]", Style::default().fg(Color::Yellow))
                }
                HoldReason::KeptBack => {
                    let label = match &pkg.detail {
                        Some(d) => format!("[kept back: {d}]"),
                        None => "[kept back]".to_string(),
                    };
                    Span::styled(label, Style::default().fg(Color::Magenta))
                }
            };
            lines.push(Line::from(vec![
                Span::raw(format!("  {} ", pkg.name)),
                reason,
            ]));
        }
    }

    if !info.rc_packages.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(section_header(&format!(
            "RC packages ({})",
            info.rc_packages.len()
        )));
        for pkg in &info.rc_packages {
            lines.push(Line::from(Span::raw(format!("  {}", pkg.name))));
        }
        lines.push(Line::from(Span::styled(
            "  Press p to purge",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Scroll to show as many lines as fit; excess lines overflow silently.
    let visible = inner.height as usize;
    let total = lines.len();
    let scroll = total.saturating_sub(visible) as u16;
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

fn render_autoremovable_panel(f: &mut Frame, h: &crate::app::HostState, area: Rect) {
    let pkgs = h.info.as_ref().map(|i| &i.autoremovable);

    let title = match pkgs {
        None => " Autoremovable ".to_string(),
        Some(v) if v.is_empty() => " Autoremovable: none ".to_string(),
        Some(v) => format!(" Autoremovable ({}) ", v.len()),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(pkgs) = pkgs
        && !pkgs.is_empty()
    {
        let lines: Vec<Line> = pkgs
            .iter()
            .map(|name| Line::from(Span::raw(format!(" {name}"))))
            .collect();
        let visible = inner.height as usize;
        let scroll = lines.len().saturating_sub(visible) as u16;
        f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
    }
}

fn render_task_panel(f: &mut Frame, app: &App, host_idx: usize, area: Rect) {
    let h = &app.hosts[host_idx];

    let (title, status_line) = match &h.task {
        None => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Task output: none ");
            f.render_widget(block, area);
            return;
        }
        Some(task) => {
            let status = match &task.status {
                TaskStatus::Running => {
                    let sp = SPINNER[(app.tick / 2) as usize % SPINNER.len()];
                    format!("{sp} {} running…  t:full view", task.kind.label())
                }
                TaskStatus::Done(0) => {
                    format!("✓ {} done  t:full view", task.kind.label())
                }
                TaskStatus::Done(code) => {
                    format!("✗ {} exited {code}  t:full view", task.kind.label())
                }
                TaskStatus::Failed(e) => {
                    format!("✗ {}: {e}  t:full view", task.kind.label())
                }
            };
            let title = format!(" Task: {} ", task.kind.label());
            (title, status)
        }
    };

    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let task = h.task.as_ref().unwrap();

    // Status line at top of panel
    let status_style = match &task.status {
        TaskStatus::Running => Style::default().fg(Color::Yellow),
        TaskStatus::Done(0) => Style::default().fg(Color::Green),
        _ => Style::default().fg(Color::Red),
    };

    // Reserve one row for status line, rest for output
    let output_rows = inner.height.saturating_sub(1) as usize;
    let total = task.output.len();
    let start = total.saturating_sub(output_rows);
    let output_lines: Vec<Line> = task
        .output
        .range(start..)
        .map(|l| Line::from(Span::raw(l.as_str())))
        .collect();

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {status_line}"),
            status_style,
        ))),
        layout[0],
    );
    f.render_widget(Paragraph::new(output_lines), layout[1]);
}

fn render_task_view(f: &mut Frame, app: &App, host_idx: usize) {
    let area = f.area();
    let h = &app.hosts[host_idx];
    let task = match &h.task {
        Some(t) => t,
        None => return,
    };

    // Full-screen overlay
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    // Header
    let status_str = match &task.status {
        TaskStatus::Running => {
            let spinner = SPINNER[(app.tick / 2) as usize % SPINNER.len()];
            format!("{spinner} running…")
        }
        TaskStatus::Done(0) => "✓ done".to_string(),
        TaskStatus::Done(code) => format!("✗ exited {code}"),
        TaskStatus::Failed(e) => format!("✗ failed: {e}"),
    };
    let header_text = format!(
        "  {} — {}  [{}]",
        h.cfg.hostname,
        task.kind.label(),
        status_str
    );
    let header = Paragraph::new(header_text)
        .block(Block::default().borders(Borders::ALL))
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(header, chunks[0]);

    // Output
    let log_block = Block::default().borders(Borders::ALL).title(" Output ");
    let inner = log_block.inner(chunks[1]);
    f.render_widget(log_block, chunks[1]);

    let total_lines = task.output.len() as u16;
    let visible = inner.height;
    let scroll_y = if task.auto_scroll {
        total_lines.saturating_sub(visible)
    } else {
        task.scroll_offset
    };

    let text: Vec<Line> = task
        .output
        .iter()
        .map(|l| Line::from(Span::raw(l.as_str())))
        .collect();
    let log = Paragraph::new(text).scroll((scroll_y, 0));
    f.render_widget(log, inner);

    // Footer hint
    let footer_text = match task.auto_scroll {
        true => " ↑/↓/PgUp/PgDn:scroll  G/End:tail(auto)  Esc/q:close",
        false => " ↑/↓/PgUp/PgDn:scroll  G/End:tail       Esc/q:close",
    };
    let footer =
        Paragraph::new(footer_text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(footer, chunks[2]);
}

fn section_header(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    ))
}

fn host_status_str(h: &crate::app::HostState) -> String {
    match &h.status {
        HostStatus::Unknown => "unknown".to_string(),
        HostStatus::Connecting => "connecting…".to_string(),
        HostStatus::Gathering => "gathering…".to_string(),
        HostStatus::Ready => {
            let upgrades = h.info.as_ref().map(|i| i.upgradable.len()).unwrap_or(0);
            let security = h.info.as_ref().map(|i| i.security_count()).unwrap_or(0);
            if upgrades > 0 && security > 0 {
                format!("{upgrades} upgrade(s) available ({security} security)")
            } else if upgrades > 0 {
                format!("{upgrades} upgrade(s) available")
            } else {
                "up to date".to_string()
            }
        }
        HostStatus::Error(e) => format!("error: {e}"),
    }
}

fn status_color(status: &HostStatus) -> Style {
    match status {
        HostStatus::Ready => Style::default().fg(Color::Green),
        HostStatus::Error(_) => Style::default().fg(Color::Red),
        HostStatus::Connecting | HostStatus::Gathering => Style::default().fg(Color::Yellow),
        HostStatus::Unknown => Style::default().fg(Color::Gray),
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn render_quit_confirm_modal(f: &mut Frame, running: usize) {
    let area = f.area();
    let modal_area = centered_rect(56.min(area.width), 8.min(area.height), area);
    f.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " ⚠  Confirm Quit ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let op_word = if running == 1 {
        "operation"
    } else {
        "operations"
    };
    let lines = vec![
        Line::raw(""),
        Line::from(Span::raw(format!(
            "  {running} background {op_word} still running."
        ))),
        Line::from(Span::raw(
            "  Quitting closes the SSH connection(s), which may",
        )),
        Line::from(Span::raw("  interrupt the operation on the remote host.")),
        Line::raw(""),
        Line::from(Span::styled(
            "  y: quit anyway   any other key: cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_reboot_confirm_modal(f: &mut Frame, hostname: &str, input: &str, mismatch: bool) {
    let area = f.area();
    let modal_area = centered_rect(60.min(area.width), 11.min(area.height), area);
    f.render_widget(Clear, modal_area);

    let border_color = if mismatch { Color::Red } else { Color::Yellow };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " ⚠  Confirm Reboot ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let input_style = if mismatch {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::White)
    };

    let mut lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  Type "),
            Span::styled(
                hostname.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" to confirm:"),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  > "),
            Span::styled(input.to_string(), input_style),
            Span::styled("█", Style::default().fg(Color::White)),
        ]),
        Line::raw(""),
    ];

    if mismatch {
        lines.push(Line::from(Span::styled(
            "  ✗ hostname does not match",
            Style::default().fg(Color::Red),
        )));
    } else {
        lines.push(Line::raw(""));
    }

    lines.push(Line::from(Span::styled(
        "  Enter: confirm reboot   Esc: cancel",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}
