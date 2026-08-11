use crate::cli::{ConnectRemoteHostPaneCommand, RemoteNetworkConfig};
use crate::host::ssh::connect_remote_host_pane_runtime::ConnectRemoteHostPaneRuntime;
use crate::infra::error_log::{LogLevel, ERROR_LOG};
use crate::infra::settings_store::SettingsStore;
use crate::lifecycle::LifecycleError;
use crate::ratatui_node::clipboard_classifier::{classify_text, ClipboardContent};
use crate::ratatui_node::clipboard_paste::{
    dispatch_paste, run_paste_job, PasteAction, PasteContext, PasteJob, PasteJobResult,
};
use crate::ratatui_node::clipboard_platform::PlatformContext;
use crate::ratatui_node::clipboard_reader::{read_clipboard, ClipboardReadResult};
use crate::ratatui_node::logical_key::KeyCode as LogicalKeyCode;
use crate::ratatui_node::logical_key::KeyModifiers as LogicalKeyModifiers;
use crate::ratatui_node::logical_key::LogicalKey;
use crate::ratatui_node::node_runtime::{
    ratatui_socket_path, ControlResponse, HistoryResponse, RatatuiSnapshot, ServerMessageJson,
    SessionView,
};
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::{unbounded, Receiver};
use crossterm::cursor::{Hide, MoveTo};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Ratatui TUI client: connects to a server's node and renders the workspace chrome.
pub struct RatatuiClientRuntime {
    port: u16,
    network: RemoteNetworkConfig,
    settings_store: SettingsStore,
}

impl RatatuiClientRuntime {
    pub fn from_port(
        port: u16,
        network: RemoteNetworkConfig,
        settings_store: SettingsStore,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            port,
            network,
            settings_store,
        })
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
                ServerMessage::Snapshot(snapshot) => *snapshot,
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
        std::thread::spawn(move || {
            while let Ok(event) = event::read() {
                if crossterm_tx.send(event).is_err() {
                    break;
                }
            }
        });

        // Channel for asynchronous clipboard reads initiated by Ctrl+V / Shift+Insert.
        let (clipboard_tx, clipboard_rx) = unbounded::<Result<ClipboardReadResult, String>>();

        // Channel for paste jobs that need blocking file I/O or base64 encoding.
        let (paste_job_tx, paste_job_rx) = unbounded::<PasteJob>();
        let (paste_result_tx, paste_result_rx) = unbounded::<PasteJobResult>();

        // Spawn a single clipboard worker thread that handles all blocking paste
        // operations (file reads, temp-file writes, base64 encoding).
        let worker_platform = PlatformContext::detect();
        let worker_result_tx = paste_result_tx.clone();
        std::thread::spawn(move || {
            while let Ok(job) = paste_job_rx.recv() {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_paste_job(&worker_platform, job)
                }))
                .unwrap_or_else(|_| PasteJobResult::Error("clipboard worker panicked".to_string()));
                if worker_result_tx.send(result).is_err() {
                    break;
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
            crossterm_rx,
            clipboard_rx,
            clipboard_tx,
            paste_job_tx,
            paste_result_rx,
            self.port,
            &self.network,
            &self.settings_store,
        );
        let _ = restore_terminal();

        result
    }
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    // Ignore errors during cleanup so a partially restored terminal still
    // leaves raw mode and the alternate screen.
    let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
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

/// Send a PASTE_TEXT command with the given text to the server.
fn send_paste_text(stream: &mut UnixStream, target_id: &str, text: &str) {
    let encoded = general_purpose::STANDARD.encode(text.as_bytes());
    let _ = writeln!(stream, "PASTE_TEXT {target_id} {encoded}");
    let _ = stream.flush();
}

/// Apply a paste action produced by the dispatcher.
///
/// Immediate text pastes are sent directly to the server; file jobs are
/// enqueued on the clipboard worker thread.
fn apply_paste_action(
    action: PasteAction,
    stream: &mut UnixStream,
    paste_job_tx: &crossbeam_channel::Sender<PasteJob>,
    status_message: &mut Option<(String, Instant)>,
) {
    match action {
        PasteAction::SendText { target_id, text } => {
            send_paste_text(stream, &target_id, &text);
        }
        PasteAction::RunJob(job) => {
            if paste_job_tx.send(job).is_err() {
                *status_message =
                    Some(("clipboard worker disconnected".to_string(), Instant::now()));
            }
        }
        PasteAction::Error(message) => {
            *status_message = Some((message, Instant::now()));
        }
        PasteAction::Nothing => {}
    }
}

/// Apply the result of a background paste job.
fn apply_paste_job_result(
    result: PasteJobResult,
    stream: &mut UnixStream,
    status_message: &mut Option<(String, Instant)>,
) {
    match result {
        PasteJobResult::PasteText { target_id, text } => {
            send_paste_text(stream, &target_id, &text);
        }
        PasteJobResult::PasteFile {
            target_id,
            filename_hint,
            base64,
        } => {
            let _ = writeln!(stream, "PASTE_FILE {target_id} {filename_hint} {base64}");
            let _ = stream.flush();
        }
        PasteJobResult::Error(message) => {
            *status_message = Some((message, Instant::now()));
        }
    }
}

/// Spawn a background thread that reads the system clipboard, then sends the
/// raw result back on the supplied channel.
fn spawn_clipboard_read(tx: crossbeam_channel::Sender<Result<ClipboardReadResult, String>>) {
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(read_clipboard)
            .unwrap_or_else(|_| Err("clipboard read panicked".to_string()));
        let _ = tx.send(result);
    });
}

/// Send a RESIZE command with the current main-pane size to the server.
fn send_main_pane_resize(stream: &mut UnixStream, sidebar_hidden: bool) {
    let raw_size = terminal_size();
    let (cols, rows) = match raw_size {
        Ok((cols, rows)) => main_pane_size(cols, rows, sidebar_hidden),
        Err(_) => (80, 24),
    };
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
    Snapshot(Box<RatatuiSnapshot>),
    Response(ControlResponse),
    History(HistoryResponse),
    Log(String),
}

#[derive(Debug, Clone)]
struct HistoryState {
    styled_lines: Vec<String>,
    /// Absolute line index of the cursor within the full history buffer.
    /// The viewport scrolls to keep this line visible.
    cursor_line: usize,
    /// Cursor column preserved while scrolling up/down through history.
    cursor_col: u16,
}

#[derive(Debug, Clone)]
struct ErrorLogState {
    entries: Vec<(u128, LogLevel, String)>,
    scroll_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsFocus {
    Input,
    History,
    SaveCheckbox,
    ApplyButton,
    ClearButton,
    CancelButton,
}

#[derive(Debug, Clone)]
struct SettingsState {
    input: String,
    selected_history: usize,
    save_persist: bool,
    focus: SettingsFocus,
    history: Vec<String>,
}

impl SettingsState {
    fn new(snapshot: &RatatuiSnapshot, history: Vec<String>) -> Self {
        let input = snapshot
            .footer
            .public_endpoint
            .as_deref()
            .unwrap_or("")
            .to_string();
        Self {
            input,
            selected_history: 0,
            save_persist: false,
            focus: SettingsFocus::Input,
            history,
        }
    }
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

#[allow(clippy::too_many_arguments)]
fn apply_server_message(
    snapshot: &mut RatatuiSnapshot,
    selected_index: &mut usize,
    last_active_target: &mut Option<String>,
    status_message: &mut Option<(String, Instant)>,
    history_state: &mut Option<HistoryState>,
    focus: Focus,
    message: ServerMessage,
) {
    match message {
        ServerMessage::Snapshot(new_snapshot) => {
            let active_changed = new_snapshot.active_target != *last_active_target;
            *snapshot = *new_snapshot;
            if *selected_index >= snapshot.sessions.len() && !snapshot.sessions.is_empty() {
                *selected_index = snapshot.sessions.len() - 1;
            }
            if active_changed {
                *last_active_target = snapshot.active_target.clone();
                // History is tied to the previously active session; exit history
                // mode so the main pane shows the new session instead of stale
                // scrollback.
                *history_state = None;
            }
            // When focus is in the main pane, keep the sidebar marker aligned
            // with the active session. When focus is in the sidebar, leave the
            // user's selection alone so arrow-key navigation does not jump.
            if focus == Focus::Main {
                if let Some(target) = snapshot.active_target.as_deref() {
                    let current_id = snapshot
                        .sessions
                        .get(*selected_index)
                        .map(|s| s.id.as_str());
                    if current_id != Some(target) {
                        if let Some(idx) = snapshot.sessions.iter().position(|s| s.id == target) {
                            *selected_index = idx;
                        }
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
            // Place the history cursor at the same position as the live terminal
            // cursor so the user continues from where they pressed Ctrl+O.
            let total = history.styled_lines.len();
            let (cursor_line, cursor_col) = if let Some((col, row)) = snapshot.main_cursor {
                let screen_rows = snapshot.main_lines.len().max(1);
                let absolute_line = total.saturating_sub(screen_rows)
                    + (row as usize).min(screen_rows.saturating_sub(1));
                (absolute_line.min(total.saturating_sub(1)), col)
            } else {
                (total.saturating_sub(1), 0)
            };
            *history_state = Some(HistoryState {
                styled_lines: history.styled_lines,
                cursor_line,
                cursor_col,
            });
        }
        ServerMessage::Log(text) => {
            *status_message = Some((text, Instant::now()));
        }
    }
}

/// Send the agent-specific session-management slash command for the active
/// target. Each agent is responsible for its own session picker / resume UI.
fn send_agent_session_command(
    stream: &mut UnixStream,
    snapshot: &RatatuiSnapshot,
    status_message: &mut Option<(String, Instant)>,
) {
    let Some(target_id) = snapshot.active_target.as_deref() else {
        *status_message = Some(("no active session".to_string(), Instant::now()));
        return;
    };
    let Some(session) = snapshot.sessions.iter().find(|s| s.id == target_id) else {
        *status_message = Some(("active session not found".to_string(), Instant::now()));
        return;
    };
    let Some(agent) = session.agent_command_name.as_deref() else {
        *status_message = Some((
            format!("no session shortcut for {}", session.command_name),
            Instant::now(),
        ));
        return;
    };

    let command = match agent {
        "kimi" => "/sessions",
        "codex" => "/resume",
        "claude" => "/resume",
        _ => {
            *status_message = Some((format!("no session shortcut for {agent}"), Instant::now()));
            return;
        }
    };

    send_paste_text(stream, target_id, command);
    let enter_key = LogicalKey {
        code: LogicalKeyCode::Enter,
        modifiers: LogicalKeyModifiers::default(),
    };
    let encoded = general_purpose::STANDARD.encode(enter_key.to_json());
    let _ = writeln!(stream, "INPUT {target_id} {encoded}");
    let _ = stream.flush();
}

/// Handle a single crossterm event. Returns `true` to continue the loop,
struct HandleCrosstermEventArgs<'a> {
    terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>,
    stream: &'a mut UnixStream,
    snapshot: &'a mut RatatuiSnapshot,
    prefix_pressed: &'a mut bool,
    focus: &'a mut Focus,
    selected_index: &'a mut usize,
    sidebar_hidden: &'a mut bool,
    history_state: &'a mut Option<HistoryState>,
    error_log_state: &'a mut Option<ErrorLogState>,
    settings_state: &'a mut Option<SettingsState>,
    status_message: &'a mut Option<(String, Instant)>,
    port: u16,
    network: &'a RemoteNetworkConfig,
    settings_store: &'a SettingsStore,
    crossterm_rx: &'a Receiver<Event>,
    clipboard_tx: &'a crossbeam_channel::Sender<Result<ClipboardReadResult, String>>,
    paste_job_tx: &'a crossbeam_channel::Sender<PasteJob>,
}

/// `false` to break (e.g. user detached).
fn handle_crossterm_event(
    event: Event,
    args: HandleCrosstermEventArgs<'_>,
) -> Result<bool, LifecycleError> {
    let HandleCrosstermEventArgs {
        terminal,
        stream,
        snapshot,
        prefix_pressed,
        focus,
        selected_index,
        sidebar_hidden,
        history_state,
        error_log_state,
        settings_state,
        status_message,
        port,
        network,
        settings_store,
        crossterm_rx,
        clipboard_tx,
        paste_job_tx,
    } = args;
    match event {
        Event::Paste(text) => {
            if *focus == Focus::Main
                && error_log_state.is_none()
                && settings_state.is_none()
                && history_state.is_none()
            {
                if text.trim().is_empty() {
                    // The terminal sent an empty bracketed-paste event. This
                    // can happen when the clipboard contains binary content
                    // (e.g. a screenshot) that cannot be represented as text.
                    // Fall back to reading the system clipboard so we can
                    // still handle files and images.
                    spawn_clipboard_read(clipboard_tx.clone());
                } else {
                    // Fast path: the terminal already gave us the pasted text
                    // via bracketed-paste mode. Classify it locally and
                    // dispatch without touching the system clipboard API.
                    let platform = PlatformContext::detect();
                    let ctx = PasteContext { platform, snapshot };
                    let content = classify_text(&text);
                    apply_paste_action(
                        dispatch_paste(content, &ctx),
                        stream,
                        paste_job_tx,
                        status_message,
                    );
                }
            }
        }
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            // Error-log popup takes precedence over other overlays.
            if error_log_state.is_some() {
                return handle_error_log_key(key, error_log_state);
            }

            // Settings popup takes precedence over history and main input.
            if settings_state.is_some() {
                return handle_settings_key(key, settings_state, stream, snapshot, status_message);
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
                    KeyCode::Char('v') | KeyCode::Char('V')
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && *focus == Focus::Main
                            && error_log_state.is_none()
                            && settings_state.is_none()
                            && history_state.is_none() =>
                    {
                        spawn_clipboard_read(clipboard_tx.clone());
                    }
                    KeyCode::Insert
                        if key.modifiers.contains(KeyModifiers::SHIFT)
                            && *focus == Focus::Main
                            && error_log_state.is_none()
                            && settings_state.is_none()
                            && history_state.is_none() =>
                    {
                        spawn_clipboard_read(clipboard_tx.clone());
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
                            ERROR_LOG
                                .log(format!("[timing] client ACTIVATE_TARGET {}", session.id));
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
                    KeyCode::Char('h') | KeyCode::Char('H')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        send_agent_session_command(stream, snapshot, status_message);
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
                                entries: ERROR_LOG.recent_entries(10_000),
                                // Start at the bottom so the most recent log
                                // lines are visible immediately.
                                scroll_offset: usize::MAX,
                            })
                        };
                    }
                    KeyCode::Char('p') | KeyCode::Char('P')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        *history_state = None;
                        *error_log_state = None;
                        if settings_state.is_some() {
                            *settings_state = None;
                        } else {
                            let history = settings_store.public_history().unwrap_or_default();
                            *settings_state = Some(SettingsState::new(snapshot, history));
                        }
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
                            let mut cursor_state = None;
                            render(
                                frame,
                                RenderArgs {
                                    snapshot,
                                    focus: *focus,
                                    selected_index: *selected_index,
                                    sidebar_hidden: *sidebar_hidden,
                                    history_state: None,
                                    error_log_state: None,
                                    settings_state: None,
                                    active_target: snapshot.active_target.as_deref(),
                                    status_message: status_message
                                        .as_ref()
                                        .map(|(text, _)| text.as_str()),
                                    dim_background: true,
                                    cursor_state: &mut cursor_state,
                                },
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

#[allow(clippy::too_many_arguments)]
fn run_event_loop(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    stream: &mut UnixStream,
    mut snapshot: RatatuiSnapshot,
    server_rx: Receiver<ServerMessage>,
    crossterm_rx: Receiver<Event>,
    clipboard_rx: Receiver<Result<ClipboardReadResult, String>>,
    clipboard_tx: crossbeam_channel::Sender<Result<ClipboardReadResult, String>>,
    paste_job_tx: crossbeam_channel::Sender<PasteJob>,
    paste_result_rx: Receiver<PasteJobResult>,
    port: u16,
    network: &RemoteNetworkConfig,
    settings_store: &SettingsStore,
) -> Result<(), LifecycleError> {
    let mut prefix_pressed = false;
    let mut focus = Focus::Main;
    let mut selected_index = 0usize;
    let mut sidebar_hidden = false;
    let mut history_state: Option<HistoryState> = None;
    let mut error_log_state: Option<ErrorLogState> = None;
    let mut settings_state: Option<SettingsState> = None;
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
                        focus,
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
                    HandleCrosstermEventArgs {
                        terminal: &mut terminal,
                        stream,
                        snapshot: &mut snapshot,
                        prefix_pressed: &mut prefix_pressed,
                        focus: &mut focus,
                        selected_index: &mut selected_index,
                        sidebar_hidden: &mut sidebar_hidden,
                        history_state: &mut history_state,
                        error_log_state: &mut error_log_state,
                        settings_state: &mut settings_state,
                        status_message: &mut status_message,
                        port,
                        network,
                        settings_store,
                        crossterm_rx: &crossterm_rx,
                        clipboard_tx: &clipboard_tx,
                        paste_job_tx: &paste_job_tx,
                    },
                )? {
                    break;
                }
            }
            recv(clipboard_rx) -> result => {
                let platform = PlatformContext::detect();
                let ctx = PasteContext {
                    platform,
                    snapshot: &snapshot,
                };
                match result {
                    Ok(Ok(read_result)) => {
                        let content = match read_result {
                            ClipboardReadResult::Text(text) => Some(classify_text(&text)),
                            ClipboardReadResult::Image { filename_hint, bytes } => {
                                Some(ClipboardContent::BinaryFile { filename_hint, bytes })
                            }
                            ClipboardReadResult::Empty => {
                                status_message = Some((
                                    "clipboard is empty".to_string(),
                                    Instant::now(),
                                ));
                                None
                            }
                        };
                        if let Some(content) = content {
                            apply_paste_action(
                                dispatch_paste(content, &ctx),
                                stream,
                                &paste_job_tx,
                                &mut status_message,
                            );
                        }
                    }
                    Ok(Err(error)) => {
                        status_message = Some((format!("clipboard error: {error}"), Instant::now()));
                    }
                    Err(_) => {}
                }
            }
            recv(paste_result_rx) -> result => {
                if let Ok(job_result) = result {
                    apply_paste_job_result(job_result, stream, &mut status_message);
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
                    focus,
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
                        HandleCrosstermEventArgs {
                            terminal: &mut terminal,
                            stream,
                            snapshot: &mut snapshot,
                            prefix_pressed: &mut prefix_pressed,
                            focus: &mut focus,
                            selected_index: &mut selected_index,
                            sidebar_hidden: &mut sidebar_hidden,
                            history_state: &mut history_state,
                            error_log_state: &mut error_log_state,
                            settings_state: &mut settings_state,
                            status_message: &mut status_message,
                            port,
                            network,
                            settings_store,
                            crossterm_rx: &crossterm_rx,
                            clipboard_tx: &clipboard_tx,
                            paste_job_tx: &paste_job_tx,
                        },
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

        let dim_background = error_log_state.is_some() || settings_state.is_some();
        let mut cursor_state = None;
        terminal
            .draw(|frame| {
                render(
                    frame,
                    RenderArgs {
                        snapshot: &snapshot,
                        focus,
                        selected_index,
                        sidebar_hidden,
                        history_state: history_state.as_ref(),
                        error_log_state: error_log_state.as_mut(),
                        settings_state: settings_state.as_ref(),
                        active_target: snapshot.active_target.as_deref(),
                        status_message: status_message.as_ref().map(|(text, _)| text.as_str()),
                        dim_background,
                        cursor_state: &mut cursor_state,
                    },
                )
            })
            .map_err(|error| {
                LifecycleError::Io("failed to draw ratatui frame".to_string(), error)
            })?;

        // When the PTY hides its cursor, ratatui will hide the hardware cursor
        // and may leave it at an arbitrary cell. The IME candidate window still
        // needs the correct anchor, so move the hidden cursor to the active
        // input position without showing it.
        if focus == Focus::Main {
            if let Some(ImeCursorState {
                area,
                cursor: Some((col, row)),
                cursor_visible: false,
            }) = cursor_state
            {
                let cursor_x = area.x + col;
                let cursor_y = area.y + row;
                if area.contains(ratatui::layout::Position::new(cursor_x, cursor_y)) {
                    let _ = execute!(terminal.backend_mut(), MoveTo(cursor_x, cursor_y), Hide,);
                }
            }
        }
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
    let total = state.styled_lines.len();
    let max_line = total.saturating_sub(1);
    const PAGE_SIZE: usize = 10;

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
            state.cursor_line = state.cursor_line.saturating_sub(1);
        }
        KeyCode::Down => {
            state.cursor_line = (state.cursor_line + 1).min(max_line);
        }
        KeyCode::PageUp => {
            state.cursor_line = state.cursor_line.saturating_sub(PAGE_SIZE);
        }
        KeyCode::PageDown => {
            state.cursor_line = (state.cursor_line + PAGE_SIZE).min(max_line);
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
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
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
        KeyCode::Home | KeyCode::Char('g') => {
            state.scroll_offset = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            state.scroll_offset = usize::MAX;
        }
        _ => {}
    }
    Ok(true)
}

fn handle_settings_key(
    key: KeyEvent,
    settings_state: &mut Option<SettingsState>,
    stream: &mut UnixStream,
    _snapshot: &RatatuiSnapshot,
    status_message: &mut Option<(String, Instant)>,
) -> Result<bool, LifecycleError> {
    let Some(state) = settings_state.as_mut() else {
        return Ok(true);
    };

    match key.code {
        KeyCode::Char('p') | KeyCode::Char('P')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            *settings_state = None;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            *settings_state = None;
        }
        KeyCode::Tab => {
            state.focus = next_settings_focus(state.focus);
            if state.focus == SettingsFocus::History && state.history.is_empty() {
                state.focus = next_settings_focus(state.focus);
            }
        }
        KeyCode::BackTab => {
            state.focus = prev_settings_focus(state.focus);
            if state.focus == SettingsFocus::History && state.history.is_empty() {
                state.focus = prev_settings_focus(state.focus);
            }
        }
        KeyCode::Enter => match state.focus {
            SettingsFocus::ApplyButton => {
                apply_settings(state, stream, status_message)?;
                *settings_state = None;
            }
            SettingsFocus::ClearButton => {
                let save = state.save_persist;
                send_set_public(stream, None, save)?;
                *settings_state = None;
            }
            SettingsFocus::CancelButton => {
                *settings_state = None;
            }
            SettingsFocus::SaveCheckbox => {
                state.save_persist = !state.save_persist;
            }
            SettingsFocus::History => {
                if let Some(value) = state.history.get(state.selected_history) {
                    state.input = value.clone();
                }
            }
            SettingsFocus::Input => {}
        },
        KeyCode::Char(' ') if state.focus == SettingsFocus::SaveCheckbox => {
            state.save_persist = !state.save_persist;
        }
        KeyCode::Char(' ') if state.focus == SettingsFocus::History => {
            if let Some(value) = state.history.get(state.selected_history) {
                state.input = value.clone();
                state.focus = SettingsFocus::Input;
            }
        }
        KeyCode::Up if state.focus == SettingsFocus::History && state.selected_history > 0 => {
            state.selected_history -= 1;
        }
        KeyCode::Down
            if state.focus == SettingsFocus::History
                && state.selected_history + 1 < state.history.len() =>
        {
            state.selected_history += 1;
        }
        _ => {
            if state.focus == SettingsFocus::Input {
                match key.code {
                    KeyCode::Char(ch) => state.input.push(ch),
                    KeyCode::Backspace => {
                        state.input.pop();
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(true)
}

fn next_settings_focus(focus: SettingsFocus) -> SettingsFocus {
    match focus {
        SettingsFocus::Input => SettingsFocus::History,
        SettingsFocus::History => SettingsFocus::SaveCheckbox,
        SettingsFocus::SaveCheckbox => SettingsFocus::ApplyButton,
        SettingsFocus::ApplyButton => SettingsFocus::ClearButton,
        SettingsFocus::ClearButton => SettingsFocus::CancelButton,
        SettingsFocus::CancelButton => SettingsFocus::Input,
    }
}

fn prev_settings_focus(focus: SettingsFocus) -> SettingsFocus {
    match focus {
        SettingsFocus::Input => SettingsFocus::CancelButton,
        SettingsFocus::History => SettingsFocus::Input,
        SettingsFocus::SaveCheckbox => SettingsFocus::History,
        SettingsFocus::ApplyButton => SettingsFocus::SaveCheckbox,
        SettingsFocus::ClearButton => SettingsFocus::ApplyButton,
        SettingsFocus::CancelButton => SettingsFocus::ClearButton,
    }
}

fn apply_settings(
    state: &mut SettingsState,
    stream: &mut UnixStream,
    status_message: &mut Option<(String, Instant)>,
) -> Result<(), LifecycleError> {
    let endpoint = state.input.trim();
    if endpoint.is_empty() {
        *status_message = Some((
            "public endpoint cannot be empty; use Clear to remove".to_string(),
            Instant::now(),
        ));
        return Ok(());
    }
    send_set_public(stream, Some(endpoint.to_string()), state.save_persist)
}

fn send_set_public(
    stream: &mut UnixStream,
    endpoint: Option<String>,
    save: bool,
) -> Result<(), LifecycleError> {
    let line = match endpoint {
        Some(endpoint) => format!("SET_PUBLIC {endpoint}{}", if save { " SAVE" } else { "" }),
        None => format!("CLEAR_PUBLIC{}", if save { " SAVE" } else { "" }),
    };
    writeln!(stream, "{line}").map_err(|error| {
        LifecycleError::Io("failed to send SET_PUBLIC command".to_string(), error)
    })?;
    stream.flush().map_err(|error| {
        LifecycleError::Io("failed to flush SET_PUBLIC command".to_string(), error)
    })
}

fn key_event_to_logical_key(key: &KeyEvent) -> Option<LogicalKey> {
    // Ignore key-release events; crossterm already filters by KeyEventKind::Press
    // in the caller, so any key reaching here is a press we want to forward.
    Some(LogicalKey::from(key))
}

#[derive(Clone, Copy, Debug, Default)]
struct ImeCursorState {
    area: Rect,
    cursor: Option<(u16, u16)>,
    cursor_visible: bool,
}

struct RenderArgs<'a> {
    snapshot: &'a RatatuiSnapshot,
    focus: Focus,
    selected_index: usize,
    sidebar_hidden: bool,
    history_state: Option<&'a HistoryState>,
    error_log_state: Option<&'a mut ErrorLogState>,
    settings_state: Option<&'a SettingsState>,
    active_target: Option<&'a str>,
    status_message: Option<&'a str>,
    dim_background: bool,
    cursor_state: &'a mut Option<ImeCursorState>,
}

fn render(frame: &mut Frame, args: RenderArgs<'_>) {
    let RenderArgs {
        snapshot,
        focus,
        selected_index,
        sidebar_hidden,
        history_state,
        error_log_state,
        settings_state,
        active_target,
        status_message,
        dim_background,
        cursor_state,
    } = args;
    let area = frame.size();

    // Outer vertical layout: chrome above, footer below.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // Inner horizontal layout: main pane left, optional separator, optional sidebar right.
    // This layout is computed even in history mode so the sidebar stays visible.
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

    let history_viewport =
        history_state.map(|history| compute_history_viewport(history, inner[0].height as usize));

    *cursor_state = Some(ImeCursorState {
        area: inner[0],
        cursor: snapshot.main_cursor,
        cursor_visible: snapshot.main_cursor_visible,
    });

    if let Some(history) = history_state {
        render_history_view(frame, history, inner[0], cursor_state);
    } else {
        let main_block = Block::default()
            .borders(Borders::NONE)
            .style(dim_style(Style::default(), dim_background));
        let main_text = render_main_text(snapshot, inner[0]);
        let main = Paragraph::new(main_text).block(main_block);
        frame.render_widget(main, inner[0]);

        // Draw cursor if the active session provided a cursor position.
        // When the PTY hides its cursor we still track the position for the IME
        // post-render adjustment, but we do not draw a visible cursor here.
        if focus == Focus::Main {
            if let Some((col, row)) = snapshot.main_cursor {
                let cursor_x = inner[0].x + col;
                let cursor_y = inner[0].y + row;
                if inner[0].contains(ratatui::layout::Position::new(cursor_x, cursor_y))
                    && snapshot.main_cursor_visible
                {
                    frame
                        .buffer_mut()
                        .get_mut(cursor_x, cursor_y)
                        .set_style(Style::default().add_modifier(Modifier::REVERSED));
                    frame.set_cursor(cursor_x, cursor_y);
                }
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
    } else if let Some(viewport) = history_viewport {
        let footer_style = dim_style(
            Style::default().bg(Color::Blue).fg(Color::White),
            dim_background,
        );
        Paragraph::new(render_history_footer_line(
            outer[1].width as usize,
            viewport.offset,
            viewport.visible_bottom,
            viewport.total_lines,
        ))
        .style(footer_style)
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

    if let Some(settings) = settings_state {
        render_settings_popup(frame, settings, snapshot, outer[0]);
    }
}

/// Viewport coordinates for a history buffer rendered inside a given height.
struct HistoryViewport {
    offset: usize,
    content_height: usize,
    visible_bottom: usize,
    total_lines: usize,
    cursor_line: usize,
}

fn compute_history_viewport(history: &HistoryState, height: usize) -> HistoryViewport {
    let total_lines = history.styled_lines.len();
    let cursor_line = history.cursor_line.min(total_lines.saturating_sub(1));
    let offset = if total_lines <= height {
        0
    } else if cursor_line + height > total_lines {
        total_lines - height
    } else {
        cursor_line
    };
    let visible_bottom = (offset + height).min(total_lines);
    HistoryViewport {
        offset,
        content_height: height,
        visible_bottom,
        total_lines,
        cursor_line,
    }
}

fn render_history_view(
    frame: &mut Frame,
    history: &HistoryState,
    area: Rect,
    cursor_state: &mut Option<ImeCursorState>,
) {
    if area.height == 0 {
        return;
    }

    let width = area.width as usize;
    let content_height = area.height as usize;
    let viewport = compute_history_viewport(history, content_height);

    let mut lines = Vec::new();
    for line in history
        .styled_lines
        .iter()
        .skip(viewport.offset)
        .take(viewport.content_height)
    {
        let spans: Vec<Span<'static>> = crate::terminal::parse_ansi_styled_line(line)
            .into_iter()
            .map(|(text, style)| Span::styled(text, text_style_to_ratatui(&style)))
            .collect();
        lines.push(truncate_spans_to_width(spans, width));
    }
    while lines.len() < viewport.content_height {
        lines.push(Line::from(""));
    }

    frame.render_widget(Paragraph::new(lines).style(Style::default()), area);

    // Draw a visible cursor on the cursor line so the user knows their position.
    if viewport.total_lines > 0 {
        let cursor_screen_row = viewport.cursor_line.saturating_sub(viewport.offset);
        if cursor_screen_row < viewport.content_height {
            let cursor_col = history.cursor_col.min(area.width.saturating_sub(1));
            let cursor_x = area.x + cursor_col;
            let cursor_y = area.y + cursor_screen_row as u16;
            if area.contains(ratatui::layout::Position::new(cursor_x, cursor_y)) {
                frame
                    .buffer_mut()
                    .get_mut(cursor_x, cursor_y)
                    .set_style(Style::default().add_modifier(Modifier::REVERSED));
                frame.set_cursor(cursor_x, cursor_y);
                *cursor_state = Some(ImeCursorState {
                    area,
                    cursor: Some((cursor_col, cursor_screen_row as u16)),
                    cursor_visible: true,
                });
            }
        }
    }
}

fn render_history_footer_line(
    area_width: usize,
    offset: usize,
    visible_bottom: usize,
    total_lines: usize,
) -> Line<'static> {
    let position_text = if total_lines == 0 {
        "[empty]".to_string()
    } else {
        format!("[{}-{} / {}]", offset + 1, visible_bottom, total_lines)
    };
    let right_text = "Ctrl-O/Esc Exit · PgUp/PgDn Page · Up/Down Line";
    let left_text = format!("History  {position_text}");
    let left_width = display_width(&left_text);
    let right_width = display_width(right_text);
    let fill = area_width.saturating_sub(left_width + right_width);
    Line::from(vec![
        Span::styled(
            left_text,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".repeat(fill), Style::default().fg(Color::Gray)),
        Span::styled(right_text, Style::default().fg(Color::Gray)),
    ])
}

/// Render a unified popup background: clear the popup rectangle so underlying
/// Background color used for modal popups; kept in sync with the Ctrl+W
/// "Connect Remote Host" popup so all modals look consistent.
const POPUP_BG: Color = Color::Rgb(22, 24, 30);

/// cells do not leak through, then fill the popup interior with the shared
/// popup background. The area behind the popup is dimmed by the caller via the
/// `dim_background` flag passed to `render`.
fn render_popup_background(frame: &mut Frame, popup: Rect) {
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(POPUP_BG)), popup);
}

fn render_error_log_popup(frame: &mut Frame, state: &mut ErrorLogState, area: Rect) {
    let popup = centered_rect(90, 85, area);
    render_popup_background(frame, popup);

    let block = Block::default()
        .title("Error Log")
        .title_alignment(ratatui::layout::Alignment::Center)
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::White).bg(POPUP_BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let width = inner.width as usize;
    // Reserve the bottom row for the position/hint footer and one blank row
    // so the footer does not visually collide with the last log line.
    let content_height = inner.height.saturating_sub(2) as usize;
    let total_lines = state.entries.len();
    let max_offset = total_lines.saturating_sub(content_height);
    let offset = state.scroll_offset.min(max_offset);
    // Keep the stored offset clamped to the current viewport so Up/Down/PageUp
    // operate on the visible position rather than stale values like usize::MAX.
    state.scroll_offset = offset;

    // Fixed-width columns: time (HH:MM:SS.mmm) + level (1 char) + module tag + content.
    const TIME_WIDTH: usize = 12;
    const LEVEL_WIDTH: usize = 1;
    const GUTTER: usize = 2;
    const MAX_MODULE_WIDTH: usize = 20;
    let module_width = state
        .entries
        .iter()
        .filter_map(|(_, _, msg)| parse_module_tag(msg).0.map(display_width))
        .max()
        .unwrap_or(0)
        .min(MAX_MODULE_WIDTH);
    let content_width = width.saturating_sub(TIME_WIDTH + LEVEL_WIDTH + module_width + GUTTER * 3);

    let mut lines = Vec::new();
    for (ts, level, message) in state.entries.iter().skip(offset).take(content_height) {
        lines.push(format_log_line(
            *ts,
            *level,
            message,
            module_width,
            content_width,
        ));
    }
    while lines.len() < content_height {
        lines.push(Line::from(""));
    }

    let visible_bottom = (offset + lines.len()).min(total_lines.max(1));
    let position_text = if total_lines == 0 {
        "[empty]".to_string()
    } else {
        format!("[{}-{} / {}]", offset + 1, visible_bottom, total_lines)
    };
    let footer_text = format!(
        "{}  ·  Enter/Esc/q close · Home/g top · End/G bottom · ↑↓/PgUp/PgDn scroll",
        position_text
    );
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        footer_text,
        Style::default().fg(Color::Gray).bg(POPUP_BG),
    )]));

    let content_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: content_height as u16,
    };
    let footer_area = Rect {
        x: inner.x,
        y: inner.y + content_height as u16 + 1,
        width: inner.width,
        height: 1,
    };

    let content = Paragraph::new(lines)
        .style(Style::default().bg(POPUP_BG))
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(content, content_area);
    frame.render_widget(footer, footer_area);
}

fn format_log_line(
    ts: u128,
    level: LogLevel,
    message: &str,
    module_width: usize,
    content_width: usize,
) -> Line<'static> {
    let total_seconds = ts / 1000;
    let millis = (ts % 1000) as u32;
    let hours = ((total_seconds / 3600) % 24) as u32;
    let minutes = ((total_seconds / 60) % 60) as u32;
    let seconds = (total_seconds % 60) as u32;
    let time = format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, seconds, millis);

    let (level_text, level_fg, level_bg) = match level {
        LogLevel::Debug => (LogLevel::Debug.short(), Color::Black, Color::Gray),
        LogLevel::Info => (LogLevel::Info.short(), Color::White, Color::Blue),
        LogLevel::Warn => (LogLevel::Warn.short(), Color::Black, Color::Yellow),
        LogLevel::Error => (LogLevel::Error.short(), Color::White, Color::Red),
    };

    let (module, body) = parse_module_tag(message);
    let module_span = module.map(|tag| {
        let padded = pad_right(&truncate_display_width(tag, module_width), module_width);
        Span::styled(padded, Style::default().fg(Color::DarkGray).bg(POPUP_BG))
    });
    let content = truncate_display_width(body, content_width);

    let mut spans = vec![
        Span::styled(time, Style::default().fg(Color::DarkGray).bg(POPUP_BG)),
        Span::raw("  "),
        Span::styled(
            level_text.to_string(),
            Style::default().fg(level_fg).bg(level_bg),
        ),
    ];
    if module_width > 0 {
        spans.push(Span::raw("  "));
        if let Some(module_span) = module_span {
            spans.push(module_span);
        } else {
            spans.push(Span::styled(
                " ".repeat(module_width),
                Style::default().bg(POPUP_BG),
            ));
        }
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        content,
        Style::default().fg(Color::White).bg(POPUP_BG),
    ));
    Line::from(spans)
}

/// Extract a leading `[module]` tag from a log message.
/// Returns `(Some(tag), rest)` if the message starts with a non-empty,
/// space-free bracketed prefix; otherwise `(None, message)`.
fn parse_module_tag(message: &str) -> (Option<&str>, &str) {
    let Some(rest) = message.strip_prefix('[') else {
        return (None, message);
    };
    let Some(close) = rest.find(']') else {
        return (None, message);
    };
    let tag = &rest[..close];
    let after = rest[close + 1..].trim_start();
    if tag.is_empty() || tag.contains(' ') {
        return (None, message);
    }
    (Some(tag), after)
}

fn render_settings_popup(
    frame: &mut Frame,
    state: &SettingsState,
    snapshot: &RatatuiSnapshot,
    area: Rect,
) {
    let popup = centered_rect(70, 70, area);
    render_popup_background(frame, popup);

    let block = Block::default()
        .style(Style::default().bg(POPUP_BG))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Settings (Ctrl-P to close) ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.width < 10 || inner.height < 8 {
        return;
    }

    // Vertical layout: listen, input, history, checkbox, buttons, hint.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .margin(1)
        .split(inner);

    let listen_value = snapshot
        .footer
        .listener_endpoint
        .clone()
        .unwrap_or_else(|| "(unknown)".to_string());
    render_settings_readonly_row(frame, "Listen", &listen_value, rows[0], false);

    let public_value = if state.input.is_empty() {
        "(use listen)".to_string()
    } else {
        state.input.clone()
    };
    render_settings_input_row(
        frame,
        "Public",
        &public_value,
        rows[1],
        state.focus == SettingsFocus::Input,
    );

    render_settings_history(frame, state, rows[2], state.focus == SettingsFocus::History);

    let checkbox_label = if state.save_persist {
        "[x] Save for next startup"
    } else {
        "[ ] Save for next startup"
    };
    render_settings_selectable_row(
        frame,
        checkbox_label,
        rows[3],
        state.focus == SettingsFocus::SaveCheckbox,
    );

    render_settings_buttons(frame, state.focus, rows[4]);
}

fn render_settings_readonly_row(
    frame: &mut Frame,
    label: &str,
    value: &str,
    area: Rect,
    focused: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .title(format!(" {label} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = truncate_display_width(value, inner.width as usize);
    let paragraph = Paragraph::new(Line::from(vec![Span::styled(
        text,
        Style::default().fg(Color::White),
    )]));
    frame.render_widget(paragraph, inner);
}

fn render_settings_input_row(
    frame: &mut Frame,
    label: &str,
    value: &str,
    area: Rect,
    focused: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .title(format!(" {label} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let style = if focused {
        Style::default().fg(Color::White).bg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };
    let text = truncate_display_width(value, inner.width as usize);
    let text_width = display_width(&text);
    let paragraph = Paragraph::new(Line::from(vec![Span::styled(text.clone(), style)]));
    frame.render_widget(paragraph, inner);

    if focused {
        let cursor_x = inner.x + (text_width as u16).min(inner.width.saturating_sub(1));
        let cursor_y = inner.y;
        frame.set_cursor(cursor_x, cursor_y);
    }
}

fn render_settings_history(frame: &mut Frame, state: &SettingsState, area: Rect, focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .title(" History ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.history.is_empty() {
        let paragraph = Paragraph::new(Line::from(vec![Span::styled(
            "(no history)",
            Style::default().fg(Color::Gray),
        )]));
        frame.render_widget(paragraph, inner);
        return;
    }

    let width = inner.width as usize;
    let height = inner.height as usize;
    let mut lines = Vec::new();
    let start = if state.selected_history + 1 > height {
        state.selected_history + 1 - height
    } else {
        0
    };
    for (idx, value) in state.history.iter().enumerate().skip(start).take(height) {
        let is_selected = idx == state.selected_history;
        let style = if is_selected && focused {
            Style::default().bg(Color::Blue).fg(Color::White)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![Span::styled(
            truncate_display_width(value, width),
            style,
        )]));
    }
    while lines.len() < height {
        lines.push(Line::from(""));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_settings_selectable_row(frame: &mut Frame, label: &str, area: Rect, focused: bool) {
    let style = if focused {
        Style::default().bg(Color::Blue).fg(Color::White)
    } else {
        Style::default().fg(Color::White)
    };
    let paragraph = Paragraph::new(Line::from(vec![Span::styled(label, style)]));
    frame.render_widget(paragraph, area);
}

fn render_settings_buttons(frame: &mut Frame, focus: SettingsFocus, area: Rect) {
    let labels = [
        (SettingsFocus::ApplyButton, "Apply"),
        (SettingsFocus::ClearButton, "Clear"),
        (SettingsFocus::CancelButton, "Cancel"),
    ];
    let constraints: Vec<Constraint> = labels.iter().map(|_| Constraint::Ratio(1, 3)).collect();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    for (idx, (button_focus, label)) in labels.iter().enumerate() {
        let focused = focus == *button_focus;
        let style = if focused {
            Style::default().bg(Color::Green).fg(Color::Black)
        } else {
            Style::default().fg(Color::White)
        };
        let text = right_align(label, cols[idx].width as usize);
        let paragraph = Paragraph::new(Line::from(vec![Span::styled(text, style)]));
        frame.render_widget(paragraph, cols[idx]);
    }
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

    let mut left_spans = Vec::new();

    if let Some(connect) = &snapshot.footer.connect_endpoint {
        left_spans.push(Span::styled("· Connect ", accent_style));
        left_spans.push(Span::styled(format!("{} ", connect), muted_style));
    }
    if snapshot.footer.remote_count > 0 {
        left_spans.push(Span::styled("· ", muted_style));
        left_spans.push(Span::styled(
            format!("{} remote ", snapshot.footer.remote_count),
            muted_style,
        ));
    }

    if !left_spans.is_empty() {
        left_spans.push(Span::styled("· ", muted_style));
    }

    let toggle_label = if sidebar_hidden { "Show" } else { "Hide" };
    let menu_text = format!(
        "Ctrl-G {} · Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-H Sess · Ctrl-O Hist · Ctrl-E Logs · Ctrl-P Sett",
        toggle_label
    );

    let left_width = left_spans
        .iter()
        .map(|s| display_width(&s.content))
        .sum::<usize>();
    let menu_width = display_width(&menu_text);

    let path_text = active_session_current_path(snapshot)
        .map(|p| format!("📁 {p}"))
        .unwrap_or_default();
    let path_width = display_width(&path_text);
    let available_for_path = area_width.saturating_sub(left_width + menu_width + 1);
    let path_display = if path_text.is_empty() || available_for_path == 0 {
        String::new()
    } else if path_width > available_for_path {
        truncate_from_left(&path_text, available_for_path)
    } else {
        path_text
    };
    let path_display_width = display_width(&path_display);

    let fill = area_width.saturating_sub(left_width + menu_width + path_display_width);

    let mut spans = left_spans;
    spans.push(Span::styled(menu_text, muted_style));
    if fill > 0 {
        spans.push(Span::styled(" ".repeat(fill), muted_style));
    }
    if !path_display.is_empty() {
        spans.push(Span::styled(path_display, muted_style));
    }

    Line::from(spans)
}

fn active_session_current_path(snapshot: &RatatuiSnapshot) -> Option<String> {
    let target_id = snapshot.active_target.as_deref()?;
    let session = snapshot.sessions.iter().find(|s| s.id == target_id)?;
    let path = session.current_path.as_deref()?;
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// Truncate a string by removing characters from the left, keeping the
/// rightmost characters. Adds a leading ellipsis if truncation occurred.
fn truncate_from_left(text: &str, width: usize) -> String {
    if width == 0 || display_width(text) <= width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut used = 0usize;
    let mut start = chars.len();
    for (index, ch) in chars.iter().enumerate().rev() {
        let ch_width = char_width(*ch);
        if used + ch_width > width.saturating_sub(1) {
            break;
        }
        used += ch_width;
        start = index;
    }
    if start >= chars.len() {
        return text.to_string();
    }
    format!("…{}", chars[start..].iter().collect::<String>())
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
