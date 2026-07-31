use crate::cli::{ConnectRemoteHostPaneCommand, RemoteNetworkConfig};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node::logical_key::LogicalKey;
use crate::runtime::ratatui_node_runtime::{
    ratatui_socket_path, ControlResponse, RatatuiSnapshot, ServerMessageJson, SessionView,
};
use crate::runtime::remote_host::connect_remote_host_pane_runtime::ConnectRemoteHostPaneRuntime;
use base64::{engine::general_purpose, Engine as _};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Ratatui TUI client: connects to a server's node and renders the workspace chrome.
pub struct RatatuiClientRuntime {
    port: u16,
    network: RemoteNetworkConfig,
}

impl RatatuiClientRuntime {
    pub fn from_port(port: u16, network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        Ok(Self { port, network })
    }

    pub fn run(&self) -> Result<(), LifecycleError> {
        let socket_path = ratatui_socket_path(self.port);
        ERROR_LOG.log(format!(
            "[ratatui-client] connecting to socket={} port={}",
            socket_path.display(),
            self.port
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

        writeln!(stream, "ATTACH").map_err(|error| {
            LifecycleError::Io(
                "failed to send attach command to ratatui node".to_string(),
                error,
            )
        })?;
        stream.flush().map_err(|error| {
            LifecycleError::Io(
                "failed to flush attach command to ratatui node".to_string(),
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
            Ok(_) => match parse_server_message(&line) {
                ServerMessage::Snapshot(snapshot) => snapshot,
                _ => RatatuiSnapshot::default(),
            },
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
                        let message = parse_server_message(trimmed);
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
        let result = run_event_loop(
            terminal,
            &mut stream,
            snapshot,
            server_rx,
            self.port,
            &self.network,
        );
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

fn run_connect_popup<F>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    port: u16,
    network: &RemoteNetworkConfig,
    render_background: F,
) -> Result<(), LifecycleError>
where
    F: FnMut(&mut Frame),
{
    let socket_path = ratatui_socket_path(port);
    let runtime = ConnectRemoteHostPaneRuntime::new(network.clone())
        .with_ratatui_port(port)
        .with_ratatui_socket_path(socket_path);
    let command = ConnectRemoteHostPaneCommand {
        current_socket_name: String::new(),
        current_session_name: "1".to_string(),
    };
    runtime.run_embedded(terminal, command, render_background)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Main,
    Sidebar,
}

enum ServerMessage {
    Snapshot(RatatuiSnapshot),
    Response(ControlResponse),
    Log(String),
}

fn parse_server_message(line: &str) -> ServerMessage {
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Ok(ServerMessageJson::Response(response)) = serde_json::from_str(trimmed) {
            return ServerMessage::Response(response);
        }
        if let Ok(ServerMessageJson::Snapshot(snapshot)) = serde_json::from_str(trimmed) {
            return ServerMessage::Snapshot(snapshot);
        }
    }
    ServerMessage::Log(trimmed.to_string())
}

/// Apply a dim modifier to a style when the background should be muted behind a
/// modal popup.
fn dim_style(style: Style, dim: bool) -> Style {
    if dim {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

fn run_event_loop(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    stream: &mut UnixStream,
    mut snapshot: RatatuiSnapshot,
    server_rx: Receiver<ServerMessage>,
    port: u16,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    let mut prefix_pressed = false;
    let mut focus = Focus::Main;
    let mut selected_index = 0usize;
    let mut last_active_target: Option<String> = None;
    let mut status_message: Option<(String, Instant)> = None;
    const STATUS_MESSAGE_DURATION: Duration = Duration::from_secs(3);

    loop {
        // Clear expired status messages before drawing so the footer menu
        // is not hidden indefinitely.
        if let Some((_, created_at)) = status_message.as_ref() {
            if created_at.elapsed() >= STATUS_MESSAGE_DURATION {
                status_message = None;
            }
        }

        terminal
            .draw(|frame| {
                render(
                    frame,
                    &snapshot,
                    focus,
                    selected_index,
                    snapshot.active_target.as_deref(),
                    status_message.as_ref().map(|(text, _)| text.as_str()),
                    false,
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
                    // When the server changes the active target (e.g. after a
                    // remote host connects), move the selection marker to that
                    // row so the sidebar stays consistent with the main pane.
                    if snapshot.active_target != last_active_target {
                        last_active_target = snapshot.active_target.clone();
                        if let Some(target) = snapshot.active_target.as_deref() {
                            if let Some(idx) = snapshot.sessions.iter().position(|s| s.id == target)
                            {
                                selected_index = idx;
                            }
                        }
                    }
                }
                Ok(ServerMessage::Response(response)) => {
                    if !response.ok {
                        if let Some(message) = response.message {
                            status_message = Some((message, Instant::now()));
                        }
                    }
                }
                Ok(ServerMessage::Log(text)) => {
                    ERROR_LOG.log(format!("[ratatui-client] server: {text}"));
                    status_message = Some((text, Instant::now()));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Server has shut down; the TUI has nothing left to render.
                    return Ok(());
                }
            }
        }

        if event::poll(Duration::from_millis(50)).map_err(|error| {
            LifecycleError::Io("failed to poll crossterm events".to_string(), error)
        })? {
            match event::read().map_err(|error| {
                LifecycleError::Io("failed to read crossterm event".to_string(), error)
            })? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
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
                                    focus = Focus::Main;
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
                                let render_background = |frame: &mut Frame| {
                                    render(
                                        frame,
                                        &snapshot,
                                        focus,
                                        selected_index,
                                        snapshot.active_target.as_deref(),
                                        status_message.as_ref().map(|(text, _)| text.as_str()),
                                        true,
                                    );
                                };
                                if let Err(error) = run_connect_popup(
                                    &mut terminal,
                                    port,
                                    network,
                                    render_background,
                                ) {
                                    status_message = Some((error.to_string(), Instant::now()));
                                }
                            }
                            _ if focus == Focus::Main => {
                                if let Some(logical_key) = key_event_to_logical_key(&key) {
                                    if let Some(target_id) = snapshot.active_target.as_deref() {
                                        let encoded =
                                            general_purpose::STANDARD.encode(logical_key.to_json());
                                        let _ = writeln!(stream, "INPUT {target_id} {encoded}");
                                        let _ = stream.flush();
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Event::Resize(cols, rows) => {
                    let _ = writeln!(stream, "RESIZE {cols} {rows}");
                    let _ = stream.flush();
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn key_event_to_logical_key(key: &KeyEvent) -> Option<LogicalKey> {
    // Ignore key-release events; crossterm already filters by KeyEventKind::Press
    // in the caller, so any key reaching here is a press we want to forward.
    Some(LogicalKey::from(key))
}

fn render(
    frame: &mut Frame,
    snapshot: &RatatuiSnapshot,
    focus: Focus,
    selected_index: usize,
    active_target: Option<&str>,
    status_message: Option<&str>,
    dim_background: bool,
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
        .borders(Borders::NONE)
        .style(dim_style(Style::default(), dim_background));
    let main_text = render_main_text(snapshot, inner[0]);
    let main = Paragraph::new(main_text).block(main_block);
    frame.render_widget(main, inner[0]);

    // Draw cursor if the active local session provided a cursor position.
    if focus == Focus::Main {
        if let Some((col, row)) = snapshot.main_cursor {
            let cursor_x = inner[0].x + col;
            let cursor_y = inner[0].y + row;
            if inner[0].contains(ratatui::layout::Position::new(cursor_x, cursor_y)) {
                frame
                    .buffer_mut()
                    .get_mut(cursor_x, cursor_y)
                    .set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }

    let separator = Block::default()
        .borders(Borders::LEFT)
        .border_style(dim_style(separator_style(focus), dim_background));
    frame.render_widget(separator, inner[1]);

    let sidebar_block = Block::default()
        .borders(Borders::NONE)
        .title_style(title_style(focus == Focus::Sidebar))
        .style(dim_style(Style::default(), dim_background));
    let sidebar = Paragraph::new(render_sidebar_lines(
        &snapshot.sessions,
        selected_index,
        inner[2],
        focus == Focus::Sidebar,
        active_target,
        dim_background,
    ))
    .block(sidebar_block);
    frame.render_widget(sidebar, inner[2]);

    let footer = if let Some(status) = status_message {
        let style = dim_style(
            Style::default().bg(Color::Yellow).fg(Color::Black),
            dim_background,
        );
        Paragraph::new(pad_right(status, outer[1].width as usize)).style(style)
    } else {
        let footer_style = dim_style(
            Style::default().bg(Color::Blue).fg(Color::White),
            dim_background,
        );
        Paragraph::new(render_footer_line(
            snapshot,
            outer[1].width as usize,
            dim_background,
        ))
        .style(footer_style)
    };
    frame.render_widget(footer, outer[1]);
}

fn render_main_text<'a>(snapshot: &'a RatatuiSnapshot, area: Rect) -> Vec<Line<'a>> {
    let width = area.width as usize;
    let height = area.height as usize;
    let mut lines = Vec::new();

    for line in snapshot.main_lines.iter() {
        lines.push(Line::from(truncate_display_width(line, width)));
    }

    while lines.len() < height {
        lines.push(Line::from(""));
    }

    lines
}

fn render_sidebar_header(width: usize, dim_background: bool) -> Line<'static> {
    let title = "Sessions";
    let hint = "Ctrl-G hide";
    let title_width = display_width(title);
    let hint_width = display_width(hint);
    let padding = width.saturating_sub(title_width + hint_width);

    let bg = Style::default().bg(Color::Black);
    Line::from(vec![
        Span::styled(
            title.to_string(),
            dim_style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
                    .patch(bg),
                dim_background,
            ),
        ),
        Span::styled(" ".repeat(padding), dim_style(bg, dim_background)),
        Span::styled(
            hint.to_string(),
            dim_style(Style::default().fg(Color::Gray).patch(bg), dim_background),
        ),
    ])
}

fn render_sidebar_lines<'a>(
    sessions: &'a [SessionView],
    selected_index: usize,
    area: Rect,
    is_focused: bool,
    active_target: Option<&'a str>,
    dim_background: bool,
) -> Vec<Line<'a>> {
    let width = area.width as usize;
    let height = area.height as usize;
    let mut lines = Vec::new();

    // Header.
    lines.push(render_sidebar_header(width, dim_background));

    // Separator.
    lines.push(Line::from(vec![Span::styled(
        "─".repeat(width),
        dim_style(Style::default().fg(Color::DarkGray), dim_background),
    )]));

    if sessions.is_empty() {
        while lines.len() + 2 < height {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![Span::styled(
            "─".repeat(width),
            dim_style(Style::default().fg(Color::DarkGray), dim_background),
        )]));
        lines.push(Line::from(vec![Span::styled(
            right_align("(no sessions)", width),
            dim_style(Style::default().fg(Color::Gray), dim_background),
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
            dim_background,
        ));
    }

    while lines.len() + 2 < height {
        lines.push(Line::from(""));
    }

    // Bottom separator and detail line.
    lines.push(Line::from(vec![Span::styled(
        "─".repeat(width),
        dim_style(Style::default().fg(Color::DarkGray), dim_background),
    )]));
    lines.push(Line::from(vec![Span::styled(
        right_align(&selected_detail_text(&selected, width), width),
        dim_style(Style::default().fg(Color::Gray), dim_background),
    )]));

    lines
}

fn render_session_row(
    session: &SessionView,
    is_selected: bool,
    is_current: bool,
    width: usize,
    is_focused: bool,
    dim_background: bool,
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

    let base_style = dim_style(
        if is_selected && is_focused {
            Style::default().bg(Color::Blue).fg(Color::White)
        } else {
            Style::default()
        },
        dim_background,
    );

    Line::from(vec![
        Span::styled(format!("{} {}", marker, label), base_style),
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

fn render_footer_line(snapshot: &RatatuiSnapshot, area_width: usize, dim_background: bool) -> Line {
    let muted_style = dim_style(Style::default().fg(Color::Gray), dim_background);
    let accent_style = dim_style(Style::default().fg(Color::White), dim_background);

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
