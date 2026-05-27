use crate::cli::{
    prepend_global_network_args, RemoteAuthorityOutputPumpCommand,
    RemoteAuthorityTargetHostCommand, RemoteNetworkConfig,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError, TmuxPaneId, TmuxSocketName};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::{
    RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
use crate::runtime::remote_node_session_owner_runtime::live_authority_session_socket_path;
use crate::runtime::remote_target_publication_runtime::{
    signal_publication_sender_live_session_registered,
    signal_publication_sender_live_session_unregistered, RemoteTargetPublicationRuntime,
};
use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorState {
    Inactive,
    Active { raw_pty_passthrough: bool },
}

/// Tracks event-loop health and transport/FIFO stalls for diagnostic output.
/// Writes a stall summary to a temp file when a stall is detected so that
/// operators can inspect it after the session becomes unresponsive.
struct EventLoopHealth {
    last_event_time: Mutex<SystemTime>,
    events_processed: AtomicU64,
    total_input_bytes: AtomicU64,
    total_output_chunks: AtomicU64,
    fifo_stall_count: AtomicU64,
    fifo_stalled_bytes: AtomicU64,
    mirror_active: AtomicBool,
    started_at: SystemTime,
    last_stall_warn: Mutex<SystemTime>,
}

impl EventLoopHealth {
    fn new() -> Self {
        Self {
            last_event_time: Mutex::new(SystemTime::now()),
            events_processed: AtomicU64::new(0),
            total_input_bytes: AtomicU64::new(0),
            total_output_chunks: AtomicU64::new(0),
            fifo_stall_count: AtomicU64::new(0),
            fifo_stalled_bytes: AtomicU64::new(0),
            mirror_active: AtomicBool::new(false),
            started_at: SystemTime::now(),
            last_stall_warn: Mutex::new(SystemTime::now()),
        }
    }

    fn record_event(&self) {
        self.events_processed.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut t) = self.last_event_time.lock() {
            *t = SystemTime::now();
        }
    }

    fn record_input(&self, n: u64) {
        self.total_input_bytes.fetch_add(n, Ordering::Relaxed);
    }

    fn record_output(&self) {
        self.total_output_chunks.fetch_add(1, Ordering::Relaxed);
    }

    fn record_fifo_stall(&self, n: u64) {
        self.fifo_stall_count.fetch_add(1, Ordering::Relaxed);
        self.fifo_stalled_bytes.fetch_add(n, Ordering::Relaxed);
    }

    fn maybe_log_stall(&self, transport_socket_path: &str, target_id: &str) {
        let now = SystemTime::now();
        let should_warn = self
            .last_stall_warn
            .lock()
            .map(|mut t| {
                let elapsed = now.duration_since(*t).ok();
                if elapsed.map_or(true, |d| d > Duration::from_secs(5)) {
                    *t = now;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if should_warn {
            let diag_path = authority_diag_path(transport_socket_path, target_id);
            let _ = self.write_diag(&diag_path);
            eprintln!(
                "[waitagent-diag] FIFO stall: count={} bytes={} path={}",
                self.fifo_stall_count.load(Ordering::Relaxed),
                self.fifo_stalled_bytes.load(Ordering::Relaxed),
                diag_path.display(),
            );
        }
    }

    fn write_diag(&self, path: &Path) -> std::io::Result<()> {
        let elapsed = |start: SystemTime| -> String {
            SystemTime::now()
                .duration_since(start)
                .map(|d| format!("{}.{:03}s", d.as_secs(), d.subsec_millis()))
                .unwrap_or_else(|_| "?".to_string())
        };
        let last_event = self
            .last_event_time
            .lock()
            .map(|t| elapsed(*t))
            .unwrap_or_else(|_| "?".to_string());
        let uptime = elapsed(self.started_at);
        let content = format!(
            "\
[waitagent-diag]
pid={}
uptime={}
events_processed={}
total_input_bytes={}
total_output_chunks={}
fifo_stall_count={}
fifo_stalled_bytes={}
mirror_active={}
time_since_last_event={}
",
            std::process::id(),
            uptime,
            self.events_processed.load(Ordering::Relaxed),
            self.total_input_bytes.load(Ordering::Relaxed),
            self.total_output_chunks.load(Ordering::Relaxed),
            self.fifo_stall_count.load(Ordering::Relaxed),
            self.fifo_stalled_bytes.load(Ordering::Relaxed),
            self.mirror_active.load(Ordering::Relaxed),
            last_event,
        );
        std::fs::write(path, content)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteTargetTerminalFlags {
    pub alternate_screen_active: bool,
    pub application_cursor_keys: bool,
    pub cursor_visible: bool,
}

impl Default for RemoteTargetTerminalFlags {
    fn default() -> Self {
        Self {
            alternate_screen_active: false,
            application_cursor_keys: false,
            cursor_visible: true,
        }
    }
}

pub trait RemoteTargetPtyGateway: Send + Sync + Clone + 'static {
    type Error: ToString;

    fn target_presentation_pane(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, Self::Error>;

    fn resize_pty(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(), Self::Error>;

    fn capture_bootstrap_screen(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        visible_only: bool,
    ) -> Result<String, Self::Error>;

    fn capture_cursor_position(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<(usize, usize), Self::Error>;

    fn capture_terminal_flags(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<RemoteTargetTerminalFlags, Self::Error>;

    fn clear_output_pipe(&self, socket_name: &str, pane: &TmuxPaneId) -> Result<(), Self::Error>;

    fn set_output_pipe(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        command: &str,
    ) -> Result<(), Self::Error>;

    fn send_keys_to_pane(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        keys: &str,
    ) -> Result<(), Self::Error>;
}

impl RemoteTargetPtyGateway for EmbeddedTmuxBackend {
    type Error = TmuxError;

    fn target_presentation_pane(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, Self::Error> {
        self.target_presentation_pane_on_socket(socket_name, target_session_name)
    }

    fn resize_pty(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(), Self::Error> {
        self.resize_pane_on_socket(socket_name, pane, cols, rows)
    }

    fn capture_bootstrap_screen(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        visible_only: bool,
    ) -> Result<String, Self::Error> {
        if visible_only {
            self.capture_pane_ansi_visible_on_socket(socket_name, pane.as_str())
        } else {
            self.capture_pane_ansi_on_socket(socket_name, pane.as_str())
        }
    }

    fn capture_cursor_position(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<(usize, usize), Self::Error> {
        self.pane_cursor_position_on_socket(socket_name, pane.as_str())
    }

    fn capture_terminal_flags(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<RemoteTargetTerminalFlags, Self::Error> {
        self.pane_terminal_flags_on_socket(socket_name, pane.as_str())
    }

    fn clear_output_pipe(&self, socket_name: &str, pane: &TmuxPaneId) -> Result<(), Self::Error> {
        self.clear_pane_pipe_on_socket(socket_name, pane)
    }

    fn set_output_pipe(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        command: &str,
    ) -> Result<(), Self::Error> {
        self.set_pane_pipe_on_socket(socket_name, pane, command)
    }

    fn send_keys_to_pane(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        keys: &str,
    ) -> Result<(), Self::Error> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &[
                "send-keys".to_string(),
                "-l".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                keys.to_string(),
            ],
        )
        .map(|_| ())
    }
}

pub trait RemoteAuthorityPublicationGateway: Send + Sync + Clone + 'static {
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        target_id: &str,
        transport_socket_path: &str,
    ) -> Result<PathBuf, LifecycleError>;

    fn ensure_live_session_unregistered(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;
}

impl RemoteAuthorityPublicationGateway for RemoteTargetPublicationRuntime {
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        target_id: &str,
        transport_socket_path: &str,
    ) -> Result<PathBuf, LifecycleError> {
        self.ensure_publication_sender_running(socket_name)?;
        signal_publication_sender_live_session_registered(
            socket_name,
            target_session_name,
            authority_id,
            target_id,
            transport_socket_path,
        )?;
        let authority_socket_path =
            live_authority_session_socket_path(socket_name, target_session_name);
        wait_for_ready_socket(&authority_socket_path)?;
        Ok(authority_socket_path)
    }

    fn ensure_live_session_unregistered(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        signal_publication_sender_live_session_unregistered(socket_name, target_session_name)
    }
}

pub struct RemoteAuthorityTargetHostRuntime<
    G = EmbeddedTmuxBackend,
    P = RemoteTargetPublicationRuntime,
> {
    gateway: G,
    publication_gateway: P,
    current_executable: PathBuf,
}

enum AuthorityHostEvent {
    TransportCommand(RemoteAuthorityCommand),
    OutputChunk(Vec<u8>),
    TransportClosed,
}

const OUTPUT_CHANNEL_BOUND: usize = 8192;
const OUTPUT_CACHE_CAPACITY: usize = 1024;

#[derive(Clone)]
struct OutputFrame {
    sequence: u64,
    msg: AuthorityOutputMessage,
}

#[derive(Clone)]
enum AuthorityOutputMessage {
    RawPtyOutput {
        session_id: String,
        target_id: String,
        output_seq: u64,
        bytes: Vec<u8>,
    },
    TargetOutput {
        session_id: String,
        target_id: String,
        output_seq: u64,
        stream: &'static str,
        bytes: Vec<u8>,
    },
}

struct OutputPipeGuard<G>
where
    G: RemoteTargetPtyGateway,
{
    gateway: G,
    socket_name: String,
    pane: TmuxPaneId,
    ingest_socket_path: PathBuf,
    input_fifo_path: PathBuf,
}

pub struct RemoteAuthorityOutputPumpRuntime;

impl RemoteAuthorityTargetHostRuntime<EmbeddedTmuxBackend, RemoteTargetPublicationRuntime> {
    pub fn from_build_env(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        let gateway = EmbeddedTmuxBackend::from_build_env().map_err(remote_authority_error)?;
        let publication_gateway =
            RemoteTargetPublicationRuntime::from_build_env_with_network(network)?;
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        Ok(Self::new(gateway, publication_gateway, current_executable))
    }
}

impl<G, P> RemoteAuthorityTargetHostRuntime<G, P>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    pub fn new(gateway: G, publication_gateway: P, current_executable: PathBuf) -> Self {
        Self {
            gateway,
            publication_gateway,
            current_executable,
        }
    }

    pub fn run_target_host(
        &self,
        command: RemoteAuthorityTargetHostCommand,
    ) -> Result<(), LifecycleError> {
        let pane = self
            .gateway
            .target_presentation_pane(&command.socket_name, &command.target_session_name)
            .map_err(remote_authority_error)?;
        let authority_socket_path = self
            .publication_gateway
            .ensure_live_session_registered(
                &command.socket_name,
                &command.target_session_name,
                &command.authority_id,
                &command.target_id,
                &command.transport_socket_path,
            )
            .map_err(remote_authority_error)?;
        let transport = Arc::new(
            RemoteAuthorityTransportRuntime::connect(&authority_socket_path, &command.authority_id)
                .map_err(remote_authority_error)?,
        );
        let ingest_socket_path =
            authority_output_ingest_socket_path(&command.transport_socket_path, &command.target_id);
        let input_fifo_path =
            authority_input_fifo_path(&command.transport_socket_path, &command.target_id);
        let listener =
            bind_output_ingest_listener(&ingest_socket_path).map_err(remote_authority_error)?;
        create_input_fifo(&input_fifo_path).map_err(remote_authority_error)?;
        let _output_guard = OutputPipeGuard {
            gateway: self.gateway.clone(),
            socket_name: command.socket_name.clone(),
            pane: pane.clone(),
            ingest_socket_path: ingest_socket_path.clone(),
            input_fifo_path: input_fifo_path.clone(),
        };

        let (event_tx, event_rx) = mpsc::channel();
        let (output_tx, output_rx) =
            mpsc::sync_channel::<AuthorityOutputMessage>(OUTPUT_CHANNEL_BOUND);
        let reader_transport = transport.clone();
        let reader_tx = event_tx.clone();
        let command_thread = thread::spawn(move || {
            while let Ok(command) = reader_transport.recv_command() {
                if reader_tx
                    .send(AuthorityHostEvent::TransportCommand(command))
                    .is_err()
                {
                    return;
                }
            }
            let _ = reader_tx.send(AuthorityHostEvent::TransportClosed);
        });

        let sender_transport = transport.clone();
        let output_sender_thread = thread::spawn(move || {
            while let Ok(msg) = output_rx.recv() {
                ERROR_LOG.log(format!(
                    "[diag-timing] target host: output sender sending (seq={})",
                    match &msg {
                        AuthorityOutputMessage::RawPtyOutput { output_seq, .. } => *output_seq,
                        AuthorityOutputMessage::TargetOutput { output_seq, .. } => *output_seq,
                    }
                ));
                let result = match msg {
                    AuthorityOutputMessage::RawPtyOutput {
                        session_id,
                        target_id,
                        output_seq,
                        bytes,
                    } => sender_transport.send_raw_pty_output(
                        &session_id,
                        &target_id,
                        output_seq,
                        bytes,
                    ),
                    AuthorityOutputMessage::TargetOutput {
                        session_id,
                        target_id,
                        output_seq,
                        stream,
                        bytes,
                    } => sender_transport.send_target_output(
                        &session_id,
                        &target_id,
                        output_seq,
                        stream,
                        bytes,
                    ),
                };
                if let Err(error) = result {
                    ERROR_LOG.log(format!(
                        "[diag-timing] target host: output sender send error: {error}"
                    ));
                }
            }
        });

        let running = Arc::new(AtomicBool::new(true));
        let output_thread = spawn_output_ingest_thread(listener, running.clone(), event_tx);
        let mut output_seq = 0_u64;
        let mut mirror_state = MirrorState::Inactive;

        let mut input_fifo = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&input_fifo_path)
            .map_err(remote_authority_error)?;

        let health = Arc::new(EventLoopHealth::new());
        let mut pending_input: Vec<u8> = Vec::new();
        let mut output_cache: VecDeque<OutputFrame> =
            VecDeque::with_capacity(OUTPUT_CACHE_CAPACITY);

        let loop_result = loop {
            // Drain queued input bytes to the FIFO. The FIFO is blocking so
            // the write completes once the output pump reads the data.  This
            // ensures no input is silently dropped — backpressure propagates
            // naturally through the FIFO buffer.
            if !pending_input.is_empty() {
                match input_fifo.write(&pending_input) {
                    Ok(n) if n > 0 => {
                        ERROR_LOG.log(format!(
                            "[diag-timing] target host: fifo wrote {} bytes (was pending {})",
                            n,
                            pending_input.len()
                        ));
                        pending_input.drain(..n);
                        health.record_input(n as u64);
                    }
                    Ok(_) => {} // no progress, retry next iteration
                    Err(error) => {
                        ERROR_LOG.log(format!(
                            "[diag-timing] target host: fifo write error: {error}"
                        ));
                        break Err(remote_authority_error(error));
                    }
                }
                if !pending_input.is_empty() {
                    let stuck = pending_input.len() as u64;
                    health.record_fifo_stall(stuck);
                    health.maybe_log_stall(&command.transport_socket_path, &command.target_id);
                }
            }
            let event = if pending_input.is_empty() {
                match event_rx.recv() {
                    Ok(e) => e,
                    Err(_) => break Ok(()),
                }
            } else {
                match event_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(e) => e,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
                }
            };
            match event {
                AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::OpenMirror(
                    payload,
                )) => {
                    health.record_event();
                    if matches!(mirror_state, MirrorState::Active { .. }) {
                        if let Err(error) = self
                            .gateway
                            .resize_pty(&command.socket_name, &pane, payload.cols, payload.rows)
                            .map_err(remote_authority_error)
                        {
                            break Err(error);
                        }
                        if let Err(error) = transport
                            .send_open_mirror_accepted(
                                &payload.session_id,
                                &payload.target_id,
                                "online",
                            )
                            .map_err(remote_authority_error)
                        {
                            break Err(error);
                        }
                        if let Err(error) = emit_bootstrap(
                            self,
                            &command.socket_name,
                            &pane,
                            &transport,
                            &command.transport_session_id,
                            &command.target_id,
                            payload.bootstrap_mode
                                == crate::infra::remote_protocol::BootstrapMode::VisibleOnly,
                        ) {
                            break Err(error);
                        }
                        continue;
                    }
                    if payload.target_id != command.target_id
                        || payload.session_id != command.transport_session_id
                    {
                        if let Err(error) = transport
                            .send_open_mirror_rejected(
                                &payload.session_id,
                                &payload.target_id,
                                "mirror_not_available",
                                "requested session does not match local target host",
                            )
                            .map_err(remote_authority_error)
                        {
                            break Err(error);
                        }
                        continue;
                    }
                    if let Err(error) =
                        activate_mirror(self, &command, &pane, &ingest_socket_path, &payload)
                    {
                        if transport
                            .send_open_mirror_rejected(
                                &payload.session_id,
                                &payload.target_id,
                                "mirror_not_available",
                                error.to_string(),
                            )
                            .is_err()
                        {
                            break Err(error);
                        }
                        continue;
                    }
                    mirror_state = MirrorState::Active {
                        raw_pty_passthrough: payload.raw_pty_passthrough,
                    };
                    health.mirror_active.store(true, Ordering::Relaxed);
                    if let Err(error) = transport
                        .send_open_mirror_accepted(
                            &payload.session_id,
                            &payload.target_id,
                            "online",
                        )
                        .map_err(remote_authority_error)
                    {
                        break Err(error);
                    }
                    if let Err(error) = emit_bootstrap(
                        self,
                        &command.socket_name,
                        &pane,
                        &transport,
                        &command.transport_session_id,
                        &command.target_id,
                        payload.bootstrap_mode
                            == crate::infra::remote_protocol::BootstrapMode::VisibleOnly,
                    ) {
                        break Err(error);
                    }
                }
                AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::RawPtyInput(
                    payload,
                )) => {
                    health.record_event();
                    const PENDING_INPUT_MAX: usize = 256 * 1024;
                    let input = &payload.input_bytes;
                    if pending_input.len() + input.len() <= PENDING_INPUT_MAX {
                        pending_input.extend_from_slice(input);
                        ERROR_LOG.log(format!(
                            "[diag-timing] target host: buffered RawPtyInput ({} bytes), pending={}",
                            input.len(),
                            pending_input.len()
                        ));
                    }
                }
                AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::ApplyResize(
                    payload,
                )) => {
                    health.record_event();
                    if let Err(error) = self
                        .gateway
                        .resize_pty(&command.socket_name, &pane, payload.cols, payload.rows)
                        .map_err(remote_authority_error)
                    {
                        break Err(error);
                    }
                }
                AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::CloseMirror(
                    _payload,
                )) => {
                    health.record_event();
                    if matches!(mirror_state, MirrorState::Active { .. }) {
                        if let Err(error) = deactivate_mirror(self, &command, &pane) {
                            break Err(error);
                        }
                        mirror_state = MirrorState::Inactive;
                        health.mirror_active.store(false, Ordering::Relaxed);
                    }
                }
                AuthorityHostEvent::OutputChunk(bytes) => {
                    health.record_event();
                    health.record_output();
                    ERROR_LOG.log(format!(
                        "[diag-timing] target host: received OutputChunk ({} bytes)",
                        bytes.len()
                    ));
                    if matches!(mirror_state, MirrorState::Inactive) {
                        continue;
                    };
                    output_seq += 1;
                    // Always use TargetOutput: capture-pane produces plain text
                    // that needs terminal-engine interpretation on the client.
                    // RawPtyOutput bypasses the observer and writes directly,
                    // which works for ANSI-formatted PTY streams but not for
                    // the plain-text diff produced by extract_new_output.
                    let msg = AuthorityOutputMessage::TargetOutput {
                        session_id: command.transport_session_id.clone(),
                        target_id: command.target_id.clone(),
                        output_seq,
                        stream: "pty",
                        bytes,
                    };
                    // Blocking send ensures output frames are never silently
                    // dropped. Backpressure propagates to the PTY capture
                    // source when the network is congested, which is correct:
                    // slowing the producer is better than losing data.
                    if output_tx.send(msg.clone()).is_err() {
                        break Ok(());
                    }
                    output_cache.push_back(OutputFrame {
                        sequence: output_seq,
                        msg,
                    });
                    if output_cache.len() > OUTPUT_CACHE_CAPACITY {
                        output_cache.pop_front();
                    }
                }
                AuthorityHostEvent::TransportClosed => {
                    health.record_event();
                    if matches!(mirror_state, MirrorState::Active { .. }) {
                        if let Err(error) = deactivate_mirror(self, &command, &pane) {
                            break Err(error);
                        }
                    }
                    break Ok(());
                }
            }
        };

        // Drop the output sender so the output thread can exit cleanly,
        // then stop the ingest thread.
        drop(output_tx);
        let _ = output_sender_thread.join();

        // Signal the remote side that this session is exiting cleanly.
        // Must happen *before* deactivate_mirror so the TargetExited envelope
        // reaches the __remote-main-slot event loop before the gRPC stream
        // is torn down.  Otherwise the remote sees a bare Disconnected and
        // enters the reconnecting loop.
        let _ = transport
            .send_target_exited(&command.transport_session_id, &command.target_session_name);
        if matches!(mirror_state, MirrorState::Active { .. }) {
            let _ = deactivate_mirror(self, &command, &pane);
        }
        running.store(false, Ordering::Relaxed);
        let _ = UnixStream::connect(&ingest_socket_path);
        let _ = self
            .publication_gateway
            .ensure_live_session_unregistered(&command.socket_name, &command.target_session_name);
        let _ = command_thread.join();
        let _ = output_thread.join();
        // Write final diagnostics before exiting so the operator can inspect
        // health counters after a freeze.
        let _ = health.write_diag(&authority_diag_path(
            &command.transport_socket_path,
            &command.target_id,
        ));
        loop_result
    }

    pub fn run_output_pump(
        &self,
        command: RemoteAuthorityOutputPumpCommand,
    ) -> Result<(), LifecycleError> {
        RemoteAuthorityOutputPumpRuntime::run(command)
    }
}

pub(crate) fn remote_authority_target_host_args(
    socket_name: &str,
    target_session_name: &str,
    transport_session_id: &str,
    authority_id: &str,
    target_id: &str,
    transport_socket_path: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-authority-target-host".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
            "--target-session-name".to_string(),
            target_session_name.to_string(),
            "--transport-session-id".to_string(),
            transport_session_id.to_string(),
            "--authority-id".to_string(),
            authority_id.to_string(),
            "--target-id".to_string(),
            target_id.to_string(),
            "--transport-socket-path".to_string(),
            transport_socket_path.to_string(),
        ],
        network,
    )
}

impl RemoteAuthorityOutputPumpRuntime {
    pub fn run(command: RemoteAuthorityOutputPumpCommand) -> Result<(), LifecycleError> {
        ERROR_LOG.log(format!(
            "[diag-timing] output pump: starting, ingest={}, fifo={}, socket={}, pane={}",
            command.ingest_socket_path, command.input_fifo_path, command.socket_name, command.pane
        ));
        let input_fifo_path = command.input_fifo_path.clone();
        let socket = command.socket_name.clone();
        let pane = command.pane.clone();
        let tmux_bin = tmux_binary_path();
        let tmux_sock = tmux_socket_path(&socket);
        let input_thread = thread::spawn(move || -> Result<(), RemoteAuthorityHostError> {
            let mut fifo = OpenOptions::new().read(true).open(&input_fifo_path)?;
            let mut buffer = [0_u8; 4096];
            loop {
                let read = fifo.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                // Use send-keys to deliver input to the PTY.  pipe-pane -I is
                // unreliable: if pipe-pane -O sends stdin EOF (empty pane),
                // tmux may close the stdout pipe as well.
                // send-keys -H expects each byte as a separate hex argument.
                let mut args: Vec<String> = vec![
                    "-S".to_string(),
                    tmux_sock.clone(),
                    "send-keys".to_string(),
                    "-H".to_string(),
                    "-t".to_string(),
                    pane.clone(),
                ];
                for b in &buffer[..read] {
                    args.push(format!("{b:02x}"));
                }
                let result = std::process::Command::new(&tmux_bin).args(&args).output();
                if let Err(e) = result {
                    ERROR_LOG.log(format!("[diag-timing] output pump: send-keys failed: {e}"));
                    break;
                }
            }
            Ok(())
        });

        let ingest = command.ingest_socket_path.clone();
        let pump_result = pump_stdin_to_ingest_socket(&ingest);
        match input_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) if pump_result.is_ok() => return Err(remote_authority_error(error)),
            Ok(Err(_)) => {}
            Err(_) if pump_result.is_ok() => {
                return Err(remote_authority_error(RemoteAuthorityHostError::new(
                    "remote authority input pump panicked",
                )))
            }
            Err(_) => {}
        }
        pump_result.map_err(remote_authority_error)
    }
}

impl<G> Drop for OutputPipeGuard<G>
where
    G: RemoteTargetPtyGateway,
{
    fn drop(&mut self) {
        let _ = self
            .gateway
            .clear_output_pipe(&self.socket_name, &self.pane);
        let _ = fs::remove_file(&self.ingest_socket_path);
        let _ = fs::remove_file(&self.input_fifo_path);
    }
}

fn bind_output_ingest_listener(
    socket_path: &Path,
) -> Result<UnixListener, RemoteAuthorityHostError> {
    if socket_path.exists() {
        let _ = fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    Ok(listener)
}

fn create_input_fifo(fifo_path: &Path) -> Result<(), RemoteAuthorityHostError> {
    if fifo_path.exists() {
        let _ = fs::remove_file(fifo_path);
    }
    let c_path = std::ffi::CString::new(fifo_path.as_os_str().as_bytes())
        .map_err(|_| RemoteAuthorityHostError::new("input fifo path contains interior NUL"))?;
    let result = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if result == -1 {
        return Err(RemoteAuthorityHostError::new(format!(
            "failed to create input fifo {}: {}",
            fifo_path.display(),
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn spawn_output_ingest_thread(
    listener: UnixListener,
    running: Arc<AtomicBool>,
    tx: mpsc::Sender<AuthorityHostEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => loop {
                    match read_output_chunk_frame(&mut stream) {
                        Ok(bytes) => {
                            ERROR_LOG.log(format!(
                                "[diag-timing] ingest thread: received chunk ({} bytes)",
                                bytes.len()
                            ));
                            if tx.send(AuthorityHostEvent::OutputChunk(bytes)).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.is_unexpected_eof() => break,
                        Err(error) => {
                            ERROR_LOG
                                .log(format!("[diag-timing] ingest thread: read error: {error}"));
                            break;
                        }
                    }
                },
                Err(_) => break,
            }
        }
    })
}

fn pump_reader_to_ingest_socket(
    mut reader: impl Read,
    ingest_socket_path: &str,
) -> Result<(), RemoteAuthorityHostError> {
    let mut stream = UnixStream::connect(ingest_socket_path).map_err(|e| {
        ERROR_LOG.log(format!(
            "[diag-timing] output pump: UnixStream::connect({}) failed: {e}",
            ingest_socket_path
        ));
        e
    })?;
    ERROR_LOG.log(format!(
        "[diag-timing] output pump: connected to ingest socket, starting pump loop"
    ));
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            ERROR_LOG.log("[diag-timing] output pump: stdin EOF, exiting pump loop".to_string());
            break;
        }
        write_output_chunk_frame(&mut stream, &buffer[..read])?;
    }
    Ok(())
}

/// Reads raw PTY output from stdin (piped via `pipe-pane -O`) and forwards
/// it to the ingest socket.  This streams the terminal byte stream directly
/// instead of polling `capture-pane`, so ANSI escape sequences, cursor
/// movement, and incremental output are preserved faithfully.  The bootstrap
/// replay already painted the initial screen, so this only needs to handle
/// ongoing output.
fn pump_stdin_to_ingest_socket(ingest_socket_path: &str) -> Result<(), RemoteAuthorityHostError> {
    let mut stream = UnixStream::connect(ingest_socket_path).map_err(|e| {
        ERROR_LOG.log(format!(
            "[diag-timing] output pump: UnixStream::connect({}) failed: {e}",
            ingest_socket_path
        ));
        e
    })?;
    ERROR_LOG.log(
        "[diag-timing] output pump: connected to ingest socket, reading from pipe-pane -O stdin"
            .to_string(),
    );
    let mut stdin = io::stdin().lock();
    let mut buffer = [0_u8; 4096];
    loop {
        match stdin.read(&mut buffer) {
            Ok(0) => {
                // pipe-pane closed — pane was destroyed or tmux shut down
                ERROR_LOG.log("[diag-timing] output pump: stdin EOF, exiting".to_string());
                break;
            }
            Ok(read) => {
                write_output_chunk_frame(&mut stream, &buffer[..read])?;
            }
            Err(e) => {
                ERROR_LOG.log(format!(
                    "[diag-timing] output pump: stdin read error: {e}, exiting"
                ));
                return Err(RemoteAuthorityHostError::new(format!("stdin read: {e}")));
            }
        }
    }
    Ok(())
}

/// Polls `capture-pane` on the target pane and forwards output chunks to the
/// ingest socket.  Kept as dead code for reference; the active path uses
/// `pump_stdin_to_ingest_socket` (pipe-pane -O).
#[allow(dead_code)]
fn pump_capture_pane_to_ingest_socket(
    ingest_socket_path: &str,
    socket_name: &str,
    pane: &str,
) -> Result<(), RemoteAuthorityHostError> {
    let mut stream = UnixStream::connect(ingest_socket_path).map_err(|e| {
        ERROR_LOG.log(format!(
            "[diag-timing] output pump: UnixStream::connect({}) failed: {e}",
            ingest_socket_path
        ));
        e
    })?;
    ERROR_LOG.log(
        "[diag-timing] output pump: connected to ingest socket, starting capture-pane poll loop"
            .to_string(),
    );
    let tmux_bin = tmux_binary_path();
    let mut last_text = String::new();
    loop {
        let output = std::process::Command::new(&tmux_bin)
            .args([
                "-S",
                &tmux_socket_path(socket_name),
                "capture-pane",
                "-e",
                "-p",
                "-t",
                pane,
            ])
            .output()
            .map_err(|e| RemoteAuthorityHostError::new(format!("capture-pane failed: {e}")))?;
        if !output.status.success() {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        let content = String::from_utf8_lossy(&output.stdout).into_owned();
        if content != last_text {
            let esc = b"\x1b[2J\x1b[H";
            let payload: Vec<u8> = esc.iter().chain(content.as_bytes()).cloned().collect();
            write_output_chunk_frame(&mut stream, &payload)?;
            last_text = content;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Returns the suffix of `current` that differs from `previous`,
/// skipping any common prefix. Used for plain-text diff of screen captures.
fn extract_new_output(previous: &str, current: &str) -> Vec<u8> {
    let common_len = previous
        .chars()
        .zip(current.chars())
        .take_while(|(a, b)| a == b)
        .count();
    current[common_len..].as_bytes().to_vec()
}

/// Strips ANSI escape sequences (CSI, OSC, etc.) from a string, returning
/// only the visible text content. Used to compare screen captures for changes.
fn strip_ansi_escape_sequences(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip the ESC and the following sequence
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    chars.next(); // consume '['
                                  // Skip until we find a letter (the terminator)
                    while let Some(&p) = chars.peek() {
                        if p.is_ascii_alphabetic() || p == '~' {
                            chars.next(); // consume terminator
                            break;
                        }
                        chars.next();
                    }
                } else if next == ']' {
                    chars.next(); // consume ']'
                                  // OSC sequence ends with BEL (\x07) or ST (\x1b\\)
                    while let Some(&p) = chars.peek() {
                        if p == '\x07' || p == '\x1b' {
                            if p == '\x1b' {
                                chars.next(); // ESC
                                chars.next(); // '\\'
                            } else {
                                chars.next(); // BEL
                            }
                            break;
                        }
                        chars.next();
                    }
                } else {
                    // Other escape types, skip ESC
                }
            }
        } else if c == '\r' {
            // Normalize \r\n to \n
            if chars.peek() == Some(&'\n') {
                result.push('\n');
                chars.next();
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Path to the vendored tmux binary in the waitagent data directory.
fn tmux_binary_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/share/waitagent/tmux")
}

/// Path to the tmux socket for a waitagent workspace.
fn tmux_socket_path(socket_name: &str) -> String {
    format!("/tmp/tmux-1000/{socket_name}")
}

fn write_output_chunk_frame(
    writer: &mut impl Write,
    bytes: &[u8],
) -> Result<(), RemoteAuthorityHostError> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| RemoteAuthorityHostError::new("output chunk exceeds u32 framing"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_output_chunk_frame(reader: &mut impl Read) -> Result<Vec<u8>, RemoteAuthorityHostError> {
    let mut len_bytes = [0_u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn authority_output_ingest_socket_path(
    transport_socket_path: &str,
    target_id: &str,
) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-authority-output-{hash}.sock"))
}

pub fn authority_input_fifo_path(transport_socket_path: &str, target_id: &str) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-authority-input-{hash}.fifo"))
}

/// Path for the per-session diagnostic file written on FIFO stall.
/// Survives the target-host process so it can be inspected after a freeze.
pub fn authority_diag_path(transport_socket_path: &str, target_id: &str) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-diag-{hash}.diag"))
}

fn remote_authority_output_pump_shell_command(
    executable: &str,
    ingest_socket_path: &Path,
    input_fifo_path: &Path,
    socket_name: &str,
    pane: &str,
) -> String {
    [
        shell_escape(executable),
        shell_escape("__remote-authority-output-pump"),
        shell_escape("--ingest-socket-path"),
        shell_escape(&ingest_socket_path.display().to_string()),
        shell_escape("--input-fifo-path"),
        shell_escape(&input_fifo_path.display().to_string()),
        shell_escape("--socket-name"),
        shell_escape(socket_name),
        shell_escape("--pane"),
        shell_escape(pane),
    ]
    .join(" ")
}

fn activate_mirror<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    pane: &TmuxPaneId,
    ingest_socket_path: &Path,
    payload: &crate::infra::remote_protocol::OpenMirrorRequestPayload,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    let pipe_command = remote_authority_output_pump_shell_command(
        runtime.current_executable.to_string_lossy().as_ref(),
        ingest_socket_path,
        &authority_input_fifo_path(&command.transport_socket_path, &command.target_id),
        &command.socket_name,
        pane.as_str(),
    );
    runtime
        .gateway
        .clear_output_pipe(&command.socket_name, pane)
        .map_err(remote_authority_error)?;
    // Resize BEFORE setting up pipe-pane.  pipe-pane -I -O triggers a
    // layout recalculation in tmux that can override a subsequent resize.
    runtime
        .gateway
        .resize_pty(&command.socket_name, pane, payload.cols, payload.rows)
        .map_err(remote_authority_error)?;
    runtime
        .gateway
        .set_output_pipe(&command.socket_name, pane, &pipe_command)
        .map_err(remote_authority_error)?;
    Ok(())
}

fn emit_bootstrap<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    socket_name: &str,
    pane: &TmuxPaneId,
    transport: &RemoteAuthorityTransportRuntime,
    session_id: &str,
    target_id: &str,
    visible_only: bool,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    let screen = runtime
        .gateway
        .capture_bootstrap_screen(socket_name, pane, visible_only)
        .map_err(remote_authority_error)?;
    let (cursor_x, cursor_y) = runtime
        .gateway
        .capture_cursor_position(socket_name, pane)
        .map_err(remote_authority_error)?;
    let flags = runtime
        .gateway
        .capture_terminal_flags(socket_name, pane)
        .map_err(remote_authority_error)?;
    ERROR_LOG.log(format!(
        "[diag-bootstrap] screen len={} cursor=({},{}) visible_only={}",
        screen.len(),
        cursor_x,
        cursor_y,
        visible_only
    ));
    let replay = render_bootstrap_replay(&screen, cursor_x, cursor_y);
    ERROR_LOG.log(format!(
        "[diag-bootstrap] replay len={}, first 120 bytes: {:?}",
        replay.len(),
        &replay.as_bytes()[..replay.len().min(120)]
    ));
    let last_chunk_seq = if replay.is_empty() { 0 } else { 1 };
    if !replay.is_empty() {
        transport
            .send_mirror_bootstrap_chunk(
                session_id,
                target_id,
                1,
                "pty",
                replay.as_bytes().to_vec(),
            )
            .map_err(remote_authority_error)?;
    }
    transport
        .send_mirror_bootstrap_complete(
            session_id,
            target_id,
            last_chunk_seq,
            flags.alternate_screen_active,
            flags.application_cursor_keys,
            flags.cursor_visible,
        )
        .map_err(remote_authority_error)?;
    Ok(())
}

fn render_bootstrap_replay(screen: &str, cursor_x: usize, cursor_y: usize) -> String {
    let mut replay = String::from("\x1b[2J\x1b[H");
    for (index, line) in screen.lines().enumerate() {
        replay.push_str(&format!("\x1b[{};1H{}", index + 1, line));
    }
    replay.push_str(&format!(
        "\x1b[{};{}H",
        cursor_y.saturating_add(1),
        cursor_x.saturating_add(1)
    ));
    replay
}

fn deactivate_mirror<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    pane: &TmuxPaneId,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    runtime
        .gateway
        .clear_output_pipe(&command.socket_name, pane)
        .map_err(remote_authority_error)?;
    Ok(())
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn stable_socket_hash(values: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for value in values {
        for byte in value.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteAuthorityHostError {
    message: String,
    io_kind: Option<io::ErrorKind>,
}

impl RemoteAuthorityHostError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            io_kind: None,
        }
    }

    fn is_unexpected_eof(&self) -> bool {
        self.io_kind == Some(io::ErrorKind::UnexpectedEof)
    }
}

impl fmt::Display for RemoteAuthorityHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteAuthorityHostError {}

impl From<io::Error> for RemoteAuthorityHostError {
    fn from(value: io::Error) -> Self {
        Self {
            message: value.to_string(),
            io_kind: Some(value.kind()),
        }
    }
}

fn remote_authority_error(error: impl ToString) -> LifecycleError {
    LifecycleError::Io(
        "failed to run remote authority target host".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn wait_for_ready_socket(socket_path: &Path) -> Result<(), LifecycleError> {
    for _ in 0..100 {
        if socket_path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(LifecycleError::Protocol(format!(
        "authority live-session socket did not become ready at {}",
        socket_path.display()
    )))
}

#[cfg(test)]
mod remote_authority_target_host_runtime_test;
