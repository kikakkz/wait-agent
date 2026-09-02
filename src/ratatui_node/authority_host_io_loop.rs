use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;
use std::collections::{HashMap, VecDeque};
#[cfg(unix)]
use std::fs::File;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use super::local_session::render_grid_line;
use super::runtime::SharedState;
use super::state_event::StateEvent;
use crate::platform::wake_pipe::{WakeRead, WakeWrite};

/// Child process type hosted by the authority-host IO loop.
///
/// On Unix this is `std::process::Child`; on Windows `std::process::Command`
/// cannot attach a ConPTY, so spawning goes through `platform::pty` and yields
/// a `ConPtyChild` with the same `id`/`try_wait` surface.
#[cfg(unix)]
pub(crate) type SessionChild = std::process::Child;
#[cfg(windows)]
pub(crate) type SessionChild = crate::platform::pty::ConPtyChild;

#[cfg(unix)]
const SIGCHLD_TOKEN: usize = 0;
#[cfg(unix)]
const SHUTDOWN_TOKEN: usize = 1;
#[cfg(unix)]
const REQUEST_WAKE_TOKEN: usize = 2;
#[cfg(unix)]
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
        #[cfg(unix)]
        pty_master: File,
        #[cfg(windows)]
        conpty: crate::platform::pty::ConPty,
        child: SessionChild,
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
    SendBootstrap {
        session_id: String,
    },
    /// Raw ConPTY output delivered by a session reader thread (Windows only;
    /// on Unix the IO loop polls the PTY master fd directly).
    #[cfg(windows)]
    PtyOutput {
        session_id: String,
        bytes: Vec<u8>,
    },
}

/// Handle used by other threads to send requests to `AuthorityHostIoLoop`.
///
/// Every successful send also writes to an internal wake pipe so the IO loop
/// never blocks in `poll` while requests are pending.
#[derive(Clone)]
pub(crate) struct AuthorityHostIoHandle {
    tx: mpsc::Sender<AuthorityHostIoRequest>,
    wake: Arc<Mutex<Option<WakeWrite>>>,
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
                let _ = wake.wake();
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
    #[cfg(unix)]
    pty_master: File,
    /// ConPTY on Windows. The reader thread owns a duplicated output handle;
    /// dropping this state closes the pseudoconsole and kills the child tree.
    #[cfg(windows)]
    conpty: crate::platform::pty::ConPty,
    /// Declared after the PTY/ConPTY so that teardown kills the child first:
    /// closing the PTY (Unix) or pseudoconsole (Windows) is what terminates
    /// the hosted process tree.
    child: SessionChild,
    #[cfg(unix)]
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
    /// Set when a new output sender is installed but the bootstrap snapshot has
    /// not been sent yet. The authority-host target reader owns a short stability
    /// window after `OpenMirrorRequest`; once that window expires it sends
    /// `SendBootstrap`, which clears this flag and emits the snapshot.
    bootstrap_pending: bool,
}

impl SessionState {
    /// Apply a new size to the session's PTY (Unix) or ConPTY (Windows).
    fn resize_pty(&self, cols: u16, rows: u16) {
        #[cfg(unix)]
        let _ = crate::platform::pty::resize(&self.pty_master, cols, rows);
        #[cfg(windows)]
        let _ = crate::platform::pty::resize(&self.conpty, cols, rows);
    }
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
    shutdown_write: Option<WakeWrite>,
    request_wake: Arc<Mutex<Option<WakeWrite>>>,
}

impl AuthorityHostIoLoop {
    pub(crate) fn start(shared: Arc<SharedState>) -> Result<Self, LifecycleError> {
        let (tx, rx) = mpsc::channel::<AuthorityHostIoRequest>();
        let (shutdown_read, shutdown_write) =
            crate::platform::wake_pipe::pair().map_err(|error| {
                LifecycleError::Io(
                    "failed to create authority host IO shutdown pipe".to_string(),
                    error,
                )
            })?;
        let (request_read, request_write) =
            crate::platform::wake_pipe::pair().map_err(|error| {
                LifecycleError::Io(
                    "failed to create authority host IO request wake pipe".to_string(),
                    error,
                )
            })?;
        let request_wake = Arc::new(Mutex::new(Some(request_write)));
        #[cfg(windows)]
        let request_tx_for_readers = tx.clone();
        std::thread::spawn(move || {
            #[cfg(unix)]
            let result = run_io_loop(shared, rx, shutdown_read, request_read);
            #[cfg(windows)]
            let result = run_io_loop(
                shared,
                rx,
                shutdown_read,
                request_read,
                request_tx_for_readers,
            );
            if let Err(error) = result {
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
            let _ = shutdown_write.wake();
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

/// Unix IO loop: polls the PTY master fds, the SIGCHLD pipe and the wake pipes
/// with `polling::Poller`.
#[cfg(unix)]
fn run_io_loop(
    shared: Arc<SharedState>,
    rx: mpsc::Receiver<AuthorityHostIoRequest>,
    mut shutdown_read: WakeRead,
    mut request_read: WakeRead,
) -> Result<(), LifecycleError> {
    let mut poller = polling::Poller::new().map_err(|error| {
        LifecycleError::Io(
            "failed to create authority host IO poller".to_string(),
            io::Error::new(io::ErrorKind::Other, error),
        )
    })?;

    #[cfg(unix)]
    let mut sig_read = {
        let (sig_read, sig_write) = UnixStream::pair().map_err(|error| {
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
        sig_read
    };
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
            &shared,
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
            #[cfg(unix)]
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
                        let qualified_target_id =
                            format!("{}:{session_id}", shared.local_authority_id());
                        if let Some(monitor) = shared.process_monitor() {
                            monitor.unregister_session(&qualified_target_id);
                        }
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

/// How long the Windows IO loop blocks on the request channel before waking to
/// reap children (there is no SIGCHLD on Windows).
#[cfg(windows)]
const CHILD_EXIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Windows IO loop: anonymous pipes cannot be polled, so each session owns a
/// blocking reader thread (see `spawn_conpty_reader`) that feeds output back
/// through the request channel. The loop thread blocks on `recv_timeout` so
/// `check_child_exits` runs periodically.
#[cfg(windows)]
fn run_io_loop(
    shared: Arc<SharedState>,
    rx: mpsc::Receiver<AuthorityHostIoRequest>,
    mut shutdown_read: WakeRead,
    mut request_read: WakeRead,
    request_tx: mpsc::Sender<AuthorityHostIoRequest>,
) -> Result<(), LifecycleError> {
    let mut sessions: HashMap<String, SessionState> = HashMap::new();
    let mut buf = [0u8; 64];
    loop {
        drain_requests(&rx, &shared, &request_tx, &mut sessions)?;

        // The wake pipes are non-blocking TCP streams on Windows. Drain any
        // wake bytes and stop when shutdown was requested; requests themselves
        // travel on the channel, so no poller is needed.
        match shutdown_read.read(&mut buf) {
            Ok(n) if n > 0 => return Ok(()),
            _ => {}
        }
        drain_request_wake(&mut request_read);

        match rx.recv_timeout(CHILD_EXIT_POLL_INTERVAL) {
            // Requests are drained at the top of the next iteration.
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => check_child_exits(&mut sessions, &shared),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Spawn a blocking reader thread for `session_id`'s ConPTY output pipe.
///
/// The thread sends raw output bytes back through the IO loop's request
/// channel (`PtyOutput`) and never touches `SharedState`. It exits when the
/// pipe breaks (pseudoconsole teardown) or when the IO loop is gone.
#[cfg(windows)]
fn spawn_conpty_reader(
    session_id: &str,
    conpty: &crate::platform::pty::ConPty,
    request_tx: &mpsc::Sender<AuthorityHostIoRequest>,
) {
    let session_id = session_id.to_string();
    let mut output = match conpty.output().try_clone() {
        Ok(output) => output,
        Err(error) => {
            ERROR_LOG.log(format!(
                "[ratatui-authority-host-io] failed to clone ConPTY output for {session_id}: {error}"
            ));
            return;
        }
    };
    let request_tx = request_tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; PTY_READ_BUF_SIZE];
        loop {
            match output.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = buf[..n].to_vec();
                    if request_tx
                        .send(AuthorityHostIoRequest::PtyOutput {
                            session_id: session_id.clone(),
                            bytes,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-authority-host-io] conpty read error for {session_id}: {error}"
                    ));
                    break;
                }
            }
        }
    });
}

fn drain_requests(
    rx: &mpsc::Receiver<AuthorityHostIoRequest>,
    shared: &Arc<SharedState>,
    #[cfg(unix)] poller: &mut polling::Poller,
    #[cfg(windows)] request_tx: &mpsc::Sender<AuthorityHostIoRequest>,
    sessions: &mut HashMap<String, SessionState>,
    #[cfg(unix)] token_to_session: &mut HashMap<usize, String>,
    #[cfg(unix)] next_token: &mut usize,
) -> Result<(), LifecycleError> {
    while let Ok(request) = rx.try_recv() {
        match request {
            AuthorityHostIoRequest::WriteInput { session_id, bytes } => {
                if let Some(state) = sessions.get_mut(&session_id) {
                    append_input_to_pending_write(state, &bytes);
                    #[cfg(unix)]
                    drain_pending_write(state, poller);
                    #[cfg(windows)]
                    drain_pending_write(state);
                }
            }
            #[cfg(windows)]
            AuthorityHostIoRequest::PtyOutput { session_id, bytes } => {
                if let Some(state) = sessions.get_mut(&session_id) {
                    forward_output(state, &bytes);
                    flush_output_buffer(state);
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
                #[cfg(unix)]
                mut pty_master,
                #[cfg(windows)]
                conpty,
                child,
                output_tx,
                cols,
                rows,
            } => {
                #[cfg(unix)]
                let token = {
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
                                    format!(
                                        "failed to add authority host pty {session_id} to poller"
                                    ),
                                    io::Error::new(io::ErrorKind::Other, error),
                                )
                            })?;
                    }
                    token_to_session.insert(token, session_id.clone());
                    token
                };

                #[cfg(windows)]
                spawn_conpty_reader(&session_id, &conpty, request_tx);

                // Register the session with the event-driven process monitor so
                // remote viewers see the actual command running inside the shell
                // (e.g. chrome, vi) instead of always falling back to "bash".
                if let Some(monitor) = shared.process_monitor() {
                    let qualified_target_id =
                        format!("{}:{session_id}", shared.local_authority_id());
                    // Windows has no PTY foreground-process fd for the monitor;
                    // the process monitor tracks the tree by pid instead.
                    #[cfg(unix)]
                    let pty_master_fd = pty_master.as_raw_fd();
                    #[cfg(windows)]
                    let pty_master_fd = 0;
                    monitor.register_session(
                        qualified_target_id,
                        child.id(),
                        pty_master_fd,
                        Box::new(String::new),
                    );
                }

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
                        #[cfg(unix)]
                        pty_master,
                        #[cfg(windows)]
                        conpty,
                        child,
                        #[cfg(unix)]
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
                    let qualified_target_id =
                        format!("{}:{session_id}", shared.local_authority_id());
                    if let Some(monitor) = shared.process_monitor() {
                        monitor.unregister_session(&qualified_target_id);
                    }
                    #[cfg(unix)]
                    {
                        let _ = poller.delete(&state.pty_master);
                        token_to_session.remove(&state.token);
                    }
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
                    // Always defer bootstrap until a viewer console is activated.
                    // SetOutputSender is only sent while spawning a new authority
                    // host bridge (OpenMirror gRPC), before the viewer's console
                    // is known. Sending immediately here races with the subsequent
                    // OpenMirrorRequest/ApplyResize and can produce a double
                    // bootstrap on reconnect.
                    state.bootstrap_pending = true;
                }
            }
            AuthorityHostIoRequest::SendBootstrap { session_id } => {
                if let Some(state) = sessions.get_mut(&session_id) {
                    send_bootstrap(state);
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
#[cfg(unix)]
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
    let start = std::time::Instant::now();
    let bootstrap = bootstrap_ansi_for_term(&state.term);
    let generated_bytes = bootstrap.len();
    if let Some(output_tx) = state.output_tx.as_ref() {
        let _ = output_tx.send(bootstrap);
    }
    state.bootstrap_pending = false;
    ERROR_LOG.log(format!(
        "[timing] authority host bootstrap sent cols={} rows={} bytes={} elapsed_us={}",
        state.term.grid().columns(),
        state.term.grid().screen_lines(),
        generated_bytes,
        start.elapsed().as_micros()
    ));
}

/// Maximum scrollback history lines to include in a bootstrap frame. Capping
/// this bounds the reconnect payload for long-running sessions.
const MAX_BOOTSTRAP_HISTORY_LINES: usize = 1000;

/// Render the current terminal screen and scrollback history as an ANSI byte
/// sequence that reproduces the visible content and cursor position when fed
/// to a fresh terminal emulator.
///
/// Used to bootstrap a newly attached remote console so it sees the prompt
/// and prior history immediately instead of a blank pane until new PTY output
/// arrives.
fn bootstrap_ansi_for_term(term: &Term<VoidListener>) -> Vec<u8> {
    let grid = term.grid();
    let screen_lines = grid.screen_lines();
    let columns = grid.columns();
    let display_offset = grid.display_offset() as i32;
    let total_lines = grid.total_lines();
    let history_len = total_lines.saturating_sub(screen_lines);
    let history_to_render = history_len.min(MAX_BOOTSTRAP_HISTORY_LINES);

    let mut payload = Vec::with_capacity(total_lines * (columns * 8 + 32) + 64);
    // Hide cursor, clear screen, and move to home to draw the bootstrap frame
    // without intermediate flicker.
    payload.extend_from_slice(b"\x1b[?25l\x1b[2J\x1b[H");

    // Render scrollback history first, then the visible screen. Lines are drawn
    // using absolute row positioning so the bootstrap is independent of the
    // viewer's terminal size.
    for offset in (1..=history_to_render).rev() {
        let line = Line(-(offset as i32));
        let row = history_to_render - offset + 1;
        let (_, styled) = render_grid_line(grid, line, columns);
        payload.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
        payload.extend_from_slice(styled.as_bytes());
    }
    for row in 0..screen_lines {
        let line = Line(row as i32 - display_offset);
        let absolute_row = history_to_render + row + 1;
        let (_, styled) = render_grid_line(grid, line, columns);
        payload.extend_from_slice(format!("\x1b[{absolute_row};1H").as_bytes());
        payload.extend_from_slice(styled.as_bytes());
    }

    let point = grid.cursor.point;
    let cursor_col = point.column.0 as u16 + 1;
    let cursor_row = (point.line.0 + display_offset + history_to_render as i32) as u16 + 1;
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
fn drain_request_wake(request_read: &mut WakeRead) {
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

/// Write `bytes` to the session's PTY master (Unix) or ConPTY input pipe
/// (Windows). The Unix PTY is non-blocking; the ConPTY pipe is blocking.
fn write_to_pty(state: &mut SessionState, bytes: &[u8]) -> io::Result<usize> {
    #[cfg(unix)]
    {
        state.pty_master.write(bytes)
    }
    #[cfg(windows)]
    {
        state.conpty.input().write(bytes)
    }
}

/// Flush the session's PTY output side after a write.
fn flush_pty(state: &mut SessionState) {
    #[cfg(unix)]
    let _ = state.pty_master.flush();
    #[cfg(windows)]
    let _ = state.conpty.input().flush();
}

fn append_input_to_pending_write(state: &mut SessionState, bytes: &[u8]) {
    if state.pending_write.is_empty() {
        // Attempt an immediate write. If it succeeds fully we avoid buffering;
        // otherwise queue the remainder (for the writable event on Unix, or a
        // blocking retry on Windows).
        match write_to_pty(state, bytes) {
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
    flush_pty(state);
}

fn drain_pending_write(state: &mut SessionState, #[cfg(unix)] poller: &mut polling::Poller) {
    if state.pending_write.is_empty() {
        #[cfg(unix)]
        {
            // We are only interested in readability when there is nothing to write.
            // Keep the PTY in level-triggered mode; `Poller::modify` defaults to
            // oneshot, which would drop subsequent output events between writes.
            let _ = poller.modify_with_mode(
                &state.pty_master,
                polling::Event::readable(state.token),
                polling::PollMode::Level,
            );
        }
        return;
    }

    let mut remaining = std::mem::take(&mut state.pending_write);
    let mut offset = 0;
    while offset < remaining.len() {
        match write_to_pty(state, &remaining[offset..]) {
            Ok(n) => offset += n,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-authority-host-io] pty write error: {error}"
                ));
                remaining.clear();
                #[cfg(unix)]
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
        remaining.drain(..offset);
    }
    state.pending_write = remaining;

    #[cfg(unix)]
    {
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
    }
    flush_pty(state);
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
        state.resize_pty(cols, rows);
        state.term.resize(TermSize {
            cols: cols as usize,
            rows: rows as usize,
        });
        // Do not send bootstrap here. The authority-host target reader defers
        // bootstrap for a short stability window after OpenMirrorRequest to
        // avoid redraws for connections that drop immediately.
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
            state.resize_pty(console.cols, console.rows);
            state.term.resize(TermSize {
                cols: console.cols as usize,
                rows: console.rows as usize,
            });
        }
        // Bootstrap is triggered by the authority-host target reader after its
        // stability window, not by console election here.
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

#[cfg(unix)]
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::cli::RemoteNetworkConfig;
    use std::process::Command;

    fn make_term(cols: usize, rows: usize) -> Term<VoidListener> {
        Term::new(Config::default(), &TermSize { cols, rows }, VoidListener)
    }

    fn make_test_session_state(output_tx: Option<mpsc::Sender<Vec<u8>>>) -> SessionState {
        let pty = rustix_openpty::openpty(None, None).expect("openpty should succeed");
        let pty_master = File::from(pty.controller);
        // A short-lived child is enough: the test path does not wait on it.
        let child = Command::new("false").spawn().expect("spawn should succeed");
        SessionState {
            pty_master,
            child,
            token: 100,
            output_tx,
            output_buffer: VecDeque::new(),
            pending_write: Vec::new(),
            consoles: HashMap::new(),
            active_console: None,
            term: Term::new(
                Config::default(),
                &TermSize { cols: 80, rows: 24 },
                VoidListener,
            ),
            parser: ansi::Processor::new(),
            bootstrap_pending: false,
        }
    }

    #[test]
    fn set_output_sender_defers_bootstrap_until_send_bootstrap() {
        let (output_tx, output_rx) = mpsc::channel();
        let mut sessions = HashMap::new();
        let mut state = make_test_session_state(Some(output_tx.clone()));
        state.active_console = Some("console".to_string());
        sessions.insert("sess".to_string(), state);

        let (req_tx, req_rx) = mpsc::channel();
        let shared = SharedState::new(RemoteNetworkConfig::default()).expect("shared state");
        let mut poller = polling::Poller::new().expect("poller");
        let mut token_to_session = HashMap::new();
        let mut next_token = 1000;

        req_tx
            .send(AuthorityHostIoRequest::SetOutputSender {
                session_id: "sess".to_string(),
                output_tx: output_tx.clone(),
            })
            .unwrap();
        drain_requests(
            &req_rx,
            &shared,
            &mut poller,
            &mut sessions,
            &mut token_to_session,
            &mut next_token,
        )
        .expect("drain should succeed");

        assert!(
            output_rx.try_recv().is_err(),
            "SetOutputSender should not emit bootstrap immediately"
        );
        assert!(
            sessions.get("sess").unwrap().bootstrap_pending,
            "bootstrap should be pending"
        );

        // Resize should activate the console but must not emit bootstrap; the
        // authority-host target reader controls the stable window.
        req_tx
            .send(AuthorityHostIoRequest::Resize {
                session_id: "sess".to_string(),
                console_id: "console".to_string(),
                cols: 80,
                rows: 24,
            })
            .unwrap();
        drain_requests(
            &req_rx,
            &shared,
            &mut poller,
            &mut sessions,
            &mut token_to_session,
            &mut next_token,
        )
        .expect("drain should succeed");

        assert!(
            output_rx.try_recv().is_err(),
            "Resize should not emit bootstrap while pending"
        );
        assert!(
            sessions.get("sess").unwrap().bootstrap_pending,
            "bootstrap should still be pending after Resize"
        );

        req_tx
            .send(AuthorityHostIoRequest::SendBootstrap {
                session_id: "sess".to_string(),
            })
            .unwrap();
        drain_requests(
            &req_rx,
            &shared,
            &mut poller,
            &mut sessions,
            &mut token_to_session,
            &mut next_token,
        )
        .expect("drain should succeed");

        let bootstrap = output_rx
            .try_recv()
            .expect("SendBootstrap should emit the deferred bootstrap");
        let text = String::from_utf8_lossy(&bootstrap);
        assert!(
            text.contains("\x1b[2J"),
            "bootstrap should contain clear-screen sequence: {text}"
        );
        assert!(
            !sessions.get("sess").unwrap().bootstrap_pending,
            "bootstrap should no longer be pending"
        );
    }

    #[test]
    fn bootstrap_ansi_includes_scrollback_history() {
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut term = make_term(10, 2);
        // Push four lines into a 2-row terminal so the first two become scrollback.
        parser.advance(&mut term, b"line1\nline2\nline3\nline4\n");

        let bootstrap = bootstrap_ansi_for_term(&term);
        let text = String::from_utf8_lossy(&bootstrap);

        assert!(
            text.contains("line1"),
            "bootstrap should contain scrollback line1: {text}"
        );
        assert!(
            text.contains("line2"),
            "bootstrap should contain scrollback line2: {text}"
        );
        assert!(
            text.contains("line3"),
            "bootstrap should contain visible line3: {text}"
        );
        assert!(
            text.contains("line4"),
            "bootstrap should contain visible line4: {text}"
        );
    }
}
