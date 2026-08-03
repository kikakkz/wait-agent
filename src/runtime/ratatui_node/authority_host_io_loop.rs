use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use super::local_session::render_grid_line;
use super::runtime::SharedState;
use super::state_event::StateEvent;

const SIGCHLD_TOKEN: usize = 0;
const SHUTDOWN_TOKEN: usize = 1;
const REQUEST_WAKE_TOKEN: usize = 2;
const INITIAL_SESSION_TOKEN: usize = 3;
const PTY_READ_BUF_SIZE: usize = 4096;

/// Requests sent to `AuthorityHostIoLoop` to perform PTY I/O or change the
/// set of polled sessions.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum AuthorityHostIoRequest {
    WriteInput {
        session_id: String,
        bytes: Vec<u8>,
    },
    Resize {
        session_id: String,
        console_id: String,
        cols: u16,
        rows: u16,
    },
    RegisterSession {
        session_id: String,
        pty_master: File,
        child: Child,
        output_tx: Option<mpsc::Sender<Vec<u8>>>,
        cols: u16,
        rows: u16,
    },
    UnregisterSession {
        session_id: String,
    },
    UnregisterConsole {
        session_id: String,
        console_id: String,
    },
    SetOutputSender {
        session_id: String,
        output_tx: mpsc::Sender<Vec<u8>>,
    },
}

/// Handle used by other threads to send requests to `AuthorityHostIoLoop`.
///
/// Every successful send also writes to an internal wake pipe so the IO loop
/// never blocks in `poll` while requests are pending.
#[derive(Clone)]
pub(crate) struct AuthorityHostIoHandle {
    tx: mpsc::Sender<AuthorityHostIoRequest>,
    wake: Arc<Mutex<Option<UnixStream>>>,
}

impl AuthorityHostIoHandle {
    /// Send a request to the IO loop and wake it if it is blocked in poll.
    pub(crate) fn send(
        &self,
        request: AuthorityHostIoRequest,
    ) -> Result<(), mpsc::SendError<AuthorityHostIoRequest>> {
        self.tx.send(request)?;
        if let Ok(mut guard) = self.wake.lock() {
            if let Some(wake) = guard.as_mut() {
                let _ = wake.write_all(&[1]);
            }
        }
        Ok(())
    }

    /// Return a handle whose sends are silently dropped. Used before the IO
    /// loop has been started so callers do not panic.
    pub(crate) fn dangling() -> Self {
        let (tx, _) = mpsc::channel();
        Self {
            tx,
            wake: Arc::new(Mutex::new(None)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ConsoleState {
    cols: u16,
    rows: u16,
}

/// Minimal `Dimensions` implementation for creating an `alacritty_terminal::Term`.
struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

struct SessionState {
    pty_master: File,
    child: Child,
    token: usize,
    output_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Buffered PTY output produced before an output sender was installed.
    /// Kept small; once a sender is attached the buffer is flushed.
    output_buffer: VecDeque<u8>,
    /// Buffered input bytes that could not be written to the PTY immediately
    /// because the kernel buffer was full. Drained when the PTY becomes
    /// writable again.
    pending_write: Vec<u8>,
    /// Consoles currently viewing this session, mapped to their last requested
    /// dimensions. The local TUI uses the reserved id "local"; remote viewers
    /// use the console id from their OpenMirror/ApplyResize envelopes.
    consoles: HashMap<String, ConsoleState>,
    /// The console whose dimensions currently drive the PTY size. A local
    /// console always takes precedence over remote viewers.
    active_console: Option<String>,
    /// Local terminal emulator model of the PTY screen. Used to synthesize a
    /// bootstrap ANSI replay when a new remote console attaches.
    term: Term<VoidListener>,
    /// VTE parser that turns raw PTY bytes into terminal state updates.
    parser: ansi::Processor,
    /// Set when a new output sender is installed but the active console's
    /// dimensions are not known yet. The bootstrap is sent once the console
    /// is activated so it is rendered at the viewer's size instead of the
    /// PTY's initial size.
    bootstrap_pending: bool,
}

/// Single thread that owns all authority-host PTY master fd I/O.
///
/// - Registers every authority-host PTY with `polling::Poller`.
/// - Reads raw PTY bytes and forwards them to the per-session output sender.
/// - Receives write/resize requests from `RatatuiAuthorityHostSession` and
///   applies them to the PTY master.
/// - Listens for `SIGCHLD` via `signal_hook`; when it fires, iterates the
///   managed children with `try_wait()` and reports exits to `StateEventLoop`.
pub(crate) struct AuthorityHostIoLoop {
    tx: mpsc::Sender<AuthorityHostIoRequest>,
    shutdown_write: Option<UnixStream>,
    request_wake: Arc<Mutex<Option<UnixStream>>>,
}

impl AuthorityHostIoLoop {
    pub(crate) fn start(shared: Arc<SharedState>) -> Result<Self, LifecycleError> {
        let (tx, rx) = mpsc::channel::<AuthorityHostIoRequest>();
        let (shutdown_read, shutdown_write) = UnixStream::pair().map_err(|error| {
            LifecycleError::Io(
                "failed to create authority host IO shutdown pipe".to_string(),
                error,
            )
        })?;
        let (request_read, request_write) = UnixStream::pair().map_err(|error| {
            LifecycleError::Io(
                "failed to create authority host IO request wake pipe".to_string(),
                error,
            )
        })?;
        let request_wake = Arc::new(Mutex::new(Some(request_write)));
        std::thread::spawn(move || {
            if let Err(error) = run_io_loop(shared, rx, shutdown_read, request_read) {
                ERROR_LOG.log(format!(
                    "[ratatui-authority-host-io] loop exited with error: {error}"
                ));
            }
        });
        Ok(Self {
            tx,
            shutdown_write: Some(shutdown_write),
            request_wake,
        })
    }

    pub(crate) fn sender(&self) -> AuthorityHostIoHandle {
        AuthorityHostIoHandle {
            tx: self.tx.clone(),
            wake: self.request_wake.clone(),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(mut shutdown_write) = self.shutdown_write.take() {
            let _ = shutdown_write.write_all(&[1]);
        }
        if let Ok(mut guard) = self.request_wake.lock() {
            let _ = guard.take();
        }
    }
}

impl Drop for AuthorityHostIoLoop {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_io_loop(
    shared: Arc<SharedState>,
    rx: mpsc::Receiver<AuthorityHostIoRequest>,
    mut shutdown_read: UnixStream,
    mut request_read: UnixStream,
) -> Result<(), LifecycleError> {
    let mut poller = polling::Poller::new().map_err(|error| {
        LifecycleError::Io(
            "failed to create authority host IO poller".to_string(),
            io::Error::new(io::ErrorKind::Other, error),
        )
    })?;

    let (mut sig_read, sig_write) = UnixStream::pair().map_err(|error| {
        LifecycleError::Io("failed to create SIGCHLD signal pipe".to_string(), error)
    })?;
    if let Err(error) =
        signal_hook::low_level::pipe::register(signal_hook::consts::signal::SIGCHLD, sig_write)
    {
        return Err(LifecycleError::Io(
            "failed to register SIGCHLD handler".to_string(),
            io::Error::new(io::ErrorKind::Other, error),
        ));
    }
    // SAFETY: sig_read outlives the poller; it is owned by this loop and
    // unregistered only on loop exit.
    unsafe {
        poller
            .add_with_mode(
                &sig_read,
                polling::Event::readable(SIGCHLD_TOKEN),
                polling::PollMode::Level,
            )
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to add SIGCHLD pipe to poller".to_string(),
                    io::Error::new(io::ErrorKind::Other, error),
                )
            })?;
    }
    // SAFETY: shutdown_read outlives the poller; it is owned by this loop and
    // unregistered only on loop exit.
    unsafe {
        poller
            .add_with_mode(
                &shutdown_read,
                polling::Event::readable(SHUTDOWN_TOKEN),
                polling::PollMode::Level,
            )
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to add authority host IO shutdown pipe to poller".to_string(),
                    io::Error::new(io::ErrorKind::Other, error),
                )
            })?;
    }
    // SAFETY: request_read outlives the poller; it is owned by this loop and
    // unregistered only on loop exit.
    unsafe {
        poller
            .add_with_mode(
                &request_read,
                polling::Event::readable(REQUEST_WAKE_TOKEN),
                polling::PollMode::Level,
            )
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to add authority host IO request wake pipe to poller".to_string(),
                    io::Error::new(io::ErrorKind::Other, error),
                )
            })?;
    }

    let mut sessions: HashMap<String, SessionState> = HashMap::new();
    let mut token_to_session: HashMap<usize, String> = HashMap::new();
    let mut next_token: usize = INITIAL_SESSION_TOKEN;
    let mut events = polling::Events::new();

    loop {
        // Drain pending requests before/after each poll so the loop stays
        // responsive even when PTY traffic is steady.
        drain_requests(
            &rx,
            &mut poller,
            &mut sessions,
            &mut token_to_session,
            &mut next_token,
        )?;

        events.clear();
        match poller.wait(&mut events, None) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(error) => {
                return Err(LifecycleError::Io(
                    "authority host IO poll failed".to_string(),
                    error,
                ));
            }
        }

        for event in events.iter() {
            if event.key == SIGCHLD_TOKEN {
                let mut buf = [0u8; 1];
                let _ = sig_read.read(&mut buf);
                check_child_exits(&mut sessions, &shared);
                continue;
            }

            if event.key == SHUTDOWN_TOKEN {
                let mut buf = [0u8; 1];
                let _ = shutdown_read.read(&mut buf);
                return Ok(());
            }

            if event.key == REQUEST_WAKE_TOKEN {
                drain_request_wake(&mut request_read);
                // Requests are drained at the top of the next loop iteration.
                continue;
            }

            let Some(session_id) = token_to_session.get(&event.key).cloned() else {
                continue;
            };
            let mut dead = false;
            if event.readable {
                dead = read_pty(&session_id, &mut sessions);
                if dead {
                    if let Some(state) = sessions.remove(&session_id) {
                        let _ = poller.delete(&state.pty_master);
                        token_to_session.remove(&state.token);
                    }
                    let _ = shared
                        .state_sender()
                        .send(StateEvent::AuthorityHostSessionPtyClosed {
                            target_id: session_id.to_string(),
                        });
                }
            }
            if event.writable && !dead {
                if let Some(state) = sessions.get_mut(&session_id) {
                    drain_pending_write(state, &mut poller);
                }
            }
        }
    }
}

fn drain_requests(
    rx: &mpsc::Receiver<AuthorityHostIoRequest>,
    poller: &mut polling::Poller,
    sessions: &mut HashMap<String, SessionState>,
    token_to_session: &mut HashMap<usize, String>,
    next_token: &mut usize,
) -> Result<(), LifecycleError> {
    while let Ok(request) = rx.try_recv() {
        match request {
            AuthorityHostIoRequest::WriteInput { session_id, bytes } => {
                if let Some(state) = sessions.get_mut(&session_id) {
                    append_input_to_pending_write(state, &bytes);
                    drain_pending_write(state, poller);
                }
            }
            AuthorityHostIoRequest::Resize {
                session_id,
                console_id,
                cols,
                rows,
            } => {
                if let Some(state) = sessions.get_mut(&session_id) {
                    apply_console_resize(state, &session_id, console_id, cols, rows);
                }
            }
            AuthorityHostIoRequest::UnregisterConsole {
                session_id,
                console_id,
            } => {
                if let Some(state) = sessions.get_mut(&session_id) {
                    unregister_console(state, &session_id, console_id);
                }
            }
            AuthorityHostIoRequest::RegisterSession {
                session_id,
                mut pty_master,
                child,
                output_tx,
                cols,
                rows,
            } => {
                set_nonblocking(&mut pty_master);
                let token = *next_token;
                *next_token += 1;
                // SAFETY: pty_master is owned by this loop (stored in `sessions`)
                // and lives until UnregisterSession is processed.
                unsafe {
                    poller
                        .add_with_mode(
                            &pty_master,
                            polling::Event::readable(token),
                            polling::PollMode::Level,
                        )
                        .map_err(|error| {
                            LifecycleError::Io(
                                format!("failed to add authority host pty {session_id} to poller"),
                                io::Error::new(io::ErrorKind::Other, error),
                            )
                        })?;
                }
                token_to_session.insert(token, session_id.clone());
                let term = Term::new(
                    Config::default(),
                    &TermSize {
                        cols: cols as usize,
                        rows: rows as usize,
                    },
                    VoidListener,
                );
                sessions.insert(
                    session_id,
                    SessionState {
                        pty_master,
                        child,
                        token,
                        output_tx,
                        output_buffer: VecDeque::new(),
                        pending_write: Vec::new(),
                        consoles: HashMap::new(),
                        active_console: None,
                        term,
                        parser: ansi::Processor::new(),
                        bootstrap_pending: false,
                    },
                );
            }
            AuthorityHostIoRequest::UnregisterSession { session_id } => {
                if let Some(state) = sessions.remove(&session_id) {
                    let _ = poller.delete(&state.pty_master);
                    token_to_session.remove(&state.token);
                }
            }
            AuthorityHostIoRequest::SetOutputSender {
                session_id,
                output_tx,
            } => {
                if let Some(state) = sessions.get_mut(&session_id) {
                    state.output_tx = Some(output_tx);
                    // The bootstrap snapshot already captures all output produced
                    // so far; replaying the buffered raw bytes on top of it would
                    // corrupt the screen. Drop the buffer.
                    state.output_buffer.clear();
                    if state.active_console.is_some() {
                        send_bootstrap(state);
                    } else {
                        // Defer bootstrap until the viewer's console is activated
                        // and the PTY/Term have been resized to the right geometry.
                        state.bootstrap_pending = true;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Reads available data from the session's PTY master.
///
/// Returns `true` if the PTY has reached EOF or encountered a fatal read
/// error. The caller must then remove the session from the poller and from
/// the internal maps and notify the state loop.
fn read_pty(session_id: &str, sessions: &mut HashMap<String, SessionState>) -> bool {
    let mut buf = [0u8; PTY_READ_BUF_SIZE];
    let Some(state) = sessions.get_mut(session_id) else {
        return false;
    };

    let mut total_read = 0usize;
    loop {
        match state.pty_master.read(&mut buf) {
            Ok(0) => {
                return true;
            }
            Ok(n) => {
                total_read += n;
                forward_output(state, &buf[..n]);
                if n < buf.len() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-authority-host-io] pty read error for {session_id}: {error}"
                ));
                return true;
            }
        }
    }

    if total_read > 0 {
        flush_output_buffer(state);
    }
    false
}

fn forward_output(state: &mut SessionState, bytes: &[u8]) {
    state.parser.advance(&mut state.term, bytes);
    if let Some(tx) = state.output_tx.as_ref() {
        let chunk = state.output_buffer.make_contiguous();
        let mut payload = Vec::with_capacity(chunk.len() + bytes.len());
        payload.extend_from_slice(chunk);
        payload.extend_from_slice(bytes);
        state.output_buffer.clear();
        let _ = tx.send(payload);
    } else {
        state.output_buffer.extend(bytes);
        // Cap the buffer to avoid unbounded growth if no bridge is ever
        // attached.  Dropping the oldest bytes is acceptable because the
        // viewer is not connected yet.
        while state.output_buffer.len() > PTY_READ_BUF_SIZE * 4 {
            state.output_buffer.pop_front();
        }
    }
}

fn flush_output_buffer(state: &mut SessionState) {
    if state.output_buffer.is_empty() {
        return;
    }
    if let Some(tx) = state.output_tx.as_ref() {
        let chunk: Vec<u8> = state.output_buffer.drain(..).collect();
        let _ = tx.send(chunk);
    }
}

/// Send a bootstrap ANSI snapshot to the session's output sender, if one is
/// installed, and clear the pending flag.
fn send_bootstrap(state: &mut SessionState) {
    if state.output_tx.is_none() {
        state.bootstrap_pending = false;
        return;
    }
    let bootstrap = bootstrap_ansi_for_term(&state.term);
    let _ = state.output_tx.as_ref().unwrap().send(bootstrap);
    state.bootstrap_pending = false;
}

/// Render the current terminal screen as an ANSI byte sequence that reproduces
/// the visible content and cursor position when fed to a fresh terminal emulator.
///
/// Used to bootstrap a newly attached remote console so it sees the prompt
/// immediately instead of a blank pane until new PTY output arrives.
fn bootstrap_ansi_for_term(term: &Term<VoidListener>) -> Vec<u8> {
    let grid = term.grid();
    let screen_lines = grid.screen_lines();
    let columns = grid.columns();
    let display_offset = grid.display_offset() as i32;

    let mut payload = Vec::with_capacity(screen_lines * (columns * 8 + 16) + 64);
    // Hide cursor, clear screen, and move to home to draw the bootstrap frame
    // without intermediate flicker.
    payload.extend_from_slice(b"\x1b[?25l\x1b[2J\x1b[H");
    for row in 0..screen_lines {
        if row > 0 {
            payload.extend_from_slice(b"\r\n");
        }
        let line = Line(row as i32 - display_offset);
        let (_, styled) = render_grid_line(&grid, line, columns);
        payload.extend_from_slice(styled.as_bytes());
    }

    let point = grid.cursor.point;
    let cursor_col = point.column.0 as u16 + 1;
    let cursor_row = (point.line.0 + display_offset) as u16 + 1;
    payload.extend_from_slice(format!("\x1b[{cursor_row};{cursor_col}H").as_bytes());
    payload.extend_from_slice(b"\x1b[?25h");
    payload
}

fn check_child_exits(sessions: &mut HashMap<String, SessionState>, shared: &Arc<SharedState>) {
    for (session_id, state) in sessions.iter_mut() {
        match state.child.try_wait() {
            Ok(Some(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                let _ = shared
                    .state_sender()
                    .send(StateEvent::AuthorityHostSessionChildExited {
                        target_id: session_id.clone(),
                        exit_code,
                    });
            }
            Ok(None) => {}
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-authority-host-io] try_wait failed for {session_id}: {error}"
                ));
            }
        }
    }
}

/// Drain the request-wake pipe so level-triggered polling does not fire
/// repeatedly. Reads until the pipe is empty or an error occurs.
fn drain_request_wake(request_read: &mut UnixStream) {
    let mut buf = [0u8; 64];
    loop {
        match request_read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) if n < buf.len() => break,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

fn append_input_to_pending_write(state: &mut SessionState, bytes: &[u8]) {
    if state.pending_write.is_empty() {
        // Attempt an immediate non-blocking write. If it succeeds fully we
        // avoid buffering; otherwise queue the remainder for the writable event.
        match state.pty_master.write(bytes) {
            Ok(n) if n == bytes.len() => return,
            Ok(n) => state.pending_write.extend_from_slice(&bytes[n..]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                state.pending_write.extend_from_slice(bytes);
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-authority-host-io] pty write error: {error}"
                ));
                return;
            }
        }
    } else {
        state.pending_write.extend_from_slice(bytes);
    }
    let _ = state.pty_master.flush();
}

fn drain_pending_write(state: &mut SessionState, poller: &mut polling::Poller) {
    if state.pending_write.is_empty() {
        // We are only interested in readability when there is nothing to write.
        // Keep the PTY in level-triggered mode; `Poller::modify` defaults to
        // oneshot, which would drop subsequent output events between writes.
        let _ = poller.modify_with_mode(
            &state.pty_master,
            polling::Event::readable(state.token),
            polling::PollMode::Level,
        );
        return;
    }

    let mut offset = 0;
    while offset < state.pending_write.len() {
        match state.pty_master.write(&state.pending_write[offset..]) {
            Ok(n) => offset += n,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-authority-host-io] pty write error: {error}"
                ));
                state.pending_write.clear();
                let _ = poller.modify_with_mode(
                    &state.pty_master,
                    polling::Event::readable(state.token),
                    polling::PollMode::Level,
                );
                return;
            }
        }
    }

    if offset > 0 {
        state.pending_write.drain(..offset);
    }

    if state.pending_write.is_empty() {
        let _ = poller.modify_with_mode(
            &state.pty_master,
            polling::Event::readable(state.token),
            polling::PollMode::Level,
        );
    } else {
        let _ = poller.modify_with_mode(
            &state.pty_master,
            polling::Event::all(state.token),
            polling::PollMode::Level,
        );
    }
    let _ = state.pty_master.flush();
}

/// Apply a resize request from a console and, if that console becomes (or is)
/// the active console, resize the PTY.
///
/// Locking: this is called from the single IO loop thread; `state` is not
/// shared with other threads.
fn apply_console_resize(
    state: &mut SessionState,
    session_id: &str,
    console_id: String,
    cols: u16,
    rows: u16,
) {
    state
        .consoles
        .insert(console_id.clone(), ConsoleState { cols, rows });

    let should_activate = match state.active_console.as_deref() {
        // A local console always keeps priority.
        Some("local") => console_id == "local",
        // No active console, or only remote viewers: the latest request wins.
        _ => true,
    };

    if should_activate {
        if state.active_console.as_deref() != Some(console_id.as_str()) {
            ERROR_LOG.log(format!(
                "[ratatui-authority-host-io] session={session_id} active_console={console_id}"
            ));
            state.active_console = Some(console_id);
        }
        ERROR_LOG.log(format!(
            "[ratatui-authority-host-io] resize PTY session={session_id} cols={cols} rows={rows}"
        ));
        resize_pty(&state.pty_master, cols, rows);
        state.term.resize(TermSize {
            cols: cols as usize,
            rows: rows as usize,
        });
        if state.bootstrap_pending {
            send_bootstrap(state);
        }
    }
}

/// Remove a console from the session. If the active console was removed, elect
/// a new one (preferring the local console) and resize the PTY to its last
/// known dimensions.
fn unregister_console(state: &mut SessionState, session_id: &str, console_id: String) {
    let was_active = state.active_console.as_deref() == Some(console_id.as_str());
    state.consoles.remove(&console_id);

    if !was_active {
        return;
    }

    let next = elect_active_console(&state.consoles);
    if let Some(ref next_id) = next {
        if let Some(console) = state.consoles.get(next_id) {
            resize_pty(&state.pty_master, console.cols, console.rows);
            state.term.resize(TermSize {
                cols: console.cols as usize,
                rows: console.rows as usize,
            });
        }
        if state.bootstrap_pending {
            send_bootstrap(state);
        }
    }
    ERROR_LOG.log(format!(
        "[ratatui-authority-host-io] session={session_id} console={console_id} unregistered; next_active={next:?}"
    ));
    state.active_console = next;
}

/// Choose the next active console. The local console is preferred; otherwise
/// pick any remaining console deterministically (currently the first by key).
fn elect_active_console(consoles: &HashMap<String, ConsoleState>) -> Option<String> {
    if consoles.contains_key("local") {
        return Some("local".to_string());
    }
    consoles.keys().next().cloned()
}

fn resize_pty(pty_master: &File, cols: u16, rows: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl on a valid PTY master fd with TIOCSWINSZ is the standard
    // way to resize a pseudo-terminal.
    unsafe {
        let _ = libc::ioctl(pty_master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
    }
}

fn set_nonblocking(file: &mut File) {
    let fd = file.as_raw_fd();
    // SAFETY: fcntl on a valid fd returned by std::fs::File is safe.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}
