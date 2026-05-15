use crate::cli::{
    prepend_global_network_args, RemoteAuthorityOutputPumpCommand,
    RemoteAuthorityTargetHostCommand, RemoteNetworkConfig,
};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError, TmuxPaneId};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::{
    RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
use crate::runtime::remote_node_session_owner_runtime::live_authority_session_socket_path;
use crate::runtime::remote_target_publication_runtime::{
    signal_publication_sender_live_session_registered,
    signal_publication_sender_live_session_unregistered, RemoteTargetPublicationRuntime,
};
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

const OUTPUT_CHANNEL_BOUND: usize = 500;

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
                    eprintln!("[authority] output send error: {error}");
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
            .custom_flags(libc::O_NONBLOCK)
            .open(&input_fifo_path)
            .map_err(remote_authority_error)?;

        let health = Arc::new(EventLoopHealth::new());
        let mut pending_input: Vec<u8> = Vec::new();

        let loop_result = loop {
            // Drain queued input bytes without blocking the event loop.
            // The FIFO is opened with O_NONBLOCK so `write()` returns a
            // short count when the kernel buffer is congested, rather than
            // blocking the entire event loop (which would freeze both input
            // and output). Unwritten bytes remain in pending_input and are
            // retried on the next iteration.
            if !pending_input.is_empty() {
                match input_fifo.write(&pending_input) {
                    Ok(n) if n > 0 => {
                        pending_input.drain(..n);
                        health.record_input(n as u64);
                    }
                    Ok(_) => {} // no progress, retry next iteration
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => {
                        break Err(remote_authority_error(error));
                    }
                }
                if !pending_input.is_empty() {
                    // FIFO still congested — track for diagnostics.
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
                    // Always queue to pending_input. The drain at the top
                    // of each loop iteration flushes as much as the FIFO
                    // accepts without blocking. With O_NONBLOCK, a
                    // congested FIFO never stalls the event loop.
                    const PENDING_INPUT_MAX: usize = 64 * 1024;
                    let input = &payload.input_bytes;
                    if pending_input.len() + input.len() > PENDING_INPUT_MAX {
                        let excess = pending_input.len() + input.len() - PENDING_INPUT_MAX;
                        pending_input.drain(..excess.min(pending_input.len()));
                    }
                    pending_input.extend_from_slice(input);
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
                    let raw_pty_passthrough = match mirror_state {
                        MirrorState::Inactive => continue,
                        MirrorState::Active {
                            raw_pty_passthrough,
                        } => raw_pty_passthrough,
                    };
                    output_seq += 1;
                    let msg = if raw_pty_passthrough {
                        AuthorityOutputMessage::RawPtyOutput {
                            session_id: command.transport_session_id.clone(),
                            target_id: command.target_id.clone(),
                            output_seq,
                            bytes,
                        }
                    } else {
                        AuthorityOutputMessage::TargetOutput {
                            session_id: command.transport_session_id.clone(),
                            target_id: command.target_id.clone(),
                            output_seq,
                            stream: "pty",
                            bytes,
                        }
                    };
                    // Non-blocking send to the dedicated output thread. When
                    // the network is congested the channel fills up and we
                    // drop old chunks rather than blocking the event loop.
                    // This keeps the event loop responsive for input while
                    // the output thread deals with backpressure.
                    if output_tx.try_send(msg).is_err() {
                        eprintln!(
                            "[authority] dropping output chunk {} (channel full)",
                            output_seq
                        );
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
        let input_fifo_path = command.input_fifo_path.clone();
        let input_thread = thread::spawn(move || -> Result<(), RemoteAuthorityHostError> {
            let mut fifo = OpenOptions::new().read(true).open(input_fifo_path)?;
            let mut stdout = io::stdout().lock();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = fifo.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                stdout.write_all(&buffer[..read])?;
                stdout.flush()?;
            }
            Ok(())
        });

        let stdin = io::stdin();
        let pump_result = pump_reader_to_ingest_socket(stdin.lock(), &command.ingest_socket_path);
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
                            if tx.send(AuthorityHostEvent::OutputChunk(bytes)).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.is_unexpected_eof() => break,
                        Err(_) => break,
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
    let mut stream = UnixStream::connect(ingest_socket_path)?;
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        write_output_chunk_frame(&mut stream, &buffer[..read])?;
    }
    Ok(())
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
) -> String {
    [
        shell_escape(executable),
        shell_escape("__remote-authority-output-pump"),
        shell_escape("--ingest-socket-path"),
        shell_escape(&ingest_socket_path.display().to_string()),
        shell_escape("--input-fifo-path"),
        shell_escape(&input_fifo_path.display().to_string()),
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
    );
    runtime
        .gateway
        .clear_output_pipe(&command.socket_name, pane)
        .map_err(remote_authority_error)?;
    runtime
        .gateway
        .set_output_pipe(&command.socket_name, pane, &pipe_command)
        .map_err(remote_authority_error)?;
    runtime
        .gateway
        .resize_pty(&command.socket_name, pane, payload.cols, payload.rows)
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
    let replay = render_bootstrap_replay(&screen, cursor_x, cursor_y);
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
