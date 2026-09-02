// Legacy tmux-era session-sync backends kept during the ratatui migration; most items are currently unused.

use crate::cli::RemoteNetworkConfig;
use crate::domain::agent_detector::{accepts_at_reference, DetectorRegistry};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, SessionTransport,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_grpc_transport::RemoteNodeSessionHandle;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, RawPtyOutputPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_authority_transport_frame, write_authority_transport_frame, write_control_plane_envelope,
    AuthorityTransportFrame,
};
use crate::lifecycle::LifecycleError;
use crate::platform::remote_ipc::RemoteControlStream;
use crate::process_monitor::read_foreground_process_cmdline;
use crate::ratatui_node::authority_host_session::RatatuiAuthorityHostSession;
use crate::ratatui_node::clipboard_platform::format_file_reference;
use crate::ratatui_node::SharedState;
use crate::remote::authority::remote_authority_transport_runtime::{
    RemoteAuthorityCommand, AUTHORITY_TRANSPORT_PING_INTERVAL, AUTHORITY_TRANSPORT_READ_TIMEOUT,
    AUTHORITY_TRANSPORT_SOCKET_TIMEOUT,
};
use crate::remote::node::remote_node_session_sync_runtime::{
    remote_session_sync_error, SessionSyncAuthorityHost, SessionSyncAuthorityOutputRoute,
    LIVE_AUTHORITY_SERVER_ID, SESSION_SYNC_AUTHORITY_ID,
};

use std::collections::HashMap;
use std::io::{self, Write};
use std::net::Shutdown;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{LocalSessionCatalog, LocalTargetExitObserver};

/// Result of creating a local target in response to a remote `CreateSessionRequest`.
#[derive(Debug, Clone)]
pub struct CreatedLocalTarget {
    pub session_id: String,
    pub target_id: String,
}

/// Factory for creating a new local target session when this node is asked to
/// accept a remote `CreateSessionRequest`.
pub trait LocalTargetFactory: Clone + Send + 'static {
    type Error: ToString;

    fn create_local_target(
        &self,
        node_id: &str,
        cwd: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<CreatedLocalTarget, Self::Error>;
}

/// Signal returned by an authority host backend describing whether a host is
/// ready to accept commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityHostSignal {
    Ready,
    Starting,
    Closed,
}

/// Backend that manages an in-process authority target host for a local target
/// and delivers authority commands to it.
pub trait LocalAuthorityHostBackend: Clone + Send + 'static {
    type Error: ToString;

    /// Start a new authority host for `target_id`. The returned handle is owned
    /// by the caller and will be passed to `deliver_command` and
    /// `shutdown_authority_host`.
    fn spawn_authority_host(
        &self,
        session_handle: &RemoteNodeSessionHandle,
        target_id: &str,
        output_route: SessionSyncAuthorityOutputRoute,
    ) -> Result<SessionSyncAuthorityHost, Self::Error>;

    /// Return the current readiness signal for a host.
    fn authority_host_signal(&self, host: &SessionSyncAuthorityHost) -> AuthorityHostSignal;

    /// Deliver an authority command to a ready host.
    fn deliver_command(
        &self,
        host: &SessionSyncAuthorityHost,
        command: RemoteAuthorityCommand,
    ) -> Result<AuthorityHostSignal, Self::Error>;

    /// Stop a running authority host.
    fn shutdown_authority_host(&self, host: &SessionSyncAuthorityHost);
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
const AUTHORITY_HOST_READY_TIMEOUT: Duration = Duration::from_secs(5);

fn authority_command_target_id(command: &RemoteAuthorityCommand) -> &str {
    match command {
        RemoteAuthorityCommand::OpenMirror(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::CloseMirror(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::RawPtyInput(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::PasteFile(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::ApplyResize(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::SyncRequest { .. } | RemoteAuthorityCommand::HeartbeatPing => "",
    }
}

fn authority_command_envelope(
    command: RemoteAuthorityCommand,
) -> crate::infra::remote_protocol::ProtocolEnvelope<ControlPlanePayload> {
    use crate::infra::remote_protocol::{ErrorPayload, ProtocolEnvelope};
    let session_id = match &command {
        RemoteAuthorityCommand::OpenMirror(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::CloseMirror(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::RawPtyInput(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::PasteFile(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::ApplyResize(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::SyncRequest { .. } | RemoteAuthorityCommand::HeartbeatPing => None,
    };
    let payload = match command {
        RemoteAuthorityCommand::OpenMirror(payload) => {
            ControlPlanePayload::OpenMirrorRequest(payload)
        }
        RemoteAuthorityCommand::CloseMirror(payload) => {
            ControlPlanePayload::CloseMirrorRequest(payload)
        }
        RemoteAuthorityCommand::RawPtyInput(payload) => ControlPlanePayload::RawPtyInput(payload),
        RemoteAuthorityCommand::PasteFile(payload) => {
            ControlPlanePayload::PasteFileRequest(payload)
        }
        RemoteAuthorityCommand::ApplyResize(payload) => ControlPlanePayload::ApplyResize(payload),
        RemoteAuthorityCommand::SyncRequest { .. } | RemoteAuthorityCommand::HeartbeatPing => {
            ControlPlanePayload::Error(ErrorPayload {
                code: "local_sync_request_not_routable",
                message: "sync request is local to authority transport".to_string(),
                details: None,
            })
        }
    };
    ProtocolEnvelope {
        protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: format!("session-sync-authority-{}", timestamp_millis_now()),
        message_type: payload.message_type(),
        timestamp: format!("{}Z", timestamp_millis_now()),
        sender_id: SESSION_SYNC_AUTHORITY_ID.to_string(),
        correlation_id: None,
        session_id,
        target_id: None,
        attachment_id: None,
        console_id: None,
        payload,
    }
}

fn timestamp_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn target_session_name_from_target_id(target_id: &str) -> Option<String> {
    let target_id = target_id
        .strip_prefix("remote-peer:")
        .or_else(|| target_id.strip_prefix("local-tmux:"))
        .or_else(|| target_id.strip_prefix("remote:"))
        .unwrap_or(target_id);
    let (_, session_name) = target_id.rsplit_once(':')?;
    if session_name.is_empty() {
        None
    } else {
        Some(session_name.to_string())
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn live_authority_session_socket_path(socket_name: &str, session_name: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "waitagent-remote-session-sync-owner-{}.sock",
            sanitize_path_component(socket_name)
        ))
        .with_extension(format!(
            "authority-{}",
            sanitize_path_component(session_name)
        ))
}

pub(crate) fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

/// Reassembly buffer for a chunked `PasteFileRequest`.
struct PasteFileAssembler {
    filename_hint: String,
    total_chunks: u32,
    chunks: HashMap<u32, Vec<u8>>,
    last_received: Instant,
}

/// How long an incomplete `PasteFileAssembler` may stay in memory before it is
/// discarded. This bounds the memory growth when chunks are lost or a transfer
/// is abandoned mid-way.
const PASTE_FILE_ASSEMBLER_TIMEOUT: Duration = Duration::from_secs(300);

impl PasteFileAssembler {
    fn new(filename_hint: String, total_chunks: u32) -> Self {
        Self {
            filename_hint,
            total_chunks,
            chunks: HashMap::new(),
            last_received: Instant::now(),
        }
    }

    fn is_complete(&self) -> bool {
        self.chunks.len() as u32 == self.total_chunks
    }

    fn is_stale(&self) -> bool {
        self.last_received.elapsed() >= PASTE_FILE_ASSEMBLER_TIMEOUT
    }

    fn touch(&mut self) {
        self.last_received = Instant::now();
    }

    fn assemble(mut self) -> Vec<u8> {
        let total_len: usize = self.chunks.values().map(|c| c.len()).sum();
        let mut full = Vec::with_capacity(total_len);
        for index in 0..self.total_chunks {
            if let Some(chunk) = self.chunks.remove(&index) {
                full.extend_from_slice(&chunk);
            }
        }
        full
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[cfg(unix)]
#[allow(dead_code)]
fn spawn_live_authority_listener(
    socket_path: PathBuf,
    node_id: String,
    bridge_session_id: Arc<RwLock<String>>,
    output_route: SessionSyncAuthorityOutputRoute,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    writer_ready: Arc<Condvar>,
) {
    thread::spawn(move || {
        let Ok(listener) = bind_live_authority_listener(&socket_path) else {
            running.store(false, Ordering::Relaxed);
            return;
        };
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = bridge_live_authority_stream(
                        stream,
                        node_id.clone(),
                        bridge_session_id.clone(),
                        output_route.clone(),
                        running.clone(),
                        writer.clone(),
                        writer_ready.clone(),
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        crate::infra::best_effort::remove_file(&socket_path);
    });
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[cfg(unix)]
#[allow(dead_code)]
fn bind_live_authority_listener(socket_path: &Path) -> Result<UnixListener, io::Error> {
    if socket_path.exists() {
        crate::infra::best_effort::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[cfg(unix)]
#[allow(dead_code)]
fn bridge_live_authority_stream(
    mut host_stream: UnixStream,
    node_id: String,
    bridge_session_id: Arc<RwLock<String>>,
    output_route: SessionSyncAuthorityOutputRoute,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    writer_ready: Arc<Condvar>,
) -> Result<(), LifecycleError> {
    use crate::remote::node::remote_node_transport_runtime::{
        read_client_hello, write_server_hello,
    };
    let _node_id = read_client_hello(&mut host_stream).map_err(remote_session_sync_error)?;
    write_server_hello(&mut host_stream, LIVE_AUTHORITY_SERVER_ID)
        .map_err(remote_session_sync_error)?;
    let host_reader = host_stream.try_clone().map_err(remote_session_sync_error)?;
    {
        let mut writer_guard = match writer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                ERROR_LOG.log(
                    "[session-sync] authority writer mutex poisoned in bridge, recovering"
                        .to_string(),
                );
                poisoned.into_inner()
            }
        };
        if let Some(previous) = writer_guard.take() {
            let _ = previous.shutdown(Shutdown::Both);
        }
        *writer_guard = Some(host_stream.try_clone().map_err(remote_session_sync_error)?);
    }
    writer_ready.notify_all();

    let result = forward_host_output(
        host_reader,
        writer.clone(),
        node_id,
        bridge_session_id,
        output_route,
        running.clone(),
    );

    let _ = host_stream.shutdown(Shutdown::Both);
    let _ = match writer.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => {
            ERROR_LOG.log(
                "[session-sync] authority writer mutex poisoned in bridge cleanup, recovering"
                    .to_string(),
            );
            poisoned.into_inner().take()
        }
    };
    result
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[cfg(unix)]
#[allow(dead_code)]
fn send_authority_ping(writer: &Arc<Mutex<Option<UnixStream>>>) -> Result<(), LifecycleError> {
    let mut guard = match writer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            ERROR_LOG.log(
                "[session-sync] authority writer mutex poisoned while sending ping, recovering"
                    .to_string(),
            );
            poisoned.into_inner()
        }
    };
    let Some(stream) = guard.as_mut() else {
        return Err(remote_session_sync_error(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "authority writer closed before ping could be sent",
        )));
    };
    write_authority_transport_frame(stream, &AuthorityTransportFrame::Ping)
        .map_err(remote_session_sync_error)?;
    stream.flush().map_err(remote_session_sync_error)?;
    Ok(())
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[cfg(unix)]
#[allow(dead_code)]
fn forward_host_output(
    mut host_reader: UnixStream,
    writer: Arc<Mutex<Option<UnixStream>>>,
    node_id: String,
    bridge_session_id: Arc<RwLock<String>>,
    output_route: SessionSyncAuthorityOutputRoute,
    running: Arc<AtomicBool>,
) -> Result<(), LifecycleError> {
    host_reader
        .set_read_timeout(Some(AUTHORITY_TRANSPORT_SOCKET_TIMEOUT))
        .map_err(remote_session_sync_error)?;
    let ping_timeout = AUTHORITY_TRANSPORT_READ_TIMEOUT;
    let ping_interval = AUTHORITY_TRANSPORT_PING_INTERVAL;
    let mut last_received = Instant::now();
    let mut keepalive_sent_at: Option<Instant> = None;

    while running.load(Ordering::Relaxed) {
        match read_authority_transport_frame(&mut host_reader) {
            Ok(AuthorityTransportFrame::Pong) => {
                last_received = Instant::now();
                keepalive_sent_at = None;
            }
            Ok(AuthorityTransportFrame::Ping) => {
                last_received = Instant::now();
                keepalive_sent_at = None;
            }
            Ok(AuthorityTransportFrame::ControlPlane(envelope)) => {
                last_received = Instant::now();
                keepalive_sent_at = None;
                let session_instance_id = bridge_session_id
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                forward_host_envelope(&output_route, &node_id, &session_instance_id, *envelope)?;
            }
            Ok(AuthorityTransportFrame::RawPtyOutput(payload)) => {
                last_received = Instant::now();
                keepalive_sent_at = None;
                let session_instance_id = bridge_session_id
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let envelope = crate::infra::remote_protocol::ProtocolEnvelope {
                    protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION
                        .to_string(),
                    message_id: format!("{}-raw-pty-output-{}", node_id, payload.output_seq),
                    message_type: "raw_pty_output",
                    timestamp: String::new(),
                    sender_id: node_id.clone(),
                    correlation_id: None,
                    session_id: Some(payload.session_id.clone()),
                    target_id: Some(payload.target_id.clone()),
                    attachment_id: None,
                    console_id: None,
                    payload: ControlPlanePayload::RawPtyOutput(payload),
                };
                forward_host_envelope(&output_route, &node_id, &session_instance_id, envelope)?;
            }
            Ok(other) => {
                return Err(remote_session_sync_error(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unexpected authority frame {other:?}"),
                )));
            }
            Err(ref e) if e.is_read_timeout() => {
                if let Some(sent_at) = keepalive_sent_at {
                    if sent_at.elapsed() >= ping_timeout {
                        let _session_instance_id = bridge_session_id
                            .read()
                            .map(|g| g.clone())
                            .unwrap_or_default();

                        return Err(remote_session_sync_error(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "authority host keepalive timed out",
                        )));
                    }
                    continue;
                }
                if last_received.elapsed() >= ping_interval {
                    send_authority_ping(&writer)?;
                    keepalive_sent_at = Some(Instant::now());
                }
            }
            Err(e) => {
                return Err(remote_session_sync_error(e));
            }
        }
    }
    Ok(())
}

fn forward_host_envelope(
    output_route: &SessionSyncAuthorityOutputRoute,
    node_id: &str,
    session_instance_id: &str,
    envelope: crate::infra::remote_protocol::ProtocolEnvelope<ControlPlanePayload>,
) -> Result<(), LifecycleError> {
    match output_route {
        SessionSyncAuthorityOutputRoute::OwnerEvent(session_event_tx) => {

            if let Err(e) = session_event_tx.send(
                crate::remote::node::remote_node_session_sync_runtime::SessionSyncEvent::AuthorityHostOutput(
                    Box::new(envelope),
                ),
            ) {

                return Err(remote_session_sync_error(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    e.to_string(),
                )));
            }
        }
        SessionSyncAuthorityOutputRoute::IngressEvent(ingress_event_tx) => {

            if let Err(e) = ingress_event_tx.send(
                crate::remote::node::remote_node_ingress_server_runtime::InternalEvent::AuthorityHostOutput {
                    node_id: node_id.to_string(),
                    session_instance_id: session_instance_id.to_string(),
                    envelope,
                },
            ) {

                return Err(remote_session_sync_error(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    e.to_string(),
                )));
            }
        }
    }
    Ok(())
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
pub(super) fn wait_for_live_authority_socket(socket_path: &Path) -> Result<(), LifecycleError> {
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

// ---------------------------------------------------------------------------
// Ratatui implementations
// ---------------------------------------------------------------------------

/// Ratatui-backed local session catalog.
///
/// Reads local sessions directly from the single-process server's `SharedState`
/// catalog instead of querying a remote workspace server.
#[derive(Clone)]
pub struct RatatuiLocalSessionCatalog {
    shared: Arc<SharedState>,
}

impl RatatuiLocalSessionCatalog {
    pub fn new(shared: Arc<SharedState>) -> Self {
        Self { shared }
    }
}

impl LocalSessionCatalog for RatatuiLocalSessionCatalog {
    type Error = &'static str;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        let guard = self
            .shared
            .sessions
            .sessions
            .lock()
            .map_err(|_| "sessions mutex poisoned")?;
        Ok(guard
            .values()
            .filter(|session| *session.address.transport() == SessionTransport::Local)
            .cloned()
            .collect())
    }

    fn local_target_socket_name(&self) -> Option<&str> {
        None
    }
}

/// Ratatui-backed factory that creates a new local bash session in `SharedState`.
#[derive(Clone)]
pub struct RatatuiLocalTargetFactory {
    shared: Arc<SharedState>,
}

impl RatatuiLocalTargetFactory {
    pub fn new(shared: Arc<SharedState>, _network: RemoteNetworkConfig) -> Self {
        Self { shared }
    }
}

impl LocalTargetFactory for RatatuiLocalTargetFactory {
    type Error = LifecycleError;

    fn create_local_target(
        &self,
        node_id: &str,
        _cwd: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<CreatedLocalTarget, Self::Error> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.shared
            .state_sender()
            .send(
                crate::ratatui_node::state_event::StateEvent::CreateAuthorityHostSession {
                    request_id: node_id.to_string(),
                    cols,
                    rows,
                    reply_tx,
                },
            )
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to send create-authority-host-session event".to_string(),
                    io::Error::other(error.to_string()),
                )
            })?;
        let created = reply_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| {
                LifecycleError::Io(
                    "state event loop did not reply to create-authority-host-session".to_string(),
                    io::Error::other(error.to_string()),
                )
            })?
            .map_err(|error| {
                ERROR_LOG.log(format!(
                    "[ratatui-local-target-factory] failed to spawn authority host session: {error}"
                ));
                error
            })?;
        let target_id = format!("remote-peer:{node_id}:{}", created.session_id);
        Ok(CreatedLocalTarget {
            session_id: created.session_id,
            target_id,
        })
    }
}

/// Ratatui-backed local target exit observer.
///
/// In the single-process model, local target exit is handled directly by the
/// server loop, so there is no separate sidecar to spawn.
#[derive(Clone)]
pub struct RatatuiLocalTargetExitObserver;

impl LocalTargetExitObserver for RatatuiLocalTargetExitObserver {
    fn observe_local_target_exit(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }
}

/// Ratatui-backed authority host backend.
///
/// In the single-process model the authority listener and target-host sidecar
/// are replaced by in-process threads. Commands from the remote viewer are
/// delivered to the local PTY session and raw PTY output is forwarded back to
/// the gRPC session via `SessionSyncAuthorityOutputRoute`.
#[derive(Clone)]
pub struct RatatuiLocalAuthorityHostBackend {
    shared: Arc<SharedState>,
}

impl RatatuiLocalAuthorityHostBackend {
    pub fn new(shared: Arc<SharedState>, _network: RemoteNetworkConfig) -> Self {
        Self { shared }
    }
}

impl LocalAuthorityHostBackend for RatatuiLocalAuthorityHostBackend {
    type Error = LifecycleError;

    fn spawn_authority_host(
        &self,
        session_handle: &RemoteNodeSessionHandle,
        target_id: &str,
        output_route: SessionSyncAuthorityOutputRoute,
    ) -> Result<SessionSyncAuthorityHost, Self::Error> {
        let bound_session_instance_id = session_handle.session_instance_id().to_string();
        let node_id = session_handle.node_id().to_string();
        let session_name = target_session_name_from_target_id(target_id).ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "failed to derive local session from target id `{target_id}`"
            ))
        })?;
        let session = {
            let guard = self
                .shared
                .sessions
                .authority_host_sessions
                .lock()
                .map_err(|error| {
                    LifecycleError::Io(
                        "ratatui authority host sessions mutex poisoned".to_string(),
                        io::Error::other(error.to_string()),
                    )
                })?;
            guard.get(&session_name).cloned().ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "ratatui authority host session `{session_name}` does not exist"
                ))
            })?
        };

        let (host_end, listener_end) =
            crate::platform::remote_ipc::socket_pair().map_err(|error| {
                LifecycleError::Io(
                    "failed to create authority host socket pair".to_string(),
                    error,
                )
            })?;
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
        self.shared
            .authority_host_io_sender()
            .send(crate::ratatui_node::authority_host_io_loop::AuthorityHostIoRequest::SetOutputSender {
                session_id: session_name.clone(),
                output_tx,
            })
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to set authority host output sender".to_string(),
                    io::Error::other(error.to_string()),
                )
            })?;
        let io_tx = self.shared.authority_host_io_sender();

        let writer = Arc::new(Mutex::new(None));
        let writer_ready = Arc::new(Condvar::new());
        let running = Arc::new(AtomicBool::new(true));
        let bridge_session_id = Arc::new(RwLock::new(bound_session_instance_id.clone()));

        spawn_ratatui_authority_listener(SpawnRatatuiAuthorityListenerArgs {
            host_stream: host_end.try_clone().map_err(|error| {
                LifecycleError::Io("failed to clone host stream".to_string(), error)
            })?,
            listener_stream: listener_end.try_clone().map_err(|error| {
                LifecycleError::Io("failed to clone listener stream".to_string(), error)
            })?,
            node_id: node_id.clone(),
            bridge_session_id: bridge_session_id.clone(),
            output_route,
            running: running.clone(),
            writer: writer.clone(),
            writer_ready: writer_ready.clone(),
        });

        spawn_ratatui_authority_target_host(SpawnRatatuiAuthorityTargetHostArgs {
            listener_stream: listener_end,
            host_stream: host_end,
            session,
            target_id: target_id.to_string(),
            node_id,
            running: running.clone(),
            io_tx,
            output_rx,
            shared: self.shared.clone(),
        });

        Ok(SessionSyncAuthorityHost {
            writer,
            running,
            writer_ready,
            bound_session_instance_id,
            bridge_session_id,
        })
    }

    fn authority_host_signal(&self, host: &SessionSyncAuthorityHost) -> AuthorityHostSignal {
        match host.writer.lock() {
            Ok(guard) => {
                if guard.is_some() {
                    AuthorityHostSignal::Ready
                } else if host.running.load(Ordering::Relaxed) {
                    AuthorityHostSignal::Starting
                } else {
                    AuthorityHostSignal::Closed
                }
            }
            Err(poisoned) => {
                ERROR_LOG
                    .log("[session-sync] authority writer mutex poisoned, recovering".to_string());
                let guard = poisoned.into_inner();
                if guard.is_some() {
                    AuthorityHostSignal::Ready
                } else if host.running.load(Ordering::Relaxed) {
                    AuthorityHostSignal::Starting
                } else {
                    AuthorityHostSignal::Closed
                }
            }
        }
    }

    fn deliver_command(
        &self,
        host: &SessionSyncAuthorityHost,
        command: RemoteAuthorityCommand,
    ) -> Result<AuthorityHostSignal, Self::Error> {
        const AUTHORITY_HOST_READY_TIMEOUT: Duration = Duration::from_secs(5);
        let target_id = authority_command_target_id(&command).to_string();
        ERROR_LOG.log(format!(
            "[ratatui-session-sync] deliver_command target={target_id} command={command:?} writer_ready={}",
            host.writer.lock().is_ok_and(|g| g.is_some())
        ));
        let mut guard = match host.writer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                ERROR_LOG
                    .log("[session-sync] authority writer mutex poisoned, recovering".to_string());
                poisoned.into_inner()
            }
        };

        while guard.is_none() && host.running.load(Ordering::Relaxed) {
            let wait_result = host
                .writer_ready
                .wait_timeout(guard, AUTHORITY_HOST_READY_TIMEOUT)
                .map_err(|_| {
                    LifecycleError::Protocol(
                        "authority writer mutex poisoned while waiting for ready signal"
                            .to_string(),
                    )
                })?;
            guard = wait_result.0;
            if wait_result.1.timed_out() {
                return Ok(AuthorityHostSignal::Starting);
            }
        }

        let Some(writer) = guard.as_mut() else {
            return Ok(AuthorityHostSignal::Closed);
        };
        let envelope = authority_command_envelope(command);
        if let Err(_error) = write_control_plane_envelope(writer, &envelope) {
            let _ = writer.shutdown(Shutdown::Both);
            *guard = None;

            return Ok(AuthorityHostSignal::Closed);
        }
        Ok(AuthorityHostSignal::Ready)
    }

    fn shutdown_authority_host(&self, host: &SessionSyncAuthorityHost) {
        ERROR_LOG.log(format!(
            "[ratatui-session-sync] shutting down authority host for session={}",
            host.bound_session_instance_id
        ));
        host.running.store(false, Ordering::Relaxed);
    }
}

struct SpawnRatatuiAuthorityListenerArgs {
    host_stream: RemoteControlStream,
    #[allow(dead_code)]
    listener_stream: RemoteControlStream,
    node_id: String,
    bridge_session_id: Arc<RwLock<String>>,
    output_route: SessionSyncAuthorityOutputRoute,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<RemoteControlStream>>>,
    writer_ready: Arc<Condvar>,
}

fn spawn_ratatui_authority_listener(args: SpawnRatatuiAuthorityListenerArgs) {
    let SpawnRatatuiAuthorityListenerArgs {
        host_stream,
        listener_stream: _,
        node_id,
        bridge_session_id,
        output_route,
        running,
        writer,
        writer_ready,
    } = args;
    thread::spawn(move || {
        // The ratatui authority host is backed by an internal socket pair
        // (see `crate::platform::remote_ipc::socket_pair`). The listener owns
        // one end of the pair and uses it for both writing viewer commands
        // and reading PTY output from the target host.
        {
            let mut writer_guard = match writer.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    ERROR_LOG.log(
                        "[ratatui-session-sync] authority writer mutex poisoned in listener, recovering"
                            .to_string(),
                    );
                    poisoned.into_inner()
                }
            };
            if let Some(previous) = writer_guard.take() {
                let _ = previous.shutdown(Shutdown::Both);
            }
            match host_stream.try_clone().map_err(remote_session_sync_error) {
                Ok(stream) => *writer_guard = Some(stream),
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-session-sync] authority listener failed to clone host stream: {error}"
                    ));
                    running.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
        writer_ready.notify_all();

        let _ = host_stream.set_read_timeout(Some(AUTHORITY_TRANSPORT_SOCKET_TIMEOUT));

        let result = forward_ratatui_host_output(
            host_stream,
            node_id,
            bridge_session_id,
            output_route,
            running.clone(),
        );
        ERROR_LOG.log(format!(
            "[ratatui-session-sync] authority listener output forwarder exited: {result:?}"
        ));
        running.store(false, Ordering::Relaxed);
        let _ = match writer.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
    });
}

struct SpawnRatatuiAuthorityTargetHostArgs {
    listener_stream: RemoteControlStream,
    #[allow(dead_code)]
    host_stream: RemoteControlStream,
    session: Arc<crate::ratatui_node::authority_host_session::RatatuiAuthorityHostSession>,
    target_id: String,
    #[allow(dead_code)]
    node_id: String,
    running: Arc<AtomicBool>,
    io_tx: crate::ratatui_node::authority_host_io_loop::AuthorityHostIoHandle,
    output_rx: mpsc::Receiver<Vec<u8>>,
    shared: Arc<SharedState>,
}

/// Return whether the authority-host session should receive `@`-prefixed file
/// references.
///
/// The catalog record that process detection updates is keyed under the local
/// authority id (`local:{authority_id}:{session_id}`), while the viewer-facing
/// command uses `remote-peer:{node_id}:{session_id}`. This helper first looks up
/// the local record so agent detection is honored. If the record has not been
/// updated yet (authority-host sessions do not have a background /proc monitor
/// like local sessions do), it falls back to scanning `/proc` for the current
/// PTY foreground process.
fn authority_host_supports_at(shared: &SharedState, session: &RatatuiAuthorityHostSession) -> bool {
    if let Some(supports_at) = authority_host_supports_at_from_record(shared, &session.session_id) {
        return supports_at;
    }

    // Authority-host sessions lack a background /proc monitor, so agent hooks
    // may be the only source of agent_command_name. If hooks have not fired yet
    // (e.g. the user pastes before submitting a prompt), detect the agent from
    // the PTY foreground process at paste time.
    #[cfg(unix)]
    {
        let (argv0, argv) = read_foreground_process_cmdline(session.pty_master.as_raw_fd());
        let command_name = argv0
            .as_deref()
            .map(|cmd| DetectorRegistry::default().detect_command_name(cmd, argv.as_deref(), ""));
        command_name
            .as_deref()
            .map(accepts_at_reference)
            .unwrap_or(false)
    }
    // On Windows (no ConPTY support yet) there is no foreground process group to
    // probe; rely solely on the catalog record.
    #[cfg(not(unix))]
    {
        let _ = session;
        false
    }
}

/// Look up `agent_command_name` from the local catalog record. Returns `None`
/// when there is no record or the record has no agent command name yet.
fn authority_host_supports_at_from_record(shared: &SharedState, session_id: &str) -> Option<bool> {
    let local_target_id =
        ManagedSessionAddress::local(shared.local_authority_id(), session_id).qualified_target();
    let guard = shared
        .sessions
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard
        .get(&local_target_id)
        .and_then(|r| r.agent_command_name.as_deref())
        .map(accepts_at_reference)
}

/// Time to wait after receiving `OpenMirrorRequest` before sending a bootstrap
/// snapshot. Short-lived reconnects that drop within this window will skip the
/// bootstrap, avoiding a full screen redraw for a connection that does not last.
const BOOTSTRAP_STABLE_WINDOW: Duration = Duration::from_millis(300);

/// Bootstrap scheduling state for a target-host reader thread.
enum BootstrapState {
    /// No bootstrap is scheduled.
    None,
    /// A bootstrap is scheduled once the deadline is reached.
    Pending { deadline: Instant },
}

/// If a bootstrap is pending and its deadline has passed, ask the IO loop to
/// send it and clear the pending state. Returns whether bootstrap was sent.
fn maybe_send_bootstrap(
    state: &mut BootstrapState,
    session: &crate::ratatui_node::authority_host_session::RatatuiAuthorityHostSession,
    io_tx: &crate::ratatui_node::authority_host_io_loop::AuthorityHostIoHandle,
) -> bool {
    if let BootstrapState::Pending { deadline } = *state {
        if Instant::now() >= deadline {
            session.send_bootstrap(io_tx);
            *state = BootstrapState::None;
            return true;
        }
    }
    false
}

fn spawn_ratatui_authority_target_host(args: SpawnRatatuiAuthorityTargetHostArgs) {
    let SpawnRatatuiAuthorityTargetHostArgs {
        mut listener_stream,
        host_stream: _,
        session,
        target_id,
        node_id: _,
        running,
        io_tx,
        output_rx,
        shared,
    } = args;
    thread::spawn(move || {
        let session_id = session.session_id.clone();
        // The target host owns the other end of the socket pair. It reads
        // viewer commands from this end and forwards them to AuthorityHostIoLoop.
        // PTY output arrives on output_rx and is framed as RawPtyOutput back to
        // the listener, which forwards it over the gRPC bridge.

        let mirror_active = Arc::new(AtomicBool::new(false));
        let mut output_stream = match listener_stream.try_clone() {
            Ok(stream) => stream,
            Err(error) => {
                ERROR_LOG.log(format!(
                    "failed to clone listener stream for output pump: {error}"
                ));
                return;
            }
        };
        let output_active = mirror_active.clone();
        let output_running = running.clone();
        let output_target_id = target_id.clone();
        let output_session_id = session_id.clone();
        thread::spawn(move || {
            let mut output_seq: u64 = 1;
            while output_running.load(Ordering::Relaxed) {
                if !output_active.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                match output_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(bytes) => {
                        let mut chunk = bytes;
                        // Drain any additional buffered output to batch frames.
                        while let Ok(more) = output_rx.try_recv() {
                            chunk.extend_from_slice(&more);
                        }
                        let payload = RawPtyOutputPayload {
                            session_id: output_session_id.clone(),
                            target_id: output_target_id.clone(),
                            output_seq,
                            output_bytes: chunk,
                        };
                        let frame = AuthorityTransportFrame::RawPtyOutput(payload);
                        if write_authority_transport_frame(&mut output_stream, &frame).is_err() {
                            break;
                        }
                        if output_stream.flush().is_err() {
                            break;
                        }
                        output_seq += 1;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        let mut _input_seq: u64 = 1;
        let mut viewer_console_id: Option<String> = None;
        let mut paste_file_assemblers = HashMap::<(String, String), PasteFileAssembler>::new();
        let mut bootstrap_state = BootstrapState::None;
        while running.load(Ordering::Relaxed) {
            maybe_send_bootstrap(&mut bootstrap_state, &session, &io_tx);

            let read_timeout = match bootstrap_state {
                BootstrapState::Pending { deadline } => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    remaining.min(AUTHORITY_TRANSPORT_SOCKET_TIMEOUT)
                }
                BootstrapState::None => AUTHORITY_TRANSPORT_SOCKET_TIMEOUT,
            };
            let _ = listener_stream.set_read_timeout(Some(read_timeout));

            let frame_result = read_authority_transport_frame(&mut listener_stream);
            let mut fatal_error = false;
            match frame_result {
                Ok(AuthorityTransportFrame::ControlPlane(envelope)) => {
                    match envelope.payload {
                        ControlPlanePayload::OpenMirrorRequest(payload) => {
                            viewer_console_id = Some(payload.console_id.clone());
                            session.resize_for_console(
                                &io_tx,
                                payload.cols as u16,
                                payload.rows as u16,
                                payload.console_id,
                            );
                            mirror_active.store(true, Ordering::Relaxed);
                            // Defer bootstrap for a short window so that
                            // short-lived reconnects do not force a full screen
                            // redraw. The bootstrap is sent once the window
                            // expires without a fatal read error.
                            bootstrap_state = BootstrapState::Pending {
                                deadline: Instant::now() + BOOTSTRAP_STABLE_WINDOW,
                            };
                        }
                        ControlPlanePayload::RawPtyInput(payload) => {
                            ERROR_LOG.log(format!(
                                "[ratatui-session-sync] target host received RawPtyInput session={} target={} bytes={}",
                                payload.session_id, payload.target_id, payload.input_bytes.len()
                            ));
                            session.feed_input(&io_tx, payload.input_bytes);
                        }
                        ControlPlanePayload::ApplyResize(payload) => {
                            viewer_console_id = Some(payload.resize_authority_console_id.clone());
                            session.resize_for_console(
                                &io_tx,
                                payload.cols as u16,
                                payload.rows as u16,
                                payload.resize_authority_console_id,
                            );
                        }
                        ControlPlanePayload::CloseMirrorRequest(_) => {
                            mirror_active.store(false, Ordering::Relaxed);
                            if let Some(console_id) = viewer_console_id.take() {
                                session.unregister_console(&io_tx, console_id);
                            }
                        }
                        ControlPlanePayload::PasteFileRequest(payload) => {
                            ERROR_LOG.log(format!(
                                "[ratatui-session-sync] target host received PasteFileRequest session={} target={} file_id={} chunk={}/{} bytes={}",
                                payload.session_id,
                                payload.target_id,
                                payload.file_id,
                                payload.chunk_index,
                                payload.total_chunks,
                                payload.chunk_bytes.len()
                            ));
                            let key = (payload.session_id.clone(), payload.file_id.clone());
                            // Discard incomplete transfers whose last chunk arrived
                            // long enough ago that the transfer is clearly abandoned.
                            paste_file_assemblers.retain(|_, assembler| !assembler.is_stale());
                            if paste_file_assemblers
                                .get(&key)
                                .map(|a| a.total_chunks != payload.total_chunks)
                                .unwrap_or(false)
                            {
                                paste_file_assemblers.remove(&key);
                            }
                            let assembler =
                                paste_file_assemblers.entry(key.clone()).or_insert_with(|| {
                                    PasteFileAssembler::new(
                                        payload.filename_hint.clone(),
                                        payload.total_chunks,
                                    )
                                });
                            assembler.touch();
                            assembler
                                .chunks
                                .insert(payload.chunk_index, payload.chunk_bytes);
                            if assembler.is_complete() {
                                if let Some(assembler) = paste_file_assemblers.remove(&key) {
                                    let filename_hint = assembler.filename_hint.clone();
                                    let full_bytes = assembler.assemble();
                                    match crate::ratatui_node::clipboard_cache::write_clipboard_file(
                                        &filename_hint,
                                        &full_bytes,
                                    ) {
                                        Ok(path) => {
                                            let supports_at =
                                                authority_host_supports_at(&shared, &session);
                                            let path_string = path.to_string_lossy().into_owned();
                                            let path_ref =
                                                format_file_reference(&path_string, supports_at);
                                            session.feed_input(&io_tx, path_ref.into_bytes());
                                        }
                                        Err(error) => {
                                            ERROR_LOG.log(format!(
                                                "[ratatui-session-sync] failed to cache pasted file on authority host: {error}"
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(AuthorityTransportFrame::RawPtyInput(payload)) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-session-sync] target host received raw frame RawPtyInput session={} target={} bytes={}",
                        payload.session_id, payload.target_id, payload.input_bytes.len()
                    ));
                    session.feed_input(&io_tx, payload.input_bytes);
                    _input_seq += 1;
                }
                Ok(AuthorityTransportFrame::Ping) => {
                    let _ = write_authority_transport_frame(
                        &mut listener_stream,
                        &AuthorityTransportFrame::Pong,
                    );
                    let _ = listener_stream.flush();
                }
                Ok(AuthorityTransportFrame::Pong) => {}
                Ok(other) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-session-sync] authority target host unexpected frame: {other:?}"
                    ));
                }
                Err(ref error) if error.is_read_timeout() => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-session-sync] authority target host read error: {error}"
                    ));
                    fatal_error = true;
                }
            }
            if fatal_error {
                break;
            }
            maybe_send_bootstrap(&mut bootstrap_state, &session, &io_tx);
        }
        if let Some(console_id) = viewer_console_id.take() {
            session.unregister_console(&io_tx, console_id);
        }
        running.store(false, Ordering::Relaxed);
    });
}

fn forward_ratatui_host_output(
    mut listener_stream: RemoteControlStream,
    node_id: String,
    bridge_session_id: Arc<RwLock<String>>,
    output_route: SessionSyncAuthorityOutputRoute,
    running: Arc<AtomicBool>,
) -> Result<(), LifecycleError> {
    while running.load(Ordering::Relaxed) {
        match read_authority_transport_frame(&mut listener_stream) {
            Ok(AuthorityTransportFrame::ControlPlane(envelope)) => {
                let session_instance_id = bridge_session_id
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                forward_host_envelope(&output_route, &node_id, &session_instance_id, *envelope)?;
            }
            Ok(AuthorityTransportFrame::RawPtyOutput(payload)) => {
                let session_instance_id = bridge_session_id
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let envelope = ProtocolEnvelope {
                    protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                    message_id: format!("{}-raw-pty-output-{}", node_id, payload.output_seq),
                    message_type: "raw_pty_output",
                    timestamp: String::new(),
                    sender_id: node_id.clone(),
                    correlation_id: None,
                    session_id: Some(payload.session_id.clone()),
                    target_id: Some(payload.target_id.clone()),
                    attachment_id: None,
                    console_id: None,
                    payload: ControlPlanePayload::RawPtyOutput(payload),
                };
                forward_host_envelope(&output_route, &node_id, &session_instance_id, envelope)?;
            }
            Ok(AuthorityTransportFrame::Ping) => {}
            Ok(AuthorityTransportFrame::Pong) => {}
            Ok(other) => {
                return Err(remote_session_sync_error(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected authority frame {other:?}"),
                )));
            }
            Err(ref error) if error.is_read_timeout() => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(remote_session_sync_error(error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::infra::remote_protocol::{
        BootstrapMode, ControlPlanePayload, OpenMirrorRequestPayload, ProtocolEnvelope,
        REMOTE_PROTOCOL_VERSION,
    };
    use crate::infra::remote_transport_codec::{
        write_authority_transport_frame, AuthorityTransportFrame,
    };
    use crate::ratatui_node::authority_host_io_loop::{
        AuthorityHostIoLoop, AuthorityHostIoRequest,
    };
    use crate::ratatui_node::runtime::SharedState;
    use std::fs::File;
    use std::process::Command;

    #[test]
    fn authority_host_paste_uses_local_record_for_agent_command_name() {
        let network = RemoteNetworkConfig::default();
        let shared = SharedState::new(network).expect("SharedState::new should succeed");
        let session_id = "42";
        let local_target_id = ManagedSessionAddress::local(shared.local_authority_id(), session_id)
            .qualified_target();
        let remote_target_id = format!("remote-peer:other-node:{session_id}");

        {
            let mut guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.insert(
                local_target_id.clone(),
                ManagedSessionRecord {
                    address: ManagedSessionAddress::local(shared.local_authority_id(), session_id),
                    selector: None,
                    availability: SessionAvailability::Online,
                    workspace_dir: None,
                    workspace_key: None,
                    session_role: None,
                    opened_by: Vec::new(),
                    attached_clients: 0,
                    window_count: 1,
                    command_name: Some("bash".to_string()),
                    display_command_name: None,
                    agent_command_name: Some("kimi".to_string()),
                    current_path: None,
                    task_state: ManagedSessionTaskState::Input,
                },
            );
        }

        // When the local record has agent_command_name=kimi, the authority host
        // should report that it supports @-references.
        assert!(authority_host_supports_at_from_record(&shared, session_id).unwrap_or(false));

        // If only the viewer-facing remote-peer record exists, the authority host
        // must not use it; the local record is the source of truth.
        {
            let mut guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.remove(&local_target_id);
            guard.insert(
                remote_target_id,
                ManagedSessionRecord {
                    address: ManagedSessionAddress::remote_peer("other-node", session_id),
                    selector: None,
                    availability: SessionAvailability::Online,
                    workspace_dir: None,
                    workspace_key: None,
                    session_role: None,
                    opened_by: Vec::new(),
                    attached_clients: 0,
                    window_count: 1,
                    command_name: Some("kimi".to_string()),
                    display_command_name: None,
                    agent_command_name: Some("kimi".to_string()),
                    current_path: None,
                    task_state: ManagedSessionTaskState::Input,
                },
            );
        }
        assert!(!authority_host_supports_at_from_record(&shared, session_id).unwrap_or(false));
    }

    // Constructs a `RatatuiAuthorityHostSession` with a Unix PTY master, so it
    // only compiles on Unix.
    #[cfg(unix)]
    #[test]
    fn maybe_send_bootstrap_respects_deadline() {
        let session = crate::ratatui_node::authority_host_session::RatatuiAuthorityHostSession {
            session_id: "test".to_string(),
            command_name: "bash".to_string(),
            pty_master: std::fs::File::open("/dev/null").expect("open /dev/null"),
            child: None,
        };
        let io_tx = crate::ratatui_node::authority_host_io_loop::AuthorityHostIoHandle::dangling();

        let mut state = BootstrapState::None;
        assert!(
            !maybe_send_bootstrap(&mut state, &session, &io_tx),
            "None state should not send bootstrap"
        );

        state = BootstrapState::Pending {
            deadline: Instant::now() + Duration::from_secs(60),
        };
        assert!(
            !maybe_send_bootstrap(&mut state, &session, &io_tx),
            "future deadline should not send bootstrap"
        );
        assert!(
            matches!(state, BootstrapState::Pending { .. }),
            "future deadline should keep pending state"
        );

        state = BootstrapState::Pending {
            deadline: Instant::now() - Duration::from_millis(1),
        };
        assert!(
            maybe_send_bootstrap(&mut state, &session, &io_tx),
            "expired deadline should send bootstrap"
        );
        assert!(
            matches!(state, BootstrapState::None),
            "expired deadline should clear pending state"
        );
    }

    /// The pty placeholder for this test is a `UnixStream` pair because the
    /// registered fd must block on read (a null device would report immediate
    /// EOF and the io loop would drop the session), and `Command::new("false")`
    /// is Unix-only. The transport pair uses the cross-platform `socket_pair`.
    #[cfg(unix)]
    #[test]
    fn open_mirror_request_delays_bootstrap_until_stable_window() {
        let session_id = "stable-test";
        let target_id = format!("remote-peer:node:{session_id}");
        let session = Arc::new(RatatuiAuthorityHostSession {
            session_id: session_id.to_string(),
            command_name: "bash".to_string(),
            pty_master: File::open("/dev/null").expect("open /dev/null"),
            child: None,
        });

        let shared = SharedState::new(RemoteNetworkConfig::default()).expect("shared state");
        let io_loop = AuthorityHostIoLoop::start(shared.clone()).expect("start io loop");
        let io_tx = io_loop.sender();

        // Register the session with an output sender so bootstrap has somewhere to go.
        let (output_tx, output_rx) = mpsc::channel();
        let (pty_master, _pty_holder) = UnixStream::pair().expect("create pty pair");
        io_tx
            .send(AuthorityHostIoRequest::RegisterSession {
                session_id: session_id.to_string(),
                pty_master: File::from(std::os::fd::OwnedFd::from(pty_master)),
                child: Command::new("false").spawn().expect("spawn child"),
                output_tx: Some(output_tx.clone()),
                cols: 80,
                rows: 24,
            })
            .expect("register session");
        io_tx
            .send(AuthorityHostIoRequest::SetOutputSender {
                session_id: session_id.to_string(),
                output_tx,
            })
            .expect("set output sender");
        thread::sleep(Duration::from_millis(50));

        // Start the target-host reader over an internal socket pair.
        let (mut host_stream, listener_stream) =
            crate::platform::remote_ipc::socket_pair().expect("create transport pair");
        let running = Arc::new(AtomicBool::new(true));
        spawn_ratatui_authority_target_host(SpawnRatatuiAuthorityTargetHostArgs {
            listener_stream,
            host_stream: host_stream.try_clone().expect("clone host stream"),
            session,
            target_id: target_id.clone(),
            node_id: "node".to_string(),
            running: running.clone(),
            io_tx,
            output_rx,
            shared,
        });

        // Send OpenMirrorRequest from the viewer side.
        let payload = OpenMirrorRequestPayload {
            session_id: session_id.to_string(),
            target_id: target_id.clone(),
            console_id: "console".to_string(),
            cols: 80,
            rows: 24,
            raw_pty_passthrough: true,
            bootstrap_mode: BootstrapMode::Full,
        };
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: "open-mirror-1".to_string(),
            message_type: "open_mirror_request",
            timestamp: "0Z".to_string(),
            sender_id: "node".to_string(),
            correlation_id: None,
            session_id: Some(session_id.to_string()),
            target_id: Some(target_id.clone()),
            attachment_id: None,
            console_id: Some("console".to_string()),
            payload: ControlPlanePayload::OpenMirrorRequest(payload),
        };
        write_authority_transport_frame(
            &mut host_stream,
            &AuthorityTransportFrame::ControlPlane(Box::new(envelope)),
        )
        .expect("write open mirror request");
        host_stream.flush().expect("flush host stream");

        // Bootstrap should NOT arrive immediately; it is deferred by the stable window.
        let start = Instant::now();
        assert!(host_stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .is_ok());
        let early = read_authority_transport_frame(&mut host_stream);
        assert!(
            early.is_err(),
            "bootstrap should not arrive within the stable window"
        );

        // After the stable window expires, the bootstrap frame should arrive.
        let remaining = BOOTSTRAP_STABLE_WINDOW.saturating_sub(start.elapsed());
        if remaining > Duration::ZERO {
            thread::sleep(remaining + Duration::from_millis(20));
        }
        let bootstrap = read_authority_transport_frame(&mut host_stream)
            .expect("bootstrap should arrive after stable window");
        match bootstrap {
            AuthorityTransportFrame::RawPtyOutput(payload) => {
                let text = String::from_utf8_lossy(&payload.output_bytes);
                assert!(
                    text.contains("\x1b[2J"),
                    "bootstrap should contain clear-screen sequence: {text}"
                );
            }
            other => panic!("expected RawPtyOutput bootstrap, got {other:?}"),
        }

        // Tear down: close the socket so the target host reader exits.
        drop(host_stream);
        running.store(false, Ordering::Relaxed);
    }
}
