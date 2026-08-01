use crate::cli::{ConnectRemoteHostPaneCommand, RemoteNetworkConfig};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node::logical_key::LogicalKey;
use crate::runtime::ratatui_node_runtime::{
    ratatui_socket_path, ControlResponse, HistoryResponse, RatatuiSnapshot, ServerMessageJson,
    SessionView,
};
use crate::runtime::remote_host::connect_remote_host_pane_runtime::ConnectRemoteHostPaneRuntime;
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::{unbounded, Receiver};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
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
        let (server_tx, server_rx) = unbounded::<ServerMessage>();
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

        // Spawn a background reader for crossterm events so the main loop can
        // wait event-driven on both server messages and keyboard/resize events.
        let (crossterm_tx, crossterm_rx) = unbounded::<Event>();
        std::thread::spawn(move || loop {
            match event::read() {
                Ok(event) => {
                    if crossterm_tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
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
            crossterm_rx,
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

/// Compute the dimensions of the main pane that actually renders session
/// content. The footer always consumes one row; the sidebar, when visible,
/// consumes a separator plus a 32-column panel.
fn main_pane_size(cols: u16, rows: u16, sidebar_hidden: bool) -> (u16, u16) {
    let chrome_rows = rows.saturating_sub(1).max(1);
    if sidebar_hidden {
        (cols.max(1), chrome_rows)
    } else {
        (cols.saturating_sub(33).max(1), chrome_rows)
    }
}

/// Send a RESIZE command with the current main-pane size to the server.
fn send_main_pane_resize(stream: &mut UnixStream, sidebar_hidden: bool) {
    let raw_size = terminal_size();
    let (cols, rows) = match raw_size {
        Ok((cols, rows)) => main_pane_size(cols, rows, sidebar_hidden),
        Err(_) => (80, 24),
    };
    ERROR_LOG.log(format!(
        "[ratatui-client] send resize sidebar_hidden={sidebar_hidden} raw={raw_size:?} main_cols={cols} main_rows={rows}"
    ));
    let _ = writeln!(stream, "RESIZE {cols} {rows}");
    let _ = stream.flush();
}

fn run_connect_popup<F>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    port: u16,
    network: &RemoteNetworkConfig,
    render_background: F,
    crossterm_rx: &Receiver<Event>,
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
    runtime.run_embedded(terminal, command, render_background, crossterm_rx)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Main,
    Sidebar,
}

enum ServerMessage {
    Snapshot(RatatuiSnapshot),
    Response(ControlResponse),
    History(HistoryResponse),
    Log(String),
}

#[derive(Debug, Clone)]
struct HistoryState {
    styled_lines: Vec<String>,
    scroll_offset: usize,
}

#[derive(Debug, Clone)]
struct ErrorLogState {
    entries: Vec<(u128, String)>,
    scroll_offset: usize,
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
        if let Ok(ServerMessageJson::History(history)) = serde_json::from_str(trimmed) {
            return ServerMessage::History(history);
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

fn apply_server_message(
    snapshot: &mut RatatuiSnapshot,
    selected_index: &mut usize,
    last_active_target: &mut Option<String>,
    status_message: &mut Option<(String, Instant)>,
    history_state: &mut Option<HistoryState>,
    message: ServerMessage,
) {
    match message {
        ServerMessage::Snapshot(new_snapshot) => {
            *snapshot = new_snapshot;
            if *selected_index >= snapshot.sessions.len() && !snapshot.sessions.is_empty() {
                *selected_index = snapshot.sessions.len() - 1;
            }
            // When the server changes the active target (e.g. after a
            // remote host connects), move the selection marker to that
            // row so the sidebar stays consistent with the main pane.
            if snapshot.active_target != *last_active_target {
                *last_active_target = snapshot.active_target.clone();
                if let Some(target) = snapshot.active_target.as_deref() {
                    if let Some(idx) = snapshot.sessions.iter().position(|s| s.id == target) {
                        *selected_index = idx;
                    }
                }
            }
        }
        ServerMessage::Response(response) => {
            if !response.ok {
                if let Some(message) = response.message {
                    *status_message = Some((message, Instant::now()));
                }
            }
        }
        ServerMessage::History(history) => {
            *history_state = Some(HistoryState {
                styled_lines: history.styled_lines,
                scroll_offset: 0,
            });
        }
        ServerMessage::Log(text) => {
            ERROR_LOG.log(format!("[ratatui-client] server: {text}"));
            *status_message = Some((text, Instant::now()));
        }
    }
}

/// Handle a single crossterm event. Returns `true` to continue the loop,
/// `false` to break (e.g. user detached).
fn handle_crossterm_event(
    event: Event,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    stream: &mut UnixStream,
    snapshot: &mut RatatuiSnapshot,
    prefix_pressed: &mut bool,
    focus: &mut Focus,
    selected_index: &mut usize,
    sidebar_hidden: &mut bool,
    history_state: &mut Option<HistoryState>,
    error_log_state: &mut Option<ErrorLogState>,
    status_message: &mut Option<(String, Instant)>,
    port: u16,
    network: &RemoteNetworkConfig,
    crossterm_rx: &Receiver<Event>,
) -> Result<bool, LifecycleError> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            // Error-log popup takes precedence over other overlays.
            if error_log_state.is_some() {
                return handle_error_log_key(key, error_log_state);
            }

            // History mode consumes navigation keys until exited.
            if history_state.is_some() {
                return handle_history_key(key, history_state, status_message);
            }

            if *prefix_pressed {
                *prefix_pressed = false;
                match key.code {
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        let _ = writeln!(stream, "DETACH");
                        let _ = stream.flush();
                        return Ok(false);
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        *prefix_pressed = true;
                    }
                    KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        *sidebar_hidden = false;
                        *focus = Focus::Sidebar;
                        if *selected_index >= snapshot.sessions.len()
                            && !snapshot.sessions.is_empty()
                        {
                            *selected_index = snapshot.sessions.len() - 1;
                        }
                        send_main_pane_resize(stream, *sidebar_hidden);
                    }
                    KeyCode::Left if *focus == Focus::Sidebar => {
                        *focus = Focus::Main;
                    }
                    KeyCode::Up if *focus == Focus::Sidebar && *selected_index > 0 => {
                        *selected_index -= 1;
                    }
                    KeyCode::Down
                        if *focus == Focus::Sidebar
                            && *selected_index + 1 < snapshot.sessions.len() =>
                    {
                        *selected_index += 1;
                    }
                    KeyCode::Enter if *focus == Focus::Sidebar => {
                        if let Some(session) = snapshot.sessions.get(*selected_index) {
                            snapshot.active_target = Some(session.id.clone());
                            let _ = writeln!(stream, "ACTIVATE_TARGET {}", session.id);
                            let _ = stream.flush();
                            *focus = Focus::Main;
                            ERROR_LOG
                                .log(format!("[ratatui-client] activate session: {}", session.id));
                            send_main_pane_resize(stream, *sidebar_hidden);
                        }
                    }
                    KeyCode::Char('g') | KeyCode::Char('G')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        *sidebar_hidden = !*sidebar_hidden;
                        if !*sidebar_hidden {
                            *focus = Focus::Sidebar;
                            if *selected_index >= snapshot.sessions.len()
                                && !snapshot.sessions.is_empty()
                            {
                                *selected_index = snapshot.sessions.len() - 1;
                            }
                        }
                        send_main_pane_resize(stream, *sidebar_hidden);
                    }
                    KeyCode::Char('o') | KeyCode::Char('O')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        *error_log_state = None;
                        if let Some(target_id) = snapshot.active_target.as_deref() {
                            let _ = writeln!(stream, "GET_HISTORY {}", target_id);
                            let _ = stream.flush();
                        }
                    }
                    KeyCode::Char('e') | KeyCode::Char('E')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        *history_state = None;
                        *error_log_state = if error_log_state.is_some() {
                            None
                        } else {
                            Some(ErrorLogState {
                                entries: ERROR_LOG.entries(),
                                scroll_offset: 0,
                            })
                        };
                    }
                    KeyCode::Char('s') | KeyCode::Char('S')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        if let Some(session) = snapshot.sessions.get(*selected_index) {
                            if session.transport != "local" {
                                let _ = writeln!(
                                    stream,
                                    "CREATE_REMOTE_SESSION {}",
                                    session.authority_node_id
                                );
                                let _ = stream.flush();
                            } else {
                                *status_message = Some((
                                    "selected session is local; use Ctrl-N for a local session"
                                        .to_string(),
                                    Instant::now(),
                                ));
                            }
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
                                snapshot,
                                *focus,
                                *selected_index,
                                *sidebar_hidden,
                                None,
                                None,
                                snapshot.active_target.as_deref(),
                                status_message.as_ref().map(|(text, _)| text.as_str()),
                                true,
                            )
                        };
                        if let Err(error) = run_connect_popup(
                            terminal,
                            port,
                            network,
                            render_background,
                            crossterm_rx,
                        ) {
                            *status_message = Some((error.to_string(), Instant::now()));
                        }
                        // The popup may have consumed resize events; refresh the
                        // server with the current main-pane size.
                        send_main_pane_resize(stream, *sidebar_hidden);
                    }
                    _ if *focus == Focus::Main => {
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
            let (main_cols, main_rows) = main_pane_size(cols, rows, *sidebar_hidden);
            ERROR_LOG.log(format!(
                "[ratatui-client] event resize raw_cols={cols} raw_rows={rows} main_cols={main_cols} main_rows={main_rows}"
            ));
            let _ = writeln!(stream, "RESIZE {main_cols} {main_rows}");
            let _ = stream.flush();
        }
        _ => {}
    }
    Ok(true)
}

fn run_event_loop(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    stream: &mut UnixStream,
    mut snapshot: RatatuiSnapshot,
    server_rx: Receiver<ServerMessage>,
    crossterm_rx: Receiver<Event>,
    port: u16,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    let mut prefix_pressed = false;
    let mut focus = Focus::Main;
    let mut selected_index = 0usize;
    let mut sidebar_hidden = false;
    let mut history_state: Option<HistoryState> = None;
    let mut error_log_state: Option<ErrorLogState> = None;
    let mut last_active_target: Option<String> = None;
    let mut status_message: Option<(String, Instant)> = None;
    const STATUS_MESSAGE_DURATION: Duration = Duration::from_secs(3);

    // Tell the server the initial main-pane size so the PTY/observer screen
    // matches what we are about to draw.
    send_main_pane_resize(stream, sidebar_hidden);

    loop {
        // Wait event-driven for the next input from either source.
        crossbeam_channel::select! {
            recv(server_rx) -> result => {
                match result {
                    Ok(message) => apply_server_message(
                        &mut snapshot,
                        &mut selected_index,
                        &mut last_active_target,
                        &mut status_message,
                        &mut history_state,
                        message,
                    ),
                    Err(_) => {
                        // Server has shut down; the TUI has nothing left to render.
                        return Ok(());
                    }
                }
            }
            recv(crossterm_rx) -> result => {
                let event = match result {
                    Ok(event) => event,
                    Err(_) => continue,
                };
                if !handle_crossterm_event(
                    event,
                    &mut terminal,
                    stream,
                    &mut snapshot,
                    &mut prefix_pressed,
                    &mut focus,
                    &mut selected_index,
                    &mut sidebar_hidden,
                    &mut history_state,
                    &mut error_log_state,
                    &mut status_message,
                    port,
                    network,
                    &crossterm_rx,
                )? {
                    break;
                }
            }
        }

        // Drain any server-pushed snapshots that arrived while we processed.
        loop {
            match server_rx.try_recv() {
                Ok(message) => apply_server_message(
                    &mut snapshot,
                    &mut selected_index,
                    &mut last_active_target,
                    &mut status_message,
                    &mut history_state,
                    message,
                ),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Drain any crossterm events that arrived while we processed.
        loop {
            match crossterm_rx.try_recv() {
                Ok(event) => {
                    if !handle_crossterm_event(
                        event,
                        &mut terminal,
                        stream,
                        &mut snapshot,
                        &mut prefix_pressed,
                        &mut focus,
                        &mut selected_index,
                        &mut sidebar_hidden,
                        &mut history_state,
                        &mut error_log_state,
                        &mut status_message,
                        port,
                        network,
                        &crossterm_rx,
                    )? {
                        return Ok(());
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }

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
                    sidebar_hidden,
                    history_state.as_ref(),
                    error_log_state.as_ref(),
                    snapshot.active_target.as_deref(),
                    status_message.as_ref().map(|(text, _)| text.as_str()),
                    false,
                )
            })
            .map_err(|error| {
                LifecycleError::Io("failed to draw ratatui frame".to_string(), error)
            })?;
    }

    Ok(())
}

fn handle_history_key(
    key: KeyEvent,
    history_state: &mut Option<HistoryState>,
    _status_message: &mut Option<(String, Instant)>,
) -> Result<bool, LifecycleError> {
    let Some(state) = history_state.as_mut() else {
        return Ok(true);
    };

    match key.code {
        KeyCode::Char('o') | KeyCode::Char('O')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            *history_state = None;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            *history_state = None;
        }
        KeyCode::Up => {
            state.scroll_offset = state.scroll_offset.saturating_sub(1);
        }
        KeyCode::Down => {
            state.scroll_offset = state.scroll_offset.saturating_add(1);
        }
        KeyCode::PageUp => {
            state.scroll_offset = state.scroll_offset.saturating_sub(10);
        }
        KeyCode::PageDown => {
            state.scroll_offset = state.scroll_offset.saturating_add(10);
        }
        _ => {}
    }
    Ok(true)
}

fn handle_error_log_key(
    key: KeyEvent,
    error_log_state: &mut Option<ErrorLogState>,
) -> Result<bool, LifecycleError> {
    let Some(state) = error_log_state.as_mut() else {
        return Ok(true);
    };

    match key.code {
        KeyCode::Char('e') | KeyCode::Char('E')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            *error_log_state = None;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            *error_log_state = None;
        }
        KeyCode::Up => {
            state.scroll_offset = state.scroll_offset.saturating_sub(1);
        }
        KeyCode::Down => {
            state.scroll_offset = state.scroll_offset.saturating_add(1);
        }
        KeyCode::PageUp => {
            state.scroll_offset = state.scroll_offset.saturating_sub(10);
        }
        KeyCode::PageDown => {
            state.scroll_offset = state.scroll_offset.saturating_add(10);
        }
        _ => {}
    }
    Ok(true)
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
    sidebar_hidden: bool,
    history_state: Option<&HistoryState>,
    error_log_state: Option<&ErrorLogState>,
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

    if let Some(history) = history_state {
        render_history_view(frame, snapshot, history, outer[0]);
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
            Paragraph::new(render_history_footer_line(outer[1].width as usize)).style(footer_style)
        };
        frame.render_widget(footer, outer[1]);
        return;
    }

    // Inner horizontal layout: main pane left, optional separator, optional sidebar right.
    let inner = if sidebar_hidden {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0)])
            .split(outer[0])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(32),
            ])
            .split(outer[0])
    };
    ERROR_LOG.log(format!(
        "[ratatui-client] render frame={}x{} main_pane={}x{} sidebar_hidden={sidebar_hidden}",
        area.width, area.height, inner[0].width, inner[0].height
    ));

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

    if !sidebar_hidden {
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
    }

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
            sidebar_hidden,
            dim_background,
        ))
        .style(footer_style)
    };
    frame.render_widget(footer, outer[1]);

    if let Some(error_log) = error_log_state {
        render_error_log_popup(frame, error_log, outer[0]);
    }
}

fn render_history_view(
    frame: &mut Frame,
    _snapshot: &RatatuiSnapshot,
    history: &HistoryState,
    area: Rect,
) {
    let width = area.width as usize;
    let height = area.height as usize;
    let total_lines = history.styled_lines.len();
    let max_offset = total_lines.saturating_sub(height);
    let offset = history.scroll_offset.min(max_offset);

    let mut lines = Vec::new();
    for line in history.styled_lines.iter().skip(offset).take(height) {
        let spans: Vec<Span<'static>> = crate::terminal::parse_ansi_styled_line(line)
            .into_iter()
            .map(|(text, style)| Span::styled(text, text_style_to_ratatui(&style)))
            .collect();
        lines.push(truncate_spans_to_width(spans, width));
    }
    while lines.len() < height {
        lines.push(Line::from(""));
    }

    let block = Block::default()
        .borders(Borders::NONE)
        .style(Style::default());
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_history_footer_line(area_width: usize) -> Line<'static> {
    let text = "History  Ctrl-O/Esc Exit · PgUp/PgDn Page · Up/Down Line";
    let text_width = display_width(text);
    let fill = area_width.saturating_sub(text_width);
    Line::from(vec![
        Span::styled(text.to_string(), Style::default().fg(Color::Gray)),
        Span::styled(" ".repeat(fill), Style::default().fg(Color::Gray)),
    ])
}

fn render_error_log_popup(frame: &mut Frame, state: &ErrorLogState, area: Rect) {
    let popup = centered_rect(90, 85, area);

    // Dim the area behind the popup.
    let dim = Paragraph::new("").style(Style::default().bg(Color::Black));
    frame.render_widget(dim, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title("Error Log");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let width = inner.width as usize;
    let height = inner.height as usize;
    let total_lines = state.entries.len();
    let max_offset = total_lines.saturating_sub(height);
    let offset = state.scroll_offset.min(max_offset);

    let mut lines = Vec::new();
    for (_, message) in state.entries.iter().skip(offset).take(height) {
        lines.push(Line::from(truncate_display_width(message, width)));
    }
    while lines.len() < height {
        lines.push(Line::from(""));
    }

    let footer_text = format!(
        "{} lines · Ctrl-E/Esc/q close · Up/Down/PgUp/PgDn scroll",
        total_lines
    );
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        footer_text,
        Style::default().fg(Color::Gray),
    )]));

    let content_height = inner.height.saturating_sub(1);
    let content_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: content_height,
    };
    let footer_area = Rect {
        x: inner.x,
        y: inner.y + content_height,
        width: inner.width,
        height: 1,
    };

    let content = Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(content, content_area);
    frame.render_widget(footer, footer_area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn render_main_text(snapshot: &RatatuiSnapshot, area: Rect) -> Vec<Line<'static>> {
    let width = area.width as usize;
    let height = area.height as usize;
    let mut lines = Vec::new();

    for line in snapshot.main_styled_lines.iter() {
        let spans: Vec<Span<'static>> = crate::terminal::parse_ansi_styled_line(line)
            .into_iter()
            .map(|(text, style)| Span::styled(text, text_style_to_ratatui(&style)))
            .collect();
        lines.push(truncate_spans_to_width(spans, width));
    }

    while lines.len() < height {
        lines.push(Line::from(""));
    }

    lines
}

fn text_style_to_ratatui(style: &crate::terminal::TextStyle) -> Style {
    use ratatui::style::Modifier;

    let mut rat_style = Style::default();
    if style.bold {
        rat_style = rat_style.add_modifier(Modifier::BOLD);
    }
    if style.dim {
        rat_style = rat_style.add_modifier(Modifier::DIM);
    }
    if style.italic {
        rat_style = rat_style.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        rat_style = rat_style.add_modifier(Modifier::UNDERLINED);
    }
    if style.blink {
        rat_style = rat_style.add_modifier(Modifier::SLOW_BLINK);
    }
    if style.inverse {
        rat_style = rat_style.add_modifier(Modifier::REVERSED);
    }
    if style.strikethrough {
        rat_style = rat_style.add_modifier(Modifier::CROSSED_OUT);
    }
    if let Some(fg) = style.foreground {
        rat_style = rat_style.fg(color_value_to_ratatui(fg));
    }
    if let Some(bg) = style.background {
        rat_style = rat_style.bg(color_value_to_ratatui(bg));
    }
    rat_style
}

fn color_value_to_ratatui(color: crate::terminal::ColorValue) -> Color {
    match color {
        crate::terminal::ColorValue::Indexed(index) => Color::Indexed(index),
        crate::terminal::ColorValue::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
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

/// Truncate a collection of styled spans to a target display width.
fn truncate_spans_to_width(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut out_spans = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut truncated = String::new();
        for ch in span.content.chars() {
            let ch_width = char_width(ch);
            if used + ch_width > width {
                break;
            }
            truncated.push(ch);
            used += ch_width;
        }
        if !truncated.is_empty() {
            out_spans.push(Span::styled(truncated, span.style));
        }
        if used >= width {
            break;
        }
    }
    Line::from(out_spans)
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

fn render_footer_line(
    snapshot: &RatatuiSnapshot,
    area_width: usize,
    sidebar_hidden: bool,
    dim_background: bool,
) -> Line {
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

    let toggle_label = if sidebar_hidden { "Show" } else { "Hide" };
    spans.push(Span::styled(
        format!(
            "Ctrl-G {} · Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-O Hist · Ctrl-E Logs",
            toggle_label
        ),
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
