use crate::cli::{
    prepend_global_network_args, RemoteAuthorityOutputPumpCommand, RemoteAuthorityPaneDiedCommand,
    RemoteAuthorityTargetHostCommand, RemoteNetworkConfig,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError, TmuxPaneId};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
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
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorState {
    Inactive,
    Active {
        stream_id: u64,
        raw_pty_passthrough: bool,
    },
}

/// Tracks target-host event-loop counters for diagnostic output.
struct EventLoopHealth {
    last_event_time: Mutex<SystemTime>,
    events_processed: AtomicU64,
    total_input_bytes: AtomicU64,
    total_output_chunks: AtomicU64,
    mirror_active: AtomicBool,
    started_at: SystemTime,
}

impl EventLoopHealth {
    fn new() -> Self {
        Self {
            last_event_time: Mutex::new(SystemTime::now()),
            events_processed: AtomicU64::new(0),
            total_input_bytes: AtomicU64::new(0),
            total_output_chunks: AtomicU64::new(0),
            mirror_active: AtomicBool::new(false),
            started_at: SystemTime::now(),
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
mirror_active={}
time_since_last_event={}
",
            std::process::id(),
            uptime,
            self.events_processed.load(Ordering::Relaxed),
            self.total_input_bytes.load(Ordering::Relaxed),
            self.total_output_chunks.load(Ordering::Relaxed),
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

    fn clear_output_pipe_if_owner(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        owner: &str,
    ) -> Result<bool, Self::Error>;

    fn output_pipe_is_live(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        owner: &str,
    ) -> Result<bool, Self::Error>;

    fn set_output_pipe_owned(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        owner: &str,
        command: &str,
    ) -> Result<(), Self::Error>;

    fn send_input(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;

    fn set_pane_died_hook(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        command: &str,
    ) -> Result<(), Self::Error>;

    fn clear_pane_died_hook(&self, socket_name: &str, pane: &TmuxPaneId)
        -> Result<(), Self::Error>;
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

    fn clear_output_pipe_if_owner(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        owner: &str,
    ) -> Result<bool, Self::Error> {
        self.clear_pane_pipe_on_socket_if_owner(socket_name, pane, owner)
    }

    fn output_pipe_is_live(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        owner: &str,
    ) -> Result<bool, Self::Error> {
        self.pane_pipe_is_live_on_socket_for_owner(socket_name, pane, owner)
    }

    fn set_output_pipe_owned(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        owner: &str,
        command: &str,
    ) -> Result<(), Self::Error> {
        self.set_pane_pipe_on_socket_owned(socket_name, pane, owner, command)
    }

    fn send_input(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        self.send_input_to_pane_on_socket(socket_name, pane, bytes)
    }

    fn set_pane_died_hook(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        command: &str,
    ) -> Result<(), Self::Error> {
        self.set_pane_hook_on_socket(socket_name, pane, REMOTE_AUTHORITY_PANE_DIED_HOOK, command)
    }

    fn clear_pane_died_hook(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        self.unset_pane_hook_on_socket(socket_name, pane, REMOTE_AUTHORITY_PANE_DIED_HOOK)
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

    fn signal_source_session_closed(
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

    fn signal_source_session_closed(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        self.signal_source_session_closed(socket_name, target_session_name)
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
    OutputChunk { stream_id: u64, bytes: Vec<u8> },
    OutputClosed { stream_id: u64 },
    PaneDied { pane_id: String },
    TransportClosed,
}

const OUTPUT_CHANNEL_BOUND: usize = 8192;
const OUTPUT_FRAME_CACHE_CAP: usize = 1024;
const INPUT_RING_BUFFER_CAP: usize = 256 * 1024;
const INPUT_CONGESTION_HIGH_WATERMARK: usize = INPUT_RING_BUFFER_CAP * 3 / 4;
const INPUT_CONGESTION_LOW_WATERMARK: usize = INPUT_RING_BUFFER_CAP / 4;
const INPUT_DRAIN_CHUNK_MAX: usize = 4096;
const REMOTE_AUTHORITY_PANE_DIED_HOOK: &str = "pane-died[20]";

#[derive(Clone)]
enum AuthorityOutputMessage {
    TargetOutput {
        session_id: String,
        target_id: String,
        output_seq: u64,
        stream: &'static str,
        bytes: Vec<u8>,
    },
}

struct AuthorityPaneBinding {
    pane: TmuxPaneId,
}

#[derive(Clone)]
struct OutputFrameCacheEntry {
    seq: u64,
    session_id: String,
    target_id: String,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct OutputFrameCache {
    frames: VecDeque<OutputFrameCacheEntry>,
}

impl OutputFrameCache {
    fn push(&mut self, entry: OutputFrameCacheEntry) {
        if self.frames.len() == OUTPUT_FRAME_CACHE_CAP {
            self.frames.pop_front();
        }
        self.frames.push_back(entry);
    }

    fn replay_from(&self, expected_seq: u64, received_seq: u64) -> Vec<OutputFrameCacheEntry> {
        self.frames
            .iter()
            .filter(|entry| entry.seq >= expected_seq && entry.seq < received_seq)
            .cloned()
            .collect()
    }
}

struct InputRingBuffer {
    queue: Mutex<VecDeque<u8>>,
    available: Condvar,
}

impl InputRingBuffer {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(INPUT_RING_BUFFER_CAP)),
            available: Condvar::new(),
        }
    }
}

pub struct RemoteAuthorityOutputPumpRuntime;

impl RemoteAuthorityTargetHostRuntime<EmbeddedTmuxBackend, RemoteTargetPublicationRuntime> {
    pub fn from_build_env(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        let gateway = EmbeddedTmuxBackend::from_build_env().map_err(remote_authority_error)?;
        let publication_gateway =
            RemoteTargetPublicationRuntime::from_build_env_with_network(network)?;
        let current_executable = current_waitagent_executable()?;
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
        let event_socket_path =
            authority_event_socket_path(&command.transport_socket_path, &command.target_id);
        let output_listener =
            bind_output_ingest_listener(&ingest_socket_path).map_err(remote_authority_error)?;
        let event_listener =
            bind_output_ingest_listener(&event_socket_path).map_err(remote_authority_error)?;
        let mut current_binding = None;
        ensure_authority_pane_binding(self, &command, &mut current_binding, &event_socket_path)?;

        let (event_tx, event_rx) = mpsc::channel();
        let output_cache = Arc::new(Mutex::new(OutputFrameCache::default()));
        let input_buffer = Arc::new(InputRingBuffer::new());
        let input_congested = Arc::new(AtomicBool::new(false));
        let current_input_pane = Arc::new(Mutex::new(
            current_binding.as_ref().map(|binding| binding.pane.clone()),
        ));
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
                        AuthorityOutputMessage::TargetOutput { output_seq, .. } => *output_seq,
                    }
                ));
                let result = match msg {
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
        let input_drain_thread = spawn_input_drain_thread(
            self.gateway.clone(),
            command.socket_name.clone(),
            input_buffer.clone(),
            current_input_pane.clone(),
            running.clone(),
        );
        let output_thread =
            spawn_output_ingest_thread(output_listener, running.clone(), event_tx.clone());
        let pane_event_thread =
            spawn_pane_event_thread(event_listener, running.clone(), event_tx.clone());
        let mut output_seq = 0_u64;
        let mut next_stream_id = 0_u64;
        let mut mirror_state = MirrorState::Inactive;

        let health = Arc::new(EventLoopHealth::new());

        let loop_result = loop {
            let event = match event_rx.recv() {
                Ok(event) => event,
                Err(_) => break Ok(()),
            };
            match event {
                AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::OpenMirror(
                    payload,
                )) => {
                    health.record_event();
                    let pane = match ensure_authority_pane_binding(
                        self,
                        &command,
                        &mut current_binding,
                        &event_socket_path,
                    ) {
                        Ok(pane) => pane,
                        Err(error) => break Err(error),
                    };
                    if matches!(mirror_state, MirrorState::Active { .. }) {
                        match self
                            .gateway
                            .output_pipe_is_live(
                                &command.socket_name,
                                &pane,
                                &remote_mirror_pipe_owner(&command.target_id),
                            )
                            .map_err(remote_authority_error)
                        {
                            Ok(true) => {
                                if let Err(error) = self
                                    .gateway
                                    .resize_pty(
                                        &command.socket_name,
                                        &pane,
                                        payload.cols,
                                        payload.rows,
                                    )
                                    .map_err(remote_authority_error)
                                {
                                    break Err(error);
                                }
                                if let Err(error) = send_mirror_accepted_and_bootstrap(
                                    self, &command, &pane, &transport, &payload,
                                ) {
                                    break Err(error);
                                }
                                continue;
                            }
                            Ok(false) => {
                                ERROR_LOG.log(format!(
                                    "[diag-pipe] authority active mirror has no live output pipe; reactivating target={} session={} socket={} pane={}",
                                    command.target_id,
                                    command.target_session_name,
                                    command.socket_name,
                                    pane.as_str()
                                ));
                                mirror_state = MirrorState::Inactive;
                                health.mirror_active.store(false, Ordering::Relaxed);
                            }
                            Err(error) => break Err(error),
                        }
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
                    next_stream_id = next_stream_id.saturating_add(1);
                    let stream_id = next_stream_id;
                    if let Err(error) = activate_mirror(
                        self,
                        &command,
                        &pane,
                        &ingest_socket_path,
                        stream_id,
                        &payload,
                    ) {
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
                        stream_id,
                        raw_pty_passthrough: payload.raw_pty_passthrough,
                    };
                    health.mirror_active.store(true, Ordering::Relaxed);
                    if let Err(error) = send_mirror_accepted_and_bootstrap(
                        self, &command, &pane, &transport, &payload,
                    ) {
                        break Err(error);
                    }
                }
                AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::RawPtyInput(
                    payload,
                )) => {
                    health.record_event();
                    if let Err(error) = ensure_authority_pane_binding(
                        self,
                        &command,
                        &mut current_binding,
                        &event_socket_path,
                    ) {
                        break Err(error);
                    }
                    *current_input_pane
                        .lock()
                        .expect("current input pane mutex should not be poisoned") =
                        current_binding.as_ref().map(|binding| binding.pane.clone());
                    let bytes_len = payload.input_bytes.len();
                    let congested =
                        enqueue_input_bytes(&input_buffer, &payload.input_bytes, &input_congested);
                    if let Some(congested) = congested {
                        if let Err(error) = transport.send_input_congestion(congested) {
                            ERROR_LOG.log(format!(
                                "[diag-timing] target host: input congestion signal failed: {error}"
                            ));
                        }
                    }
                    health.record_input(bytes_len as u64);
                    ERROR_LOG.log(format!(
                        "[diag-timing] target host: queued RawPtyInput ({} bytes)",
                        bytes_len
                    ));
                }
                AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::SyncRequest {
                    expected_seq,
                    received_seq,
                }) => {
                    health.record_event();
                    let pane = match ensure_authority_pane_binding(
                        self,
                        &command,
                        &mut current_binding,
                        &event_socket_path,
                    ) {
                        Ok(pane) => pane,
                        Err(error) => break Err(error),
                    };
                    *current_input_pane
                        .lock()
                        .expect("current input pane mutex should not be poisoned") =
                        current_binding.as_ref().map(|binding| binding.pane.clone());
                    if let Err(error) = replay_output_frames_for_sync_request(
                        self,
                        &command,
                        &pane,
                        &transport,
                        &output_cache,
                        expected_seq,
                        received_seq,
                    ) {
                        break Err(error);
                    }
                }
                AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::ApplyResize(
                    payload,
                )) => {
                    health.record_event();
                    let pane = match ensure_authority_pane_binding(
                        self,
                        &command,
                        &mut current_binding,
                        &event_socket_path,
                    ) {
                        Ok(pane) => pane,
                        Err(error) => break Err(error),
                    };
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
                        let pane = match ensure_authority_pane_binding(
                            self,
                            &command,
                            &mut current_binding,
                            &event_socket_path,
                        ) {
                            Ok(pane) => pane,
                            Err(error) => break Err(error),
                        };
                        if let Err(error) = deactivate_mirror(self, &command, &pane) {
                            break Err(error);
                        }
                        mirror_state = MirrorState::Inactive;
                        health.mirror_active.store(false, Ordering::Relaxed);
                    }
                }
                AuthorityHostEvent::OutputChunk { stream_id, bytes } => {
                    health.record_event();
                    health.record_output();
                    ERROR_LOG.log(format!(
                        "[diag-timing] target host: received OutputChunk stream={} ({} bytes)",
                        stream_id,
                        bytes.len()
                    ));
                    if !matches!(mirror_state, MirrorState::Active { stream_id: active_stream_id, .. } if active_stream_id == stream_id)
                    {
                        continue;
                    };
                    output_seq += 1;
                    // Always use TargetOutput: capture-pane produces plain text
                    // that needs terminal-engine interpretation on the client.
                    // RawPtyOutput carries raw PTY bytes streamed through
                    // pipe-pane -O; TargetOutput carries full-screen captures
                    // from the output pump.
                    output_cache
                        .lock()
                        .expect("output frame cache mutex should not be poisoned")
                        .push(OutputFrameCacheEntry {
                            seq: output_seq,
                            session_id: command.transport_session_id.clone(),
                            target_id: command.target_id.clone(),
                            bytes: bytes.clone(),
                        });
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
                    if output_tx.send(msg).is_err() {
                        break Ok(());
                    }
                }
                AuthorityHostEvent::OutputClosed { stream_id } => {
                    health.record_event();
                    ERROR_LOG.log(format!(
                        "[diag-timing] target host: output stream closed stream={} target={}",
                        stream_id, command.target_id
                    ));
                    if matches!(mirror_state, MirrorState::Active { stream_id: active_stream_id, .. } if active_stream_id == stream_id)
                    {
                        mirror_state = MirrorState::Inactive;
                        health.mirror_active.store(false, Ordering::Relaxed);
                    }
                }
                AuthorityHostEvent::PaneDied { pane_id } => {
                    health.record_event();
                    let current_pane = current_binding
                        .as_ref()
                        .map(|binding| binding.pane.as_str().to_string());
                    ERROR_LOG.log(format!(
                        "[diag-timing] target host: pane-died event pane={} current={:?} target={}",
                        pane_id, current_pane, command.target_id
                    ));
                    if current_pane.as_deref() == Some(pane_id.as_str()) {
                        break Ok(());
                    }
                }
                AuthorityHostEvent::TransportClosed => {
                    health.record_event();
                    if matches!(mirror_state, MirrorState::Active { .. }) {
                        if let Some(binding) = current_binding.as_ref() {
                            if let Err(error) = deactivate_mirror(self, &command, &binding.pane) {
                                break Err(error);
                            }
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
        ERROR_LOG.log(format!(
            "[diag-timing] target host: event loop exited, sending TargetExited (target={}, session={})",
            command.target_id,
            command.target_session_name
        ));
        let _ = transport
            .send_target_exited(&command.transport_session_id, &command.target_session_name);
        let _ = self
            .publication_gateway
            .signal_source_session_closed(&command.socket_name, &command.target_session_name);
        if matches!(mirror_state, MirrorState::Active { .. }) {
            if let Some(binding) = current_binding.as_ref() {
                let _ = deactivate_mirror(self, &command, &binding.pane);
            }
        }
        cleanup_authority_pane_binding(self, &command, current_binding.take());
        running.store(false, Ordering::Relaxed);
        input_buffer.available.notify_all();
        let _ = UnixStream::connect(&ingest_socket_path);
        let _ = UnixStream::connect(&event_socket_path);
        let _ = fs::remove_file(&ingest_socket_path);
        let _ = fs::remove_file(&event_socket_path);
        let _ = self
            .publication_gateway
            .ensure_live_session_unregistered(&command.socket_name, &command.target_session_name);
        let _ = command_thread.join();
        let _ = output_thread.join();
        let _ = pane_event_thread.join();
        let _ = input_drain_thread.join();
        // Write final diagnostics before exiting so operators can inspect
        // event counters for this target host.
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

pub fn run_pane_died_event(command: RemoteAuthorityPaneDiedCommand) -> Result<(), LifecycleError> {
    send_pane_died_event(&command.event_socket_path, &command.pane_id);
    Ok(())
}

fn ensure_authority_pane_binding<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    current_binding: &mut Option<AuthorityPaneBinding>,
    event_socket_path: &Path,
) -> Result<TmuxPaneId, LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    let pane = runtime
        .gateway
        .target_presentation_pane(&command.socket_name, &command.target_session_name)
        .map_err(remote_authority_error)?;
    if current_binding
        .as_ref()
        .is_some_and(|binding| binding.pane == pane)
    {
        return Ok(pane);
    }

    if let Some(previous) = current_binding.take() {
        let owner = remote_mirror_pipe_owner(&command.target_id);
        let _ = runtime.gateway.clear_output_pipe_if_owner(
            &command.socket_name,
            &previous.pane,
            &owner,
        );
        let _ = runtime
            .gateway
            .clear_pane_died_hook(&command.socket_name, &previous.pane);
    }

    let pane_died_hook = remote_authority_pane_died_hook_command(
        runtime.current_executable.to_string_lossy().as_ref(),
        event_socket_path,
        pane.as_str(),
    );
    runtime
        .gateway
        .set_pane_died_hook(&command.socket_name, &pane, &pane_died_hook)
        .map_err(remote_authority_error)?;
    ERROR_LOG.log(format!(
        "[diag-authority-pane] bound target={} session={} socket={} pane={}",
        command.target_id,
        command.target_session_name,
        command.socket_name,
        pane.as_str()
    ));
    *current_binding = Some(AuthorityPaneBinding { pane: pane.clone() });
    Ok(pane)
}

fn cleanup_authority_pane_binding<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    binding: Option<AuthorityPaneBinding>,
) where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    if let Some(binding) = binding {
        let owner = remote_mirror_pipe_owner(&command.target_id);
        let _ =
            runtime
                .gateway
                .clear_output_pipe_if_owner(&command.socket_name, &binding.pane, &owner);
        let _ = runtime
            .gateway
            .clear_pane_died_hook(&command.socket_name, &binding.pane);
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
            "[diag-timing] output pump: starting, ingest={}, socket={}, stream={}",
            command.ingest_socket_path, command.socket_name, command.stream_id
        ));
        let ingest = command.ingest_socket_path.clone();
        pump_stdin_to_ingest_socket(&ingest, command.stream_id).map_err(remote_authority_error)
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

fn enqueue_input_bytes(
    input_buffer: &Arc<InputRingBuffer>,
    bytes: &[u8],
    congested: &Arc<AtomicBool>,
) -> Option<bool> {
    let mut queue = input_buffer
        .queue
        .lock()
        .expect("input ring buffer mutex should not be poisoned");
    while queue.len().saturating_add(bytes.len()) > INPUT_RING_BUFFER_CAP {
        queue.pop_front();
    }
    queue.extend(bytes.iter().copied());
    let len = queue.len();
    input_buffer.available.notify_one();
    if len >= INPUT_CONGESTION_HIGH_WATERMARK
        && congested
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        Some(true)
    } else if len <= INPUT_CONGESTION_LOW_WATERMARK
        && congested
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        Some(false)
    } else {
        None
    }
}

fn spawn_input_drain_thread<G>(
    gateway: G,
    socket_name: String,
    input_buffer: Arc<InputRingBuffer>,
    current_pane: Arc<Mutex<Option<TmuxPaneId>>>,
    running: Arc<AtomicBool>,
) -> thread::JoinHandle<()>
where
    G: RemoteTargetPtyGateway,
{
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            let bytes = {
                let mut queue = input_buffer
                    .queue
                    .lock()
                    .expect("input ring buffer mutex should not be poisoned");
                while queue.is_empty() && running.load(Ordering::Relaxed) {
                    queue = input_buffer
                        .available
                        .wait(queue)
                        .expect("input ring buffer mutex should not be poisoned");
                }
                if queue.is_empty() {
                    Vec::new()
                } else {
                    let drain_len = queue.len().min(INPUT_DRAIN_CHUNK_MAX);
                    queue.drain(..drain_len).collect::<Vec<_>>()
                }
            };
            if bytes.is_empty() {
                continue;
            }
            let pane = current_pane
                .lock()
                .expect("current input pane mutex should not be poisoned")
                .clone();
            let Some(pane) = pane else {
                continue;
            };
            if let Err(error) = gateway.send_input(&socket_name, &pane, &bytes) {
                ERROR_LOG.log(format!(
                    "[diag-timing] target host: input drain send error: {}",
                    error.to_string()
                ));
                thread::sleep(Duration::from_millis(20));
            }
        }
    })
}

fn replay_output_frames_for_sync_request<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    pane: &TmuxPaneId,
    transport: &RemoteAuthorityTransportRuntime,
    output_cache: &Arc<Mutex<OutputFrameCache>>,
    expected_seq: u64,
    received_seq: u64,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    let replay = output_cache
        .lock()
        .expect("output frame cache mutex should not be poisoned")
        .replay_from(expected_seq, received_seq);
    if !replay.is_empty() {
        for entry in replay {
            transport
                .send_sync_response(&entry.session_id, &entry.target_id, entry.seq, entry.bytes)
                .map_err(remote_authority_error)?;
        }
        return Ok(());
    }

    let screen = runtime
        .gateway
        .capture_bootstrap_screen(&command.socket_name, pane, false)
        .map_err(remote_authority_error)?;
    let (cursor_x, cursor_y) = runtime
        .gateway
        .capture_cursor_position(&command.socket_name, pane)
        .map_err(remote_authority_error)?;
    let replay = render_bootstrap_replay(&screen, cursor_x, cursor_y);
    transport
        .send_sync_response(
            &command.transport_session_id,
            &command.target_id,
            expected_seq,
            replay.into_bytes(),
        )
        .map_err(remote_authority_error)?;
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
                Ok((mut stream, _)) => {
                    let stream_id = match read_stream_id_frame(&mut stream) {
                        Ok(stream_id) => stream_id,
                        Err(error) => {
                            ERROR_LOG.log(format!(
                                "[diag-timing] ingest thread: stream id read error: {error}"
                            ));
                            continue;
                        }
                    };
                    loop {
                        match read_output_chunk_frame(&mut stream) {
                            Ok(bytes) => {
                                ERROR_LOG.log(format!(
                                    "[diag-timing] ingest thread: received chunk stream={} ({} bytes)",
                                    stream_id,
                                    bytes.len()
                                ));
                                if tx
                                    .send(AuthorityHostEvent::OutputChunk { stream_id, bytes })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(error) if error.is_unexpected_eof() => {
                                let _ = tx.send(AuthorityHostEvent::OutputClosed { stream_id });
                                break;
                            }
                            Err(error) => {
                                ERROR_LOG.log(format!(
                                    "[diag-timing] ingest thread: read error stream={stream_id}: {error}"
                                ));
                                let _ = tx.send(AuthorityHostEvent::OutputClosed { stream_id });
                                break;
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn spawn_pane_event_thread(
    listener: UnixListener,
    running: Arc<AtomicBool>,
    tx: mpsc::Sender<AuthorityHostEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut pane_id = String::new();
                    if stream.read_to_string(&mut pane_id).is_err() {
                        continue;
                    }
                    let pane_id = pane_id.trim().to_string();
                    if pane_id.is_empty() {
                        continue;
                    }
                    if tx.send(AuthorityHostEvent::PaneDied { pane_id }).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// Reads raw PTY output from stdin (piped via `pipe-pane -O`) and forwards
/// it to the ingest socket.  This streams the terminal byte stream directly
/// instead of polling `capture-pane`, so ANSI escape sequences, cursor
/// movement, and incremental output are preserved faithfully.  The bootstrap
/// replay already painted the initial screen, so this only needs to handle
/// ongoing output.
fn pump_stdin_to_ingest_socket(
    ingest_socket_path: &str,
    stream_id: u64,
) -> Result<(), RemoteAuthorityHostError> {
    let mut stdin = io::stdin().lock();
    pump_reader_to_ingest_socket(&mut stdin, ingest_socket_path, stream_id)
}

fn pump_reader_to_ingest_socket<R: Read>(
    reader: &mut R,
    ingest_socket_path: &str,
    stream_id: u64,
) -> Result<(), RemoteAuthorityHostError> {
    let mut stream = UnixStream::connect(ingest_socket_path).map_err(|e| {
        ERROR_LOG.log(format!(
            "[diag-timing] output pump: UnixStream::connect({}) failed: {e}",
            ingest_socket_path
        ));
        e
    })?;
    write_stream_id_frame(&mut stream, stream_id)?;
    ERROR_LOG.log(format!(
        "[diag-timing] output pump: connected to ingest socket, stream={}, reading from pipe-pane -O stdin",
        stream_id
    ));
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
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

fn write_stream_id_frame(
    writer: &mut impl Write,
    stream_id: u64,
) -> Result<(), RemoteAuthorityHostError> {
    writer.write_all(&stream_id.to_le_bytes())?;
    writer.flush()?;
    Ok(())
}

fn read_stream_id_frame(reader: &mut impl Read) -> Result<u64, RemoteAuthorityHostError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
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

pub fn authority_event_socket_path(transport_socket_path: &str, target_id: &str) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-authority-event-{hash}.sock"))
}

/// Path for the per-session diagnostic file written when the target host exits.
pub fn authority_diag_path(transport_socket_path: &str, target_id: &str) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-diag-{hash}.diag"))
}

fn remote_authority_output_pump_shell_command(
    executable: &str,
    ingest_socket_path: &Path,
    socket_name: &str,
    stream_id: u64,
) -> String {
    [
        shell_escape(executable),
        shell_escape("__remote-authority-output-pump"),
        shell_escape("--ingest-socket-path"),
        shell_escape(&ingest_socket_path.display().to_string()),
        shell_escape("--socket-name"),
        shell_escape(socket_name),
        shell_escape("--stream-id"),
        shell_escape(&stream_id.to_string()),
    ]
    .join(" ")
}

fn remote_authority_pane_died_hook_command(
    executable: &str,
    event_socket_path: &Path,
    pane: &str,
) -> String {
    let shell_command = [
        shell_escape(executable),
        shell_escape("__remote-authority-pane-died"),
        shell_escape("--event-socket-path"),
        shell_escape(&event_socket_path.display().to_string()),
        shell_escape("--pane-id"),
        shell_escape(pane),
    ]
    .join(" ");
    format!(
        "run-shell -b {}",
        tmux_quote_argument(&format!("{shell_command} >/dev/null 2>&1"))
    )
}

fn tmux_quote_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
fn send_pane_died_event(event_socket_path: &str, pane_id: &str) {
    if let Ok(mut stream) = UnixStream::connect(event_socket_path) {
        let _ = stream.write_all(pane_id.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }
}

fn send_mirror_accepted_and_bootstrap<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    pane: &TmuxPaneId,
    transport: &RemoteAuthorityTransportRuntime,
    payload: &crate::infra::remote_protocol::OpenMirrorRequestPayload,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    transport
        .send_open_mirror_accepted(&payload.session_id, &payload.target_id, "online")
        .map_err(remote_authority_error)?;
    emit_bootstrap(
        runtime,
        &command.socket_name,
        pane,
        transport,
        &command.transport_session_id,
        &command.target_id,
        payload.bootstrap_mode == crate::infra::remote_protocol::BootstrapMode::VisibleOnly,
    )
}

fn activate_mirror<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    pane: &TmuxPaneId,
    ingest_socket_path: &Path,
    stream_id: u64,
    payload: &crate::infra::remote_protocol::OpenMirrorRequestPayload,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    let pipe_command = remote_authority_output_pump_shell_command(
        runtime.current_executable.to_string_lossy().as_ref(),
        ingest_socket_path,
        &command.socket_name,
        stream_id,
    );
    let owner = remote_mirror_pipe_owner(&command.target_id);
    // Resize BEFORE setting up pipe-pane.  pipe-pane -I -O triggers a
    // layout recalculation in tmux that can override a subsequent resize.
    runtime
        .gateway
        .resize_pty(&command.socket_name, pane, payload.cols, payload.rows)
        .map_err(remote_authority_error)?;
    runtime
        .gateway
        .set_output_pipe_owned(&command.socket_name, pane, &owner, &pipe_command)
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
    let owner = remote_mirror_pipe_owner(&command.target_id);
    runtime
        .gateway
        .clear_output_pipe_if_owner(&command.socket_name, pane, &owner)
        .map_err(remote_authority_error)?;
    Ok(())
}

fn remote_mirror_pipe_owner(target_id: &str) -> String {
    format!("remote-authority-mirror:{target_id}")
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
