use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
use crate::domain::session_catalog::{ConsoleLocation, ManagedSessionRecord, SessionTransport};
use crate::infra::base64::encode_base64;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, RemoteConsoleDescriptor,
};
use crate::infra::remote_transport_codec::RemoteTransportCodecError;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_connection_runtime::{
    AuthorityConnectionGuard, AuthorityConnectionRequest, AuthorityConnectionStarter,
    AuthorityTransportEvent, QueuedAuthorityStreamSink, QueuedAuthorityStreamStarter,
};
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_main_slot_runtime::{RemoteAttachmentBinding, RemoteMainSlotRuntime};
use crate::runtime::remote_observer_runtime::{RemoteObserverRuntime, RemoteObserverSnapshot};
use crate::runtime::remote_transport_runtime::{LocalNodeMailbox, RemoteConnectionRegistry};
use crate::terminal::{TerminalRuntime, TerminalSize};
use std::fmt;
use std::io::{self, Read, Write};
use std::os::raw::{c_int, c_void};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const SIGWINCH: c_int = 28;
const HIDE_CURSOR_ESCAPE: &str = "\x1b[?25l";
const SHOW_CURSOR_ESCAPE: &str = "\x1b[?25h";
const TARGET_PRESENCE_POLL_INTERVAL: Duration = Duration::from_millis(250);

static REMOTE_PANE_SIGWINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" {
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

pub struct RemoteMainSlotPaneRuntime {
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    authority_connections: Box<dyn AuthorityConnectionStarter>,
    external_authority_streams: Option<QueuedAuthorityStreamSink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteInteractSignal {
    ConsoleInputStarted,
    ConsoleSubmit,
    ManualReturnToPicker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteInteractSurfaceSpec {
    pub socket_name: String,
    pub surface_scope: String,
    pub target: String,
    pub console_id: String,
    pub console_host_id: String,
    pub console_location: ConsoleLocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthorityTransportStatus {
    WaitingForRemoteAuthority,
    Connected,
    Disconnected,
    Failed(String),
}

impl RemoteMainSlotPaneRuntime {
    pub fn from_build_env_with_external_authority_streams() -> Result<Self, LifecycleError> {
        Self::from_build_env_with_external_authority_streams_and_network(
            RemoteNetworkConfig::default(),
        )
    }

    pub fn from_build_env_with_external_authority_streams_and_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env().map_err(remote_pane_error)?,
        );
        Ok(Self::new_with_external_authority_streams_and_network(
            target_registry,
            current_executable,
            network,
        ))
    }

    pub fn new(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        authority_connections: Box<dyn AuthorityConnectionStarter>,
        current_executable: PathBuf,
        _network: RemoteNetworkConfig,
    ) -> Self {
        Self::new_with_optional_external_authority_streams(
            target_registry,
            authority_connections,
            None,
            current_executable,
        )
    }

    fn new_with_optional_external_authority_streams(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        authority_connections: Box<dyn AuthorityConnectionStarter>,
        external_authority_streams: Option<QueuedAuthorityStreamSink>,
        _current_executable: PathBuf,
    ) -> Self {
        Self {
            target_registry,
            authority_connections,
            external_authority_streams,
        }
    }

    pub fn new_with_external_authority_streams(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        current_executable: PathBuf,
    ) -> Self {
        Self::new_with_external_authority_streams_and_network(
            target_registry,
            current_executable,
            RemoteNetworkConfig::default(),
        )
    }

    pub fn new_with_external_authority_streams_and_network(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        current_executable: PathBuf,
        _network: RemoteNetworkConfig,
    ) -> Self {
        let (starter, sink) = QueuedAuthorityStreamStarter::channel();
        Self::new_with_optional_external_authority_streams(
            target_registry,
            Box::new(starter),
            Some(sink),
            current_executable,
        )
    }

    pub fn submit_external_authority_stream(
        &self,
        stream: UnixStream,
    ) -> Result<(), LifecycleError> {
        let sink = self.external_authority_stream_submitter()?;
        sink.submit(stream).map_err(|_| {
            LifecycleError::Protocol(
                "remote main-slot external authority stream consumer is unavailable".to_string(),
            )
        })
    }

    pub(crate) fn external_authority_stream_submitter(
        &self,
    ) -> Result<QueuedAuthorityStreamSink, LifecycleError> {
        self.external_authority_streams
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                LifecycleError::Protocol(
                "remote main-slot pane runtime is not configured for external authority streams"
                    .to_string(),
            )
            })
    }

    pub(crate) fn start_authority_connection(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Box<dyn AuthorityConnectionGuard>> {
        self.authority_connections
            .start_connection(request, registry, tx)
    }

    pub fn run(&self, command: RemoteMainSlotCommand) -> Result<(), LifecycleError> {
        self.run_surface(main_slot_surface_spec(&command))
    }

    pub(crate) fn run_surface(
        &self,
        spec: RemoteInteractSurfaceSpec,
    ) -> Result<(), LifecycleError> {
        self.run_surface_with_signal_sink(spec, |_| {})
    }

    pub(crate) fn run_surface_with_signal_sink<F>(
        &self,
        spec: RemoteInteractSurfaceSpec,
        mut on_signal: F,
    ) -> Result<(), LifecycleError>
    where
        F: FnMut(RemoteInteractSignal),
    {
        let target = self.resolve_remote_target(&spec.target, "remote interact surface")?;
        let mut terminal = TerminalRuntime::stdio();
        let initial_size = terminal.current_size_or_default();
        let _raw_mode = terminal.enter_raw_mode()?;
        let _cursor_guard = RemotePaneCursorGuard::hide().map_err(|error| {
            LifecycleError::Io("failed to hide remote interact cursor".to_string(), error)
        })?;

        let registry = RemoteConnectionRegistry::new();
        let remote_runtime = RemoteMainSlotRuntime::with_registry(registry.clone());
        let mailbox = remote_runtime
            .ensure_local_observer_connection(spec.console_host_id.clone())
            .ok_or_else(|| {
                LifecycleError::Protocol(
                    "remote observer connection registry is not available".to_string(),
                )
            })?;
        let mut observer = RemoteObserverRuntime::new(
            mailbox.clone(),
            usize::from(initial_size.cols),
            usize::from(initial_size.rows),
        );

        let (event_tx, event_rx) = mpsc::channel();
        spawn_input_thread(event_tx.clone());
        let resize_watcher = spawn_resize_watcher(event_tx.clone()).map_err(remote_pane_error)?;
        spawn_mailbox_watcher(mailbox, event_tx.clone());
        let target_presence = Arc::new(Mutex::new(true));
        spawn_target_presence_watcher(
            self.target_registry.clone(),
            spec.target.clone(),
            target_presence.clone(),
            event_tx.clone(),
        );
        let authority_transport_socket_path =
            authority_transport_socket_path(&spec.socket_name, &spec.surface_scope, &spec.target);
        let authority_tx = authority_transport_event_sender(event_tx.clone());
        let _authority_listener = self
            .start_authority_connection(
                AuthorityConnectionRequest {
                    socket_path: authority_transport_socket_path.clone(),
                    authority_id: target.address.authority_id().to_string(),
                },
                registry.clone(),
                authority_tx,
            )
            .map_err(remote_pane_error)?;
        let waiting_authority_status = AuthorityTransportStatus::WaitingForRemoteAuthority;
        thread::spawn(move || {
            let _keep_resize_watcher_alive = resize_watcher;
            thread::park();
        });
        let mut console_seq = 0u64;
        let mut input_signal_decoder = RemoteInteractInputSignalDecoder::default();
        let mut binding = None;
        let mut authority_status = if remote_runtime.has_connection(target.address.authority_id()) {
            AuthorityTransportStatus::Connected
        } else {
            waiting_authority_status.clone()
        };
        if matches!(authority_status, AuthorityTransportStatus::Connected) {
            binding = activate_surface_target(
                &remote_runtime,
                &target,
                &spec,
                &initial_size,
                &mut observer,
            )
            .map(Some)?;
        }
        draw_remote_snapshot(
            &terminal,
            &target,
            binding.as_ref(),
            &observer.snapshot(),
            &authority_status,
        )?;

        loop {
            match event_rx.recv() {
                Ok(RemotePaneEvent::MailboxUpdated) => {
                    observer.sync().map_err(remote_protocol_error)?;
                    draw_remote_snapshot(
                        &terminal,
                        &target,
                        binding.as_ref(),
                        &observer.snapshot(),
                        &authority_status,
                    )?;
                }
                Ok(RemotePaneEvent::Resize) => {
                    if let Ok(Some(size)) = terminal.capture_resize() {
                        if let Some(binding) = binding.as_ref() {
                            remote_runtime.send_pty_resize(
                                &target,
                                binding,
                                usize::from(size.cols),
                                usize::from(size.rows),
                            )?;
                        }
                    }
                    draw_remote_snapshot(
                        &terminal,
                        &target,
                        binding.as_ref(),
                        &observer.snapshot(),
                        &authority_status,
                    )?;
                }
                Ok(RemotePaneEvent::AuthorityTransport(event)) => match event {
                    AuthorityTransportEvent::Connected => {
                        authority_status = authority_status_from_runtime(
                            &remote_runtime,
                            &target,
                            target_is_present(&target_presence),
                            &waiting_authority_status,
                        );
                        if binding.is_none()
                            && matches!(authority_status, AuthorityTransportStatus::Connected)
                        {
                            match activate_surface_target(
                                &remote_runtime,
                                &target,
                                &spec,
                                &terminal.current_size_or_default(),
                                &mut observer,
                            ) {
                                Ok(activated) => {
                                    binding = Some(activated);
                                }
                                Err(error) => {
                                    authority_status =
                                        AuthorityTransportStatus::Failed(error.to_string());
                                }
                            }
                        }
                        draw_remote_snapshot(
                            &terminal,
                            &target,
                            binding.as_ref(),
                            &observer.snapshot(),
                            &authority_status,
                        )?;
                    }
                    AuthorityTransportEvent::Disconnected => {
                        authority_status = authority_status_from_runtime(
                            &remote_runtime,
                            &target,
                            target_is_present(&target_presence),
                            &waiting_authority_status,
                        );
                        binding = None;
                        draw_remote_snapshot(
                            &terminal,
                            &target,
                            binding.as_ref(),
                            &observer.snapshot(),
                            &authority_status,
                        )?;
                    }
                    AuthorityTransportEvent::Failed(message) => {
                        authority_status = AuthorityTransportStatus::Failed(message);
                        draw_remote_snapshot(
                            &terminal,
                            &target,
                            binding.as_ref(),
                            &observer.snapshot(),
                            &authority_status,
                        )?;
                    }
                    AuthorityTransportEvent::Envelope(envelope) => {
                        apply_authority_envelope(&remote_runtime, &target, &envelope)
                            .map_err(remote_protocol_error)?;
                    }
                },
                Ok(RemotePaneEvent::TargetPresenceChanged(is_present)) => {
                    authority_status = authority_status_from_runtime(
                        &remote_runtime,
                        &target,
                        is_present,
                        &waiting_authority_status,
                    );
                    draw_remote_snapshot(
                        &terminal,
                        &target,
                        binding.as_ref(),
                        &observer.snapshot(),
                        &authority_status,
                    )?;
                }
                Ok(RemotePaneEvent::Input(bytes)) => {
                    for signal in input_signal_decoder.feed(&spec, &bytes) {
                        on_signal(signal);
                    }
                    if should_exit_surface_locally(&spec, &bytes) {
                        return Ok(());
                    }
                    if let Some(binding) = binding.as_ref() {
                        console_seq += 1;
                        remote_runtime.send_console_input(
                            &target,
                            binding,
                            console_seq,
                            encode_base64(&bytes),
                        )?;
                    }
                }
                Err(_) => return Ok(()),
            }
        }
    }

    fn resolve_remote_target(
        &self,
        target_id: &str,
        surface_label: &str,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        let session = self
            .target_registry
            .find_target(target_id)
            .map_err(remote_pane_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "unknown remote target `{}` for {surface_label}",
                    target_id
                ))
            })?;
        if session.address.transport() != &SessionTransport::RemotePeer {
            return Err(LifecycleError::Protocol(format!(
                "target `{}` is not a remote target",
                target_id
            )));
        }
        Ok(session)
    }
}

fn activate_surface_target(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    spec: &RemoteInteractSurfaceSpec,
    size: &TerminalSize,
    observer: &mut RemoteObserverRuntime,
) -> Result<RemoteAttachmentBinding, LifecycleError> {
    let binding = remote_runtime.activate_target(
        target,
        RemoteConsoleDescriptor {
            console_id: spec.console_id.clone(),
            console_host_id: spec.console_host_id.clone(),
            location: spec.console_location,
        },
        usize::from(size.cols),
        usize::from(size.rows),
    )?;
    observer.sync().map_err(remote_protocol_error)?;
    Ok(binding)
}

fn should_exit_surface_locally(spec: &RemoteInteractSurfaceSpec, bytes: &[u8]) -> bool {
    spec.console_location == ConsoleLocation::ServerConsole && bytes.contains(&0x1d)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RemoteInteractInputSignalDecoder {
    pending: Vec<u8>,
    input_in_progress: bool,
}

impl RemoteInteractInputSignalDecoder {
    fn feed(
        &mut self,
        spec: &RemoteInteractSurfaceSpec,
        bytes: &[u8],
    ) -> Vec<RemoteInteractSignal> {
        self.pending.extend_from_slice(bytes);
        let mut signals = Vec::new();

        loop {
            if self.pending.is_empty() {
                break;
            }

            if spec.console_location == ConsoleLocation::ServerConsole
                && self.pending.first() == Some(&0x1d)
            {
                self.pending.drain(..1);
                self.input_in_progress = false;
                signals.push(RemoteInteractSignal::ManualReturnToPicker);
                continue;
            }

            if self.pending.starts_with(b"\x1bOM") {
                self.pending.drain(..3);
                self.push_submit_signals(&mut signals);
                continue;
            }

            if self.pending.starts_with(b"\x1b[13u") {
                self.pending.drain(..5);
                self.push_submit_signals(&mut signals);
                continue;
            }

            if self.pending.starts_with(b"\r\n") {
                self.pending.drain(..2);
                self.push_submit_signals(&mut signals);
                continue;
            }

            if self.pending.first() == Some(&b'\r') || self.pending.first() == Some(&b'\n') {
                self.pending.drain(..1);
                self.push_submit_signals(&mut signals);
                continue;
            }

            if is_partial_remote_submit_sequence(&self.pending) {
                break;
            }

            self.pending.drain(..1);
            if !self.input_in_progress {
                self.input_in_progress = true;
                signals.push(RemoteInteractSignal::ConsoleInputStarted);
            }
        }

        signals
    }

    fn push_submit_signals(&mut self, signals: &mut Vec<RemoteInteractSignal>) {
        if !self.input_in_progress {
            signals.push(RemoteInteractSignal::ConsoleInputStarted);
        }
        self.input_in_progress = false;
        signals.push(RemoteInteractSignal::ConsoleSubmit);
    }
}

fn is_partial_remote_submit_sequence(pending: &[u8]) -> bool {
    [
        b"\x1b".as_slice(),
        b"\x1b[".as_slice(),
        b"\x1bO".as_slice(),
        b"\x1b[1".as_slice(),
        b"\x1b[13".as_slice(),
    ]
    .iter()
    .any(|pattern| pattern.starts_with(pending))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemotePaneEvent {
    Input(Vec<u8>),
    Resize,
    MailboxUpdated,
    AuthorityTransport(AuthorityTransportEvent),
    TargetPresenceChanged(bool),
}

struct RemotePaneResizeWatcher {
    _writer: UnixStream,
}

struct RemotePaneCursorGuard {
    visible_on_drop: bool,
}

impl RemotePaneCursorGuard {
    fn hide() -> io::Result<Self> {
        write_escape(HIDE_CURSOR_ESCAPE)?;
        Ok(Self {
            visible_on_drop: true,
        })
    }
}

impl Drop for RemotePaneCursorGuard {
    fn drop(&mut self) {
        if self.visible_on_drop {
            let _ = write_escape(SHOW_CURSOR_ESCAPE);
        }
    }
}

impl Drop for RemotePaneResizeWatcher {
    fn drop(&mut self) {
        REMOTE_PANE_SIGWINCH_WRITE_FD.store(-1, Ordering::Relaxed);
    }
}

fn spawn_input_thread(tx: mpsc::Sender<RemotePaneEvent>) {
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; 64];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if tx
                        .send(RemotePaneEvent::Input(buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_resize_watcher(tx: mpsc::Sender<RemotePaneEvent>) -> io::Result<RemotePaneResizeWatcher> {
    let (mut reader, writer) = UnixStream::pair()?;
    REMOTE_PANE_SIGWINCH_WRITE_FD.store(writer.as_raw_fd(), Ordering::Relaxed);
    unsafe {
        signal(SIGWINCH, remote_pane_sigwinch_handler);
    }

    thread::spawn(move || {
        let mut buffer = [0_u8; 64];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(RemotePaneEvent::Resize).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(RemotePaneResizeWatcher { _writer: writer })
}

fn spawn_mailbox_watcher(mailbox: LocalNodeMailbox, tx: mpsc::Sender<RemotePaneEvent>) {
    thread::spawn(move || {
        let mut seen = mailbox.snapshot().len();
        loop {
            mailbox.wait_for_growth(seen);
            let current = mailbox.snapshot().len();
            if current <= seen {
                continue;
            }
            seen = current;
            if tx.send(RemotePaneEvent::MailboxUpdated).is_err() {
                break;
            }
        }
    });
}

fn spawn_target_presence_watcher(
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    target_id: String,
    state: Arc<Mutex<bool>>,
    tx: mpsc::Sender<RemotePaneEvent>,
) {
    thread::spawn(move || {
        let mut last_present = true;
        loop {
            let is_present = target_registry
                .find_target(&target_id)
                .ok()
                .flatten()
                .is_some();
            {
                let mut guard = state
                    .lock()
                    .expect("target presence mutex should not be poisoned");
                *guard = is_present;
            }
            if is_present != last_present {
                last_present = is_present;
                if tx
                    .send(RemotePaneEvent::TargetPresenceChanged(is_present))
                    .is_err()
                {
                    break;
                }
            }
            thread::sleep(TARGET_PRESENCE_POLL_INTERVAL);
        }
    });
}

fn target_is_present(state: &Arc<Mutex<bool>>) -> bool {
    *state
        .lock()
        .expect("target presence mutex should not be poisoned")
}

pub(crate) fn apply_authority_envelope(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<(), RemoteSocketTransportError> {
    match &envelope.payload {
        ControlPlanePayload::OpenMirrorAccepted(_) => Ok(()),
        ControlPlanePayload::OpenMirrorRejected(payload) => {
            Err(RemoteSocketTransportError::new(format!(
                "remote mirror open rejected for `{}`: {}",
                payload.target_id, payload.message
            )))
        }
        ControlPlanePayload::TargetOutput(payload) => {
            if envelope.sender_id != target.address.authority_id() {
                return Err(RemoteSocketTransportError::new(format!(
                    "authority envelope sender `{}` does not match target authority `{}`",
                    envelope.sender_id,
                    target.address.authority_id()
                )));
            }
            remote_runtime
                .send_target_output(
                    target,
                    payload.output_seq,
                    payload.stream,
                    payload.bytes_base64.clone(),
                )
                .map_err(|error| RemoteSocketTransportError::new(error.to_string()))
        }
        ControlPlanePayload::MirrorBootstrapChunk(payload) => {
            if envelope.sender_id != target.address.authority_id() {
                return Err(RemoteSocketTransportError::new(format!(
                    "authority envelope sender `{}` does not match target authority `{}`",
                    envelope.sender_id,
                    target.address.authority_id()
                )));
            }
            remote_runtime
                .send_mirror_bootstrap_chunk(
                    target,
                    payload.chunk_seq,
                    payload.stream,
                    payload.bytes_base64.clone(),
                )
                .map_err(|error| RemoteSocketTransportError::new(error.to_string()))
        }
        ControlPlanePayload::MirrorBootstrapComplete(payload) => {
            if envelope.sender_id != target.address.authority_id() {
                return Err(RemoteSocketTransportError::new(format!(
                    "authority envelope sender `{}` does not match target authority `{}`",
                    envelope.sender_id,
                    target.address.authority_id()
                )));
            }
            remote_runtime
                .send_mirror_bootstrap_complete(target, payload.last_chunk_seq)
                .map_err(|error| RemoteSocketTransportError::new(error.to_string()))
        }
        other => Err(RemoteSocketTransportError::new(format!(
            "unexpected authority envelope payload `{}`",
            other.message_type()
        ))),
    }
}

fn draw_remote_snapshot(
    terminal: &TerminalRuntime,
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    snapshot: &RemoteObserverSnapshot,
    authority_status: &AuthorityTransportStatus,
) -> Result<(), LifecycleError> {
    let viewport = terminal.current_size_or_default();
    let screen_lines = if snapshot.has_visible_output
        && matches!(authority_status, AuthorityTransportStatus::Connected)
    {
        snapshot.active_screen().styled_lines.clone()
    } else {
        placeholder_lines(target, binding, authority_status, viewport)
    };
    let active_screen = snapshot.active_screen();

    let mut stdout = io::stdout().lock();
    for row in 0..usize::from(viewport.rows.max(1)) {
        let line = screen_lines.get(row).map(String::as_str).unwrap_or("");
        write!(stdout, "\x1b[{};1H{}\x1b[K", row + 1, line).map_err(|error| {
            LifecycleError::Io("failed to draw remote main-slot output".to_string(), error)
        })?;
    }

    if snapshot.has_visible_output && active_screen.cursor_visible {
        write!(
            stdout,
            "\x1b[{};{}H\x1b[?25h",
            usize::from(active_screen.cursor_row.saturating_add(1)),
            usize::from(active_screen.cursor_col.saturating_add(1))
        )
        .map_err(|error| {
            LifecycleError::Io(
                "failed to position remote main-slot cursor".to_string(),
                error,
            )
        })?;
    } else {
        write!(stdout, "\x1b[?25l").map_err(|error| {
            LifecycleError::Io("failed to hide remote main-slot cursor".to_string(), error)
        })?;
    }
    stdout.flush().map_err(|error| {
        LifecycleError::Io("failed to flush remote main-slot output".to_string(), error)
    })
}

fn placeholder_lines(
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    authority_status: &AuthorityTransportStatus,
    viewport: TerminalSize,
) -> Vec<String> {
    let (status_label, detail_line) = match authority_status {
        AuthorityTransportStatus::WaitingForRemoteAuthority => (
            "waiting for remote authority",
            "waiting for a live authority node to register this target transport".to_string(),
        ),
        AuthorityTransportStatus::Connected => (
            "connected",
            "authority transport is live; waiting for remote target output".to_string(),
        ),
        AuthorityTransportStatus::Disconnected => (
            "disconnected",
            "authority transport disconnected; waiting for the remote authority to come back"
                .to_string(),
        ),
        AuthorityTransportStatus::Failed(message) => {
            ("failed", format!("authority transport error: {message}"))
        }
    };
    let mut lines = vec![
        format!(
            "remote target {}",
            target
                .command_name
                .as_deref()
                .unwrap_or(target.address.session_id())
        ),
        format!("target-id: {}", target.address.id().as_str()),
        format!(
            "attachment: {}",
            binding
                .map(|binding| binding.attachment_id.as_str())
                .unwrap_or("pending")
        ),
        format!("authority transport: {status_label}"),
    ];
    lines.push(detail_line);

    while lines.len() < usize::from(viewport.rows.max(1)) {
        lines.push(String::new());
    }
    lines
}

fn authority_status_from_runtime(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    target_is_present: bool,
    waiting_status: &AuthorityTransportStatus,
) -> AuthorityTransportStatus {
    if !target_is_present {
        return AuthorityTransportStatus::Disconnected;
    }
    if remote_runtime.has_connection(target.address.authority_id()) {
        AuthorityTransportStatus::Connected
    } else {
        waiting_status.clone()
    }
}

fn authority_transport_event_sender(
    tx: mpsc::Sender<RemotePaneEvent>,
) -> mpsc::Sender<AuthorityTransportEvent> {
    let (authority_tx, authority_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(event) = authority_rx.recv() {
            if tx.send(RemotePaneEvent::AuthorityTransport(event)).is_err() {
                break;
            }
        }
    });
    authority_tx
}

pub(crate) fn main_slot_surface_spec(command: &RemoteMainSlotCommand) -> RemoteInteractSurfaceSpec {
    RemoteInteractSurfaceSpec {
        socket_name: command.socket_name.clone(),
        surface_scope: command.session_name.clone(),
        target: command.target.clone(),
        console_id: main_slot_console_id(command),
        console_host_id: command.socket_name.clone(),
        console_location: ConsoleLocation::LocalWorkspace,
    }
}

fn main_slot_console_id(command: &RemoteMainSlotCommand) -> String {
    format!(
        "workspace-main-slot:{}:{}",
        command.socket_name, command.session_name
    )
}

fn write_escape(sequence: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteSocketTransportError {
    message: String,
}

impl RemoteSocketTransportError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteSocketTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteSocketTransportError {}

impl From<io::Error> for RemoteSocketTransportError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<RemoteTransportCodecError> for RemoteSocketTransportError {
    fn from(value: RemoteTransportCodecError) -> Self {
        Self::new(value.to_string())
    }
}

fn remote_protocol_error(error: impl ToString) -> LifecycleError {
    LifecycleError::Protocol(error.to_string())
}

fn remote_pane_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to run remote main-slot pane".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

extern "C" fn remote_pane_sigwinch_handler(_signal: c_int) {
    let fd = REMOTE_PANE_SIGWINCH_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }

    let byte = 1_u8;
    unsafe {
        let _ = write(fd, (&byte as *const u8).cast::<c_void>(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        activate_surface_target, apply_authority_envelope, authority_status_from_runtime,
        authority_transport_event_sender, encode_base64, main_slot_console_id,
        main_slot_surface_spec, placeholder_lines, should_exit_surface_locally,
        AuthorityTransportStatus, RemoteInteractInputSignalDecoder, RemoteInteractSignal,
        RemoteInteractSurfaceSpec, RemoteMainSlotPaneRuntime, RemotePaneEvent,
    };
    use crate::application::target_registry_service::{
        DefaultTargetCatalogGateway, TargetRegistryService,
    };
    use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::infra::remote_protocol::{
        ControlPlanePayload, MirrorBootstrapChunkPayload, MirrorBootstrapCompletePayload,
        ProtocolEnvelope, RemoteConsoleDescriptor, TargetOutputPayload,
    };
    use crate::infra::remote_transport_codec::write_registration_frame;
    use crate::runtime::remote_authority_connection_runtime::{
        spawn_authority_listener, AuthorityConnectionRequest, AuthorityTransportEvent,
    };
    use crate::runtime::remote_authority_transport_runtime::{
        authority_transport_socket_path, RemoteAuthorityCommand,
    };
    use crate::runtime::remote_main_slot_runtime::RemoteAttachmentBinding;
    use crate::runtime::remote_main_slot_runtime::RemoteMainSlotRuntime;
    use crate::runtime::remote_observer_runtime::RemoteObserverRuntime;
    use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
    use crate::terminal::TerminalSize;
    use std::fs;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::process;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn main_slot_console_id_matches_workspace_main_slot_shape() {
        let command = RemoteMainSlotCommand {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
        };

        assert_eq!(
            main_slot_console_id(&command),
            "workspace-main-slot:wa-1:workspace-1"
        );
    }

    #[test]
    fn main_slot_surface_spec_marks_local_workspace_console() {
        let command = RemoteMainSlotCommand {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
        };

        let spec = main_slot_surface_spec(&command);

        assert_eq!(spec.console_id, "workspace-main-slot:wa-1:workspace-1");
        assert_eq!(spec.console_host_id, "wa-1");
        assert_eq!(spec.surface_scope, "workspace-1");
        assert_eq!(spec.console_location, ConsoleLocation::LocalWorkspace);
    }

    #[test]
    fn only_server_console_surface_exits_on_ctrl_right_bracket() {
        let main_slot = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "wa-1".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };
        let server_console = RemoteInteractSurfaceSpec {
            console_location: ConsoleLocation::ServerConsole,
            ..main_slot.clone()
        };

        assert!(!should_exit_surface_locally(&main_slot, &[0x1d]));
        assert!(should_exit_surface_locally(&server_console, &[0x1d]));
        assert!(!should_exit_surface_locally(&server_console, b"hello"));
    }

    #[test]
    fn server_console_input_decoder_emits_input_started_and_submit() {
        let spec = server_console_surface_spec();
        let mut decoder = RemoteInteractInputSignalDecoder::default();

        assert_eq!(
            decoder.feed(&spec, b"abc\r"),
            vec![
                RemoteInteractSignal::ConsoleInputStarted,
                RemoteInteractSignal::ConsoleSubmit,
            ]
        );
    }

    #[test]
    fn server_console_input_decoder_keeps_partial_submit_sequence_until_complete() {
        let spec = server_console_surface_spec();
        let mut decoder = RemoteInteractInputSignalDecoder::default();

        assert!(decoder.feed(&spec, b"\x1b[13").is_empty());
        assert_eq!(
            decoder.feed(&spec, b"u"),
            vec![
                RemoteInteractSignal::ConsoleInputStarted,
                RemoteInteractSignal::ConsoleSubmit,
            ]
        );
    }

    #[test]
    fn server_console_input_decoder_emits_manual_return_for_ctrl_right_bracket() {
        let spec = server_console_surface_spec();
        let mut decoder = RemoteInteractInputSignalDecoder::default();

        assert_eq!(
            decoder.feed(&spec, &[0x1d]),
            vec![RemoteInteractSignal::ManualReturnToPicker]
        );
    }

    #[test]
    fn new_with_external_authority_streams_keeps_external_sink_under_runtime_ownership() {
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let runtime = RemoteMainSlotPaneRuntime::new_with_external_authority_streams(
            target_registry,
            PathBuf::from("/tmp/waitagent"),
        );

        let (_client, server) = UnixStream::pair().expect("stream pair should open");
        runtime
            .submit_external_authority_stream(server)
            .expect("runtime should accept submitted authority stream");
    }

    #[test]
    fn submitted_external_authority_stream_reaches_authority_connection_runtime() {
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let runtime = RemoteMainSlotPaneRuntime::new_with_external_authority_streams(
            target_registry,
            PathBuf::from("/tmp/waitagent"),
        );
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = runtime
            .authority_connections
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("pane-external-authority"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("authority connection runtime should start");

        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        runtime
            .submit_external_authority_stream(server)
            .expect("runtime should accept external authority stream");
        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));
    }

    #[test]
    fn runtime_without_external_authority_streams_rejects_submissions() {
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let runtime = RemoteMainSlotPaneRuntime::new(
            target_registry,
            Box::new(crate::runtime::remote_authority_connection_runtime::LocalAuthoritySocketBridgeStarter),
            PathBuf::from("/tmp/waitagent"),
            RemoteNetworkConfig::default(),
        );

        let (_client, server) = UnixStream::pair().expect("stream pair should open");
        let error = runtime
            .submit_external_authority_stream(server)
            .expect_err("default runtime should reject external authority stream submissions");

        assert_eq!(
            error.to_string(),
            "remote main-slot pane runtime is not configured for external authority streams"
        );
    }

    #[test]
    fn encode_base64_matches_standard_output_for_short_chunks() {
        assert_eq!(encode_base64(b"a"), "YQ==");
        assert_eq!(encode_base64(b"ab"), "YWI=");
        assert_eq!(encode_base64(b"abc"), "YWJj");
    }

    #[test]
    fn placeholder_lines_explain_transport_gap_before_output_arrives() {
        let lines = placeholder_lines(
            &remote_target(),
            Some(&RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            }),
            &AuthorityTransportStatus::WaitingForRemoteAuthority,
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("remote target bash"));
        assert!(lines[3].contains("waiting for remote authority"));
        assert!(lines[4].contains("live authority node"));
    }

    #[test]
    fn placeholder_lines_surface_authority_transport_failures() {
        let lines = placeholder_lines(
            &remote_target(),
            Some(&RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            }),
            &AuthorityTransportStatus::Failed("unexpected authority node".to_string()),
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert!(lines[3].contains("failed"));
        assert!(lines[4].contains("unexpected authority node"));
    }

    #[test]
    fn placeholder_lines_surface_authority_disconnect() {
        let lines = placeholder_lines(
            &remote_target(),
            Some(&RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            }),
            &AuthorityTransportStatus::Disconnected,
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert!(lines[3].contains("disconnected"));
        assert!(lines[4].contains("waiting for the remote authority"));
    }

    #[test]
    fn placeholder_lines_show_pending_attachment_before_remote_activation_begins() {
        let lines = placeholder_lines(
            &remote_target(),
            None,
            &AuthorityTransportStatus::WaitingForRemoteAuthority,
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert_eq!(lines[2], "attachment: pending");
    }

    #[test]
    fn activate_surface_target_requires_registered_authority_connection() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        let mut observer = RemoteObserverRuntime::new(mailbox, 80, 24);
        let target = remote_target();
        let spec = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: target.address.qualified_target(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "observer-a".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };

        let error = activate_surface_target(
            &runtime,
            &target,
            &spec,
            &TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 0,
                pixel_height: 0,
            },
            &mut observer,
        )
        .expect_err("activation should fail before authority connection exists");

        assert!(error
            .to_string()
            .contains("remote control-plane connection for node `peer-a` is not registered"));
    }

    #[test]
    fn activate_surface_target_succeeds_after_authority_connection_registration() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let mut observer = RemoteObserverRuntime::new(mailbox, 80, 24);
        let target = remote_target();
        let spec = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: target.address.qualified_target(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "observer-a".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };

        let binding = activate_surface_target(
            &runtime,
            &target,
            &spec,
            &TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 0,
                pixel_height: 0,
            },
            &mut observer,
        )
        .expect("activation should succeed after authority connection exists");

        assert_eq!(binding.attachment_id, "attach-1");
        assert_eq!(
            observer.snapshot().attachment_id.as_deref(),
            Some("attach-1")
        );
    }

    #[test]
    fn authority_status_from_runtime_prefers_disconnected_when_target_is_missing() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        runtime.ensure_local_connection("peer-a");

        assert_eq!(
            authority_status_from_runtime(
                &runtime,
                &remote_target(),
                false,
                &AuthorityTransportStatus::WaitingForRemoteAuthority,
            ),
            AuthorityTransportStatus::Disconnected
        );
    }

    #[test]
    fn authority_transport_socket_path_is_workspace_and_target_scoped() {
        let command = RemoteMainSlotCommand {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
        };

        let path = authority_transport_socket_path(
            &command.socket_name,
            &command.session_name,
            &command.target,
        );
        let rendered = path.to_string_lossy();

        assert!(rendered.contains("waitagent-remote-wa-1-workspace-1-peer-a_shell-1.sock"));
    }

    #[test]
    fn authority_target_output_envelope_flows_back_into_observer_terminal_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let target = remote_target();

        runtime
            .activate_target(
                &target,
                crate::infra::remote_protocol::RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: crate::domain::session_catalog::ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");

        apply_authority_envelope(&runtime, &target, &authority_target_output_envelope(1))
            .expect("authority target_output should apply");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert_eq!(
            snapshot.active_screen().lines[0],
            "a           ".to_string()
        );
    }

    #[test]
    fn authority_bootstrap_envelope_flows_back_into_observer_terminal_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let target = remote_target();

        runtime
            .activate_target(
                &target,
                crate::infra::remote_protocol::RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: crate::domain::session_catalog::ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");

        apply_authority_envelope(&runtime, &target, &authority_bootstrap_chunk_envelope(1))
            .expect("authority bootstrap chunk should apply");
        apply_authority_envelope(&runtime, &target, &authority_bootstrap_complete_envelope(1))
            .expect("authority bootstrap complete should apply");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, None);
        assert!(snapshot.has_visible_output);
        assert!(snapshot.bootstrap_complete);
        assert_eq!(
            snapshot.active_screen().lines[0],
            "a           ".to_string()
        );
    }

    #[test]
    fn authority_target_output_envelope_flows_back_into_server_console_observer_terminal_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("server-console:wa-1:console-a")
            .expect("server-console observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let target = remote_target();

        runtime
            .activate_target(
                &target,
                crate::infra::remote_protocol::RemoteConsoleDescriptor {
                    console_id: "server-console:wa-1:console-a".to_string(),
                    console_host_id: "server-console:wa-1:console-a".to_string(),
                    location: crate::domain::session_catalog::ConsoleLocation::ServerConsole,
                },
                12,
                4,
            )
            .expect("server-console remote activation should succeed");

        apply_authority_envelope(&runtime, &target, &authority_target_output_envelope(1))
            .expect("authority target_output should apply for server-console observer");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer
            .sync()
            .expect("server-console observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert_eq!(
            snapshot.console_id.as_deref(),
            Some("server-console:wa-1:console-a")
        );
        assert_eq!(
            snapshot.active_screen().lines[0],
            "a           ".to_string()
        );
    }

    #[test]
    fn authority_transport_runtime_round_trips_resize_input_and_output() {
        let registry = RemoteConnectionRegistry::new();
        let runtime = RemoteMainSlotRuntime::with_registry(registry.clone());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        let authority_mailbox = runtime
            .ensure_local_connection("peer-a")
            .expect("authority loopback registration should succeed");
        let target = remote_target();
        let binding = runtime
            .activate_target(
                &target,
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");
        let authority_open = authority_mailbox.snapshot();
        assert_eq!(authority_open.len(), 1);
        assert_eq!(authority_open[0].message_type, "open_mirror_request");
        let socket_path = authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1");
        let _ = fs::remove_file(&socket_path);
        let (pane_tx, pane_rx) = mpsc::channel();
        let authority_tx = authority_transport_event_sender(pane_tx);
        let _listener = spawn_authority_listener(
            AuthorityConnectionRequest {
                socket_path: socket_path.clone(),
                authority_id: "peer-a".to_string(),
            },
            registry.clone(),
            authority_tx,
        )
        .expect("authority listener should bind");

        let mut authority =
            UnixStream::connect(&socket_path).expect("authority transport should connect");
        write_registration_frame(&mut authority, "peer-a")
            .expect("registration frame should encode");
        assert_eq!(
            pane_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("transport event should arrive"),
            RemotePaneEvent::AuthorityTransport(AuthorityTransportEvent::Connected)
        );

        runtime
            .send_pty_resize(&target, &binding, 160, 50)
            .expect("resize should route");
        assert_eq!(
            match crate::infra::remote_transport_codec::read_control_plane_envelope(&mut authority,)
                .expect("resize command should arrive")
                .payload
            {
                ControlPlanePayload::ApplyResize(payload) => {
                    RemoteAuthorityCommand::ApplyResize(payload)
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            RemoteAuthorityCommand::ApplyResize(
                crate::infra::remote_protocol::ApplyResizePayload {
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    resize_epoch: 1,
                    resize_authority_console_id: "console-a".to_string(),
                    cols: 160,
                    rows: 50,
                }
            )
        );

        runtime
            .send_console_input(&target, &binding, 1, "YQ==")
            .expect("input should route");
        assert_eq!(
            match crate::infra::remote_transport_codec::read_control_plane_envelope(&mut authority,)
                .expect("input command should arrive")
                .payload
            {
                ControlPlanePayload::TargetInput(payload) => {
                    RemoteAuthorityCommand::TargetInput(payload)
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            RemoteAuthorityCommand::TargetInput(
                crate::infra::remote_protocol::TargetInputPayload {
                    attachment_id: "attach-1".to_string(),
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    input_seq: 1,
                    bytes_base64: "YQ==".to_string(),
                }
            )
        );

        crate::infra::remote_transport_codec::write_control_plane_envelope(
            &mut authority,
            &ProtocolEnvelope {
                protocol_version: "1.1".to_string(),
                message_id: "msg-1".to_string(),
                message_type: "target_output",
                timestamp: "2026-04-28T00:00:00Z".to_string(),
                sender_id: "peer-a".to_string(),
                correlation_id: None,
                session_id: Some("shell-1".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    output_seq: 1,
                    stream: "pty",
                    bytes_base64: "Yg==".to_string(),
                }),
            },
        )
        .expect("target output should send");
        match pane_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive")
        {
            RemotePaneEvent::AuthorityTransport(AuthorityTransportEvent::Envelope(envelope)) => {
                apply_authority_envelope(&runtime, &target, &envelope)
                    .expect("authority output should apply");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert_eq!(
            snapshot.active_screen().lines[0],
            "b           ".to_string()
        );
        let _ = fs::remove_file(&socket_path);
    }

    fn authority_target_output_envelope(output_seq: u64) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("msg-{output_seq}"),
            message_type: "target_output",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq,
                stream: "pty",
                bytes_base64: "YQ==".to_string(),
            }),
        }
    }

    fn authority_bootstrap_chunk_envelope(chunk_seq: u64) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("bootstrap-{chunk_seq}"),
            message_type: "mirror_bootstrap_chunk",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                chunk_seq,
                stream: "pty",
                bytes_base64: "YQ==".to_string(),
            }),
        }
    }

    fn authority_bootstrap_complete_envelope(
        last_chunk_seq: u64,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("bootstrap-complete-{last_chunk_seq}"),
            message_type: "mirror_bootstrap_complete",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::MirrorBootstrapComplete(MirrorBootstrapCompletePayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                last_chunk_seq,
            }),
        }
    }

    fn remote_target() -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("peer-a", "shell-1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }

    fn server_console_surface_spec() -> RemoteInteractSurfaceSpec {
        RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "server-console:console-a".to_string(),
            target: "peer-a:shell-1".to_string(),
            console_id: "server-console:wa-1:console-a".to_string(),
            console_host_id: "server-console:wa-1:console-a".to_string(),
            console_location: ConsoleLocation::ServerConsole,
        }
    }

    fn test_socket_path(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-remote-main-slot-pane-{name}-{}-{millis}.sock",
            process::id()
        ))
    }
}
