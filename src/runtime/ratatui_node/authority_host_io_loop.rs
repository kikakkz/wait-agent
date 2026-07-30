use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use super::runtime::SharedState;
use super::state_event::StateEvent;

const SIGCHLD_TOKEN: usize = 0;
const INITIAL_SESSION_TOKEN: usize = 1;
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
        cols: u16,
        rows: u16,
    },
    RegisterSession {
        session_id: String,
        pty_master: File,
        child: Child,
        output_tx: Option<mpsc::Sender<Vec<u8>>>,
    },
    UnregisterSession {
        session_id: String,
    },
    SetOutputSender {
        session_id: String,
        output_tx: mpsc::Sender<Vec<u8>>,
    },
}

struct SessionState {
    pty_master: File,
    child: Child,
    token: usize,
    output_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Buffered PTY output produced before an output sender was installed.
    /// Kept small; once a sender is attached the buffer is flushed.
    output_buffer: VecDeque<u8>,
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
}

impl AuthorityHostIoLoop {
    pub(crate) fn start(shared: Arc<SharedState>) -> Result<Self, LifecycleError> {
        let (tx, rx) = mpsc::channel::<AuthorityHostIoRequest>();
        std::thread::spawn(move || {
            if let Err(error) = run_io_loop(shared, rx) {
                ERROR_LOG.log(format!(
                    "[ratatui-authority-host-io] loop exited with error: {error}"
                ));
            }
        });
        Ok(Self { tx })
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<AuthorityHostIoRequest> {
        self.tx.clone()
    }
}

fn run_io_loop(
    shared: Arc<SharedState>,
    rx: mpsc::Receiver<AuthorityHostIoRequest>,
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

    let mut sessions: HashMap<String, SessionState> = HashMap::new();
    let mut token_to_session: HashMap<usize, String> = HashMap::new();
    let mut next_token: usize = INITIAL_SESSION_TOKEN;
    let mut events = polling::Events::new();
    let shutdown = Arc::new(AtomicBool::new(false));

    while !shutdown.load(Ordering::Relaxed) {
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
        match poller.wait(&mut events, Some(Duration::from_millis(100))) {
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

            let Some(session_id) = token_to_session.get(&event.key).cloned() else {
                continue;
            };
            if event.readable {
                let dead = read_pty(&session_id, &mut sessions);
                if dead {
                    if let Some(state) = sessions.remove(&session_id) {
                        let _ = poller.delete(&state.pty_master);
                        token_to_session.remove(&state.token);
                    }
                    let _ = shared
                        .state_sender()
                        .send(StateEvent::AuthorityHostSessionPtyClosed {
                            session_id: session_id.to_string(),
                        });
                }
            }
        }
    }

    Ok(())
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
                    write_pty_nonblocking(&mut state.pty_master, &bytes);
                }
            }
            AuthorityHostIoRequest::Resize {
                session_id,
                cols,
                rows,
            } => {
                if let Some(state) = sessions.get_mut(&session_id) {
                    resize_pty(&state.pty_master, cols, rows);
                }
            }
            AuthorityHostIoRequest::RegisterSession {
                session_id,
                mut pty_master,
                child,
                output_tx,
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
                sessions.insert(
                    session_id,
                    SessionState {
                        pty_master,
                        child,
                        token,
                        output_tx,
                        output_buffer: VecDeque::new(),
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
                    flush_output_buffer(state);
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

fn check_child_exits(sessions: &mut HashMap<String, SessionState>, shared: &Arc<SharedState>) {
    for (session_id, state) in sessions.iter_mut() {
        match state.child.try_wait() {
            Ok(Some(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                let _ = shared
                    .state_sender()
                    .send(StateEvent::AuthorityHostSessionChildExited {
                        session_id: session_id.clone(),
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

fn write_pty_nonblocking(pty_master: &mut File, bytes: &[u8]) {
    let mut offset = 0;
    while offset < bytes.len() {
        match pty_master.write(&bytes[offset..]) {
            Ok(n) => offset += n,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-authority-host-io] pty write error: {error}"
                ));
                break;
            }
        }
    }
    let _ = pty_master.flush();
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
