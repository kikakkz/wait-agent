use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node_runtime::{ratatui_socket_path, RatatuiSnapshot, SessionView};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

/// Ratatui TUI client: connects to a session's node server and renders the workspace chrome.
pub struct RatatuiClientRuntime {
    session_name: String,
}

impl RatatuiClientRuntime {
    pub fn from_session(session_name: String) -> Result<Self, LifecycleError> {
        Ok(Self { session_name })
    }

    pub fn run(&self) -> Result<(), LifecycleError> {
        let socket_path = ratatui_socket_path(&self.session_name);
        ERROR_LOG.log(format!(
            "[ratatui-client] connecting to socket={} session={}",
            socket_path.display(),
            self.session_name
        ));

        let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to connect to ratatui node socket {}",
                    socket_path.display()
                ),
                error,
            )
        })?;

        let reader = stream.try_clone().map_err(|error| {
            LifecycleError::Io("failed to clone ratatui client stream".to_string(), error)
        })?;
        let mut reader = BufReader::new(reader);

        // Read the initial snapshot from the server.
        let mut line = String::new();
        let snapshot = match reader.read_line(&mut line) {
            Ok(0) | Err(_) => RatatuiSnapshot::default(),
            Ok(_) => parse_snapshot(&line),
        };

        ERROR_LOG.log(format!(
            "[ratatui-client] snapshot session={} clients={} main={} sidebar={} sessions={}",
            snapshot.session_name,
            snapshot.client_count,
            snapshot.main,
            snapshot.sidebar,
            snapshot.sessions.len()
        ));

        // Spawn a background reader so the server can push snapshot updates.
        let (server_tx, server_rx) = mpsc::channel::<ServerMessage>();
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let message = if trimmed.starts_with('{') {
                            ServerMessage::Snapshot(parse_snapshot(trimmed))
                        } else {
                            ServerMessage::Log(trimmed.to_string())
                        };
                        if server_tx.send(message).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            original_hook(info);
        }));

        let terminal = init_terminal().map_err(|error| {
            LifecycleError::Io("failed to initialize ratatui terminal".to_string(), error)
        })?;
        let result = run_event_loop(terminal, &mut stream, snapshot, server_rx);
        let _ = restore_terminal();

        result
    }
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Main,
    Sidebar,
}

enum ServerMessage {
    Snapshot(RatatuiSnapshot),
    Log(String),
}

#[derive(Debug, Clone, Default)]
struct Overlay {
    kind: OverlayKind,
    buffer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OverlayKind {
    #[default]
    None,
    ConnectProfile,
}

fn parse_snapshot(line: &str) -> RatatuiSnapshot {
    serde_json::from_str(line.trim()).unwrap_or_default()
}

fn run_event_loop(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    stream: &mut UnixStream,
    mut snapshot: RatatuiSnapshot,
    server_rx: Receiver<ServerMessage>,
) -> Result<(), LifecycleError> {
    let mut prefix_pressed = false;
    let mut focus = Focus::Main;
    let mut selected_index = 0usize;
    let mut overlay = Overlay::default();
    let mut status_message: Option<String> = None;

    loop {
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &snapshot,
                    focus,
                    selected_index,
                    snapshot.active_target.as_deref(),
                    &overlay,
                    status_message.as_deref(),
                )
            })
            .map_err(|error| {
                LifecycleError::Io("failed to draw ratatui frame".to_string(), error)
            })?;

        // Drain any server-pushed snapshots before waiting for input.
        loop {
            match server_rx.try_recv() {
                Ok(ServerMessage::Snapshot(new_snapshot)) => {
                    snapshot = new_snapshot;
                    if selected_index >= snapshot.sessions.len() && !snapshot.sessions.is_empty() {
                        selected_index = snapshot.sessions.len() - 1;
                    }
                }
                Ok(ServerMessage::Log(text)) => {
                    ERROR_LOG.log(format!("[ratatui-client] server: {text}"));
                    status_message = Some(text);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if event::poll(Duration::from_millis(50)).map_err(|error| {
            LifecycleError::Io("failed to poll crossterm events".to_string(), error)
        })? {
            match event::read().map_err(|error| {
                LifecycleError::Io("failed to read crossterm event".to_string(), error)
            })? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if overlay.kind != OverlayKind::None {
                        if handle_overlay_key(&key, &mut overlay, stream, &mut status_message) {
                            continue;
                        }
                    }

                    if prefix_pressed {
                        prefix_pressed = false;
                        match key.code {
                            KeyCode::Char('d') | KeyCode::Char('D') => {
                                let _ = writeln!(stream, "DETACH");
                                let _ = stream.flush();
                                break;
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                prefix_pressed = true;
                            }
                            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                focus = Focus::Sidebar;
                                if selected_index >= snapshot.sessions.len()
                                    && !snapshot.sessions.is_empty()
                                {
                                    selected_index = snapshot.sessions.len() - 1;
                                }
                            }
                            KeyCode::Left if focus == Focus::Sidebar => {
                                focus = Focus::Main;
                            }
                            KeyCode::Up if focus == Focus::Sidebar && selected_index > 0 => {
                                selected_index -= 1;
                            }
                            KeyCode::Down
                                if focus == Focus::Sidebar
                                    && selected_index + 1 < snapshot.sessions.len() =>
                            {
                                selected_index += 1;
                            }
                            KeyCode::Enter if focus == Focus::Sidebar => {
                                if let Some(session) = snapshot.sessions.get(selected_index) {
                                    snapshot.active_target = Some(session.id.clone());
                                    let _ = writeln!(stream, "ACTIVATE_TARGET {}", session.id);
                                    let _ = stream.flush();
                                    ERROR_LOG.log(format!(
                                        "[ratatui-client] activate session: {}",
                                        session.id
                                    ));
                                }
                            }
                            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let _ = writeln!(stream, "CREATE_LOCAL_SESSION");
                                let _ = stream.flush();
                            }
                            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                overlay = Overlay {
                                    kind: OverlayKind::ConnectProfile,
                                    buffer: String::new(),
                                };
                            }
                            _ => {}
                        }
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    Ok(())
}

fn render(
    frame: &mut Frame,
    snapshot: &RatatuiSnapshot,
    focus: Focus,
    selected_index: usize,
    active_target: Option<&str>,
    overlay: &Overlay,
    status_message: Option<&str>,
) {
    let area = frame.size();

    // Outer vertical layout: chrome above, footer below.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // Inner horizontal layout: main pane left, separator, sidebar right.
    let inner = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(32),
        ])
        .split(outer[0]);

    let main_block = Block::default()
        .title(snapshot.main.clone())
        .borders(Borders::NONE)
        .title_style(title_style(focus == Focus::Main));
    let main =
        Paragraph::new("Main pane placeholder\n\nPress Ctrl+B d to detach.").block(main_block);
    frame.render_widget(main, inner[0]);

    let separator = Block::default()
        .borders(Borders::LEFT)
        .border_style(separator_style(focus));
    frame.render_widget(separator, inner[1]);

    let sidebar_block = Block::default()
        .borders(Borders::NONE)
        .title_style(title_style(focus == Focus::Sidebar));
    let sidebar = Paragraph::new(render_sidebar_lines(
        &snapshot.sessions,
        selected_index,
        inner[2],
        focus == Focus::Sidebar,
        active_target,
    ))
    .block(sidebar_block);
    frame.render_widget(sidebar, inner[2]);

    let footer = if let Some(status) = status_message {
        let style = Style::default().bg(Color::Yellow).fg(Color::Black);
        Paragraph::new(pad_right(status, outer[1].width as usize)).style(style)
    } else {
        let footer_style = Style::default().bg(Color::Blue).fg(Color::White);
        Paragraph::new(render_footer_line(snapshot, outer[1].width as usize)).style(footer_style)
    };
    frame.render_widget(footer, outer[1]);

    if overlay.kind == OverlayKind::ConnectProfile {
        render_connect_overlay(frame, overlay, area);
    }
}

fn render_sidebar_lines<'a>(
    sessions: &'a [SessionView],
    selected_index: usize,
    area: Rect,
    is_focused: bool,
    active_target: Option<&'a str>,
) -> Vec<Line<'a>> {
    let width = area.width as usize;
    let height = area.height as usize;
    let mut lines = Vec::new();

    // Header.
    lines.push(Line::from(vec![Span::styled(
        " [h] hide",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )]));

    // Separator.
    lines.push(Line::from("─".repeat(width)));

    if sessions.is_empty() {
        while lines.len() + 2 < height {
            lines.push(Line::from(""));
        }
        lines.push(Line::from("─".repeat(width)));
        lines.push(Line::from(vec![Span::styled(
            right_align("(no sessions)", width),
            Style::default().fg(Color::Gray),
        )]));
        return lines;
    }

    let selected = sessions
        .get(selected_index)
        .cloned()
        .unwrap_or_else(|| sessions[0].clone());

    // Visible rows with scrolling.
    let max_rows = height.saturating_sub(lines.len() + 2);
    let start = if selected_index + 1 > max_rows {
        selected_index + 1 - max_rows
    } else {
        0
    };
    for (idx, session) in sessions.iter().enumerate().skip(start).take(max_rows) {
        let is_selected = idx == selected_index;
        let is_current = active_target == Some(session.id.as_str());
        lines.push(render_session_row(
            session,
            is_selected,
            is_current,
            width,
            is_focused,
        ));
    }

    while lines.len() + 2 < height {
        lines.push(Line::from(""));
    }

    // Bottom separator and detail line.
    lines.push(Line::from("─".repeat(width)));
    lines.push(Line::from(vec![Span::styled(
        right_align(&selected_detail_text(&selected, width), width),
        Style::default().fg(Color::Gray),
    )]));

    lines
}

fn render_session_row(
    session: &SessionView,
    is_selected: bool,
    is_current: bool,
    width: usize,
    is_focused: bool,
) -> Line {
    let marker = if is_selected {
        ">"
    } else if is_current {
        "*"
    } else {
        " "
    };
    let (badge, badge_width) = sidebar_badge(&session.task_state);
    let reserved = marker.len() + 1 + 1 + badge_width;
    let label_width = width.saturating_sub(reserved);
    let label = session_row_primary_label(session, label_width);

    let base_style = if is_selected && is_focused {
        Style::default().bg(Color::Blue).fg(Color::White)
    } else {
        Style::default()
    };

    Line::from(vec![
        Span::styled(format!("{}{} ", marker, label), base_style),
        Span::styled(
            badge.to_string(),
            badge_style(&session.task_state).patch(base_style),
        ),
    ])
}

fn session_row_primary_label(session: &SessionView, width: usize) -> String {
    for candidate in session.display_label_candidates() {
        if display_width(&candidate) <= width {
            return pad_right(&candidate, width);
        }
    }
    if let Some(first) = session.display_label_candidates().first() {
        return pad_right(&truncate_display_width(first, width), width);
    }
    pad_right(
        &truncate_display_width(&session.display_label(), width),
        width,
    )
}

fn selected_detail_text(session: &SessionView, width: usize) -> String {
    let suffix = if session.availability != "online" {
        session.availability.to_ascii_uppercase()
    } else {
        session.task_state.to_uppercase()
    };
    let full_label = session.display_label();
    let full_detail = format!("{full_label} {suffix}");
    if display_width(&full_detail) <= width {
        return full_detail;
    }

    if session.transport != "local" {
        let command_host_label =
            format!("{}@{}", session.command_name, session.display_authority_id);
        let command_host_detail = format!("{command_host_label} {suffix}");
        if display_width(&command_host_detail) <= width {
            return command_host_detail;
        }

        let host_only_detail = format!("{} {suffix}", session.display_authority_id);
        if display_width(&host_only_detail) <= width {
            return host_only_detail;
        }

        if display_width(&session.display_authority_id) <= width {
            return session.display_authority_id.clone();
        }
    }

    truncate_display_width(&full_detail, width)
}

fn sidebar_badge(task_state: &str) -> (&'static str, usize) {
    let badge = match task_state {
        "running" => "🔥R",
        "input" => "🔊I",
        "confirm" => "📢C",
        _ => "·U",
    };
    (badge, display_width(badge))
}

fn badge_style(task_state: &str) -> Style {
    match task_state {
        "running" => Style::default().fg(Color::Yellow),
        "input" => Style::default().fg(Color::Cyan),
        "confirm" => Style::default().fg(Color::Magenta),
        _ => Style::default().fg(Color::Gray),
    }
}

fn right_align(text: &str, width: usize) -> String {
    let text_width = display_width(text);
    let padding = width.saturating_sub(text_width);
    format!("{}{}", " ".repeat(padding), text)
}

fn pad_right(text: &str, width: usize) -> String {
    let truncated = truncate_display_width(text, width);
    let padding = width.saturating_sub(display_width(&truncated));
    format!("{}{}", truncated, " ".repeat(padding))
}

fn truncate_display_width(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let ch_width = char_width(ch);
        if used + ch_width > width {
            break;
        }
        output.push(ch);
        used += ch_width;
    }
    output
}

fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(ch: char) -> usize {
    // Emoji and CJK characters are typically width 2 in terminals.
    if ch as u32 >= 0x1F300 {
        2
    } else {
        unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

fn render_footer_line(snapshot: &RatatuiSnapshot, area_width: usize) -> Line {
    let muted_style = Style::default().fg(Color::Gray);
    let accent_style = Style::default().fg(Color::White);

    let mut spans = Vec::new();

    if let Some(listener) = &snapshot.footer.listener_endpoint {
        spans.push(Span::styled("Listen ", accent_style));
        spans.push(Span::styled(format!("{} ", listener), muted_style));
    }
    if let Some(connect) = &snapshot.footer.connect_endpoint {
        spans.push(Span::styled("· Connect ", accent_style));
        spans.push(Span::styled(format!("{} ", connect), muted_style));
    }
    if snapshot.footer.remote_count > 0 {
        spans.push(Span::styled("· ", muted_style));
        spans.push(Span::styled(
            format!("{} remote ", snapshot.footer.remote_count),
            muted_style,
        ));
    }

    if !spans.is_empty() {
        spans.push(Span::styled("· ", muted_style));
    }

    spans.push(Span::styled(
        "Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-O Hist · Ctrl-E Logs · Ctrl-M Menu",
        muted_style,
    ));

    let text_width: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    let fill = area_width.saturating_sub(text_width);
    if fill > 0 {
        spans.push(Span::styled(" ".repeat(fill), muted_style));
    }

    Line::from(spans)
}

fn title_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn separator_style(focus: Focus) -> Style {
    match focus {
        Focus::Main => Style::default().fg(Color::DarkGray),
        Focus::Sidebar => Style::default().fg(Color::Yellow),
    }
}

fn handle_overlay_key(
    key: &crossterm::event::KeyEvent,
    overlay: &mut Overlay,
    stream: &mut UnixStream,
    status_message: &mut Option<String>,
) -> bool {
    match key.code {
        KeyCode::Esc => {
            overlay.kind = OverlayKind::None;
            overlay.buffer.clear();
            true
        }
        KeyCode::Enter => {
            let buffer = overlay.buffer.trim().to_string();
            overlay.kind = OverlayKind::None;
            overlay.buffer.clear();
            if !buffer.is_empty() {
                let _ = writeln!(stream, "CONNECT_REMOTE_HOST {}", buffer);
                let _ = stream.flush();
                *status_message = Some(format!("connecting to {buffer}..."));
            }
            true
        }
        KeyCode::Backspace => {
            overlay.buffer.pop();
            true
        }
        KeyCode::Char(ch) => {
            overlay.buffer.push(ch);
            true
        }
        _ => true,
    }
}

fn render_connect_overlay(frame: &mut Frame, overlay: &Overlay, area: Rect) {
    let width = area.width.saturating_sub(8).min(60);
    let height = 3u16;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + area.height.saturating_sub(height + 2);
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title("Connect remote host")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let text = format!("Profile: {}", overlay.buffer);
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, popup);
}
