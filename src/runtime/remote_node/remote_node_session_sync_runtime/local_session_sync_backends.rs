use crate::cli::{RemoteAuthorityTargetHostCommand, RemoteNetworkConfig};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_grpc_transport::RemoteNodeSessionHandle;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, RawPtyOutputPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_authority_transport_frame, write_authority_transport_frame, write_control_plane_envelope,
    AuthorityTransportFrame,
};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxSessionGateway};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::ratatui_node::SharedState;
use crate::runtime::remote_authority_target_host_runtime::RemoteAuthorityTargetHostRuntime;
use crate::runtime::remote_authority_transport_runtime::{
    RemoteAuthorityCommand, AUTHORITY_TRANSPORT_PING_INTERVAL, AUTHORITY_TRANSPORT_READ_TIMEOUT,
    AUTHORITY_TRANSPORT_SOCKET_TIMEOUT,
};
use crate::runtime::remote_node_session_sync_runtime::sync_helpers::remote_session_sync_owner_socket_path;
use crate::runtime::remote_node_session_sync_runtime::{
    remote_session_sync_error, SessionSyncAuthorityHost, SessionSyncAuthorityOutputRoute,
    SessionSyncAuthorityPublicationGateway, LIVE_AUTHORITY_SERVER_ID, SESSION_SYNC_AUTHORITY_ID,
};

use crate::runtime::target_host_runtime::TargetHostRuntime;
use std::io::{self, Write};
use std::net::Shutdown;
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

/// Tmux-backed factory that creates a new local target session using the
/// existing `TargetHostRuntime` path.
#[derive(Clone)]
pub struct TmuxLocalTargetFactory {
    network: RemoteNetworkConfig,
    socket_name: String,
    current_executable: PathBuf,
}

impl TmuxLocalTargetFactory {
    pub fn from_build_env_with_socket(
        network: RemoteNetworkConfig,
        socket_name: &str,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            network,
            socket_name: socket_name.to_string(),
            current_executable: current_waitagent_executable()?,
        })
    }

    pub fn with_network_socket_and_executable(
        network: RemoteNetworkConfig,
        socket_name: String,
        current_executable: PathBuf,
    ) -> Self {
        Self {
            network,
            socket_name,
            current_executable,
        }
    }
}

impl LocalTargetFactory for TmuxLocalTargetFactory {
    type Error = LifecycleError;

    fn create_local_target(
        &self,
        node_id: &str,
        cwd: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<CreatedLocalTarget, Self::Error> {
        let gateway = EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?;
        let runtime = TargetHostRuntime::from_build_env_with_network_and_executable(
            gateway,
            self.network.clone(),
            self.current_executable.clone(),
        )?;
        let workspace = runtime
            .ensure_target_host(WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                cwd,
                &self.socket_name,
                Some(rows).filter(|rows| *rows > 0),
                Some(cols).filter(|cols| *cols > 0),
            ))
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to ensure target host".to_string(),
                    io::Error::new(io::ErrorKind::Other, error.to_string()),
                )
            })?;
        let session_id = workspace.workspace_handle.session_name.as_str().to_string();
        Ok(CreatedLocalTarget {
            target_id: format!("remote-peer:{node_id}:{session_id}"),
            session_id,
        })
    }
}

/// Tmux-backed authority host backend.
///
/// Preserves the original Unix-socket authority listener plus in-process
/// authority target host sidecar model.
#[derive(Clone)]
pub struct TmuxLocalAuthorityHostBackend {
    network: RemoteNetworkConfig,
    current_executable: PathBuf,
}

impl TmuxLocalAuthorityHostBackend {
    pub fn from_build_env(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        Ok(Self {
            network,
            current_executable: current_waitagent_executable()?,
        })
    }

    pub fn with_network_and_executable(
        network: RemoteNetworkConfig,
        current_executable: PathBuf,
    ) -> Self {
        Self {
            network,
            current_executable,
        }
    }
}

const AUTHORITY_HOST_READY_TIMEOUT: Duration = Duration::from_secs(5);

impl LocalAuthorityHostBackend for TmuxLocalAuthorityHostBackend {
    type Error = LifecycleError;

    fn spawn_authority_host(
        &self,
        session_handle: &RemoteNodeSessionHandle,
        target_id: &str,
        output_route: SessionSyncAuthorityOutputRoute,
    ) -> Result<SessionSyncAuthorityHost, Self::Error> {
        let bound_session_instance_id = session_handle.session_instance_id().to_string();
        let session_name = target_session_name_from_target_id(target_id).ok_or_else(|| {
            ERROR_LOG.log(format!(
                "[session-sync] failed to extract session from target id `{target_id}`"
            ));
            LifecycleError::Protocol(format!(
                "failed to derive local session from target id `{target_id}`"
            ))
        })?;
        let socket_name = find_socket_for_session(&session_name).ok_or_else(|| {
            ERROR_LOG.log(format!(
                "[session-sync] no local socket owns session `{session_name}` for `{target_id}`"
            ));
            LifecycleError::Protocol(format!(
                "no local workspace socket owns session `{session_name}` for `{target_id}`"
            ))
        })?;
        let authority_socket_path = live_authority_session_socket_path(&socket_name, &session_name);
        let transport_socket_path = remote_session_sync_owner_socket_path(&socket_name);
        let running = Arc::new(AtomicBool::new(true));
        let writer = Arc::new(Mutex::new(None));
        let writer_ready = Arc::new(Condvar::new());
        let bridge_session_id = Arc::new(RwLock::new(bound_session_instance_id.clone()));
        spawn_live_authority_listener(
            authority_socket_path.clone(),
            session_handle.node_id().to_string(),
            bridge_session_id.clone(),
            output_route,
            running.clone(),
            writer.clone(),
            writer_ready.clone(),
        );
        spawn_in_process_authority_target_host(
            running.clone(),
            writer.clone(),
            writer_ready.clone(),
            self.network.clone(),
            self.current_executable.clone(),
            RemoteAuthorityTargetHostCommand {
                socket_name: socket_name.clone(),
                target_session_name: session_name.clone(),
                transport_session_id: target_session_name_from_target_id(target_id)
                    .unwrap_or_else(|| target_id.to_string()),
                authority_id: session_handle.node_id().to_string(),
                target_id: target_id.to_string(),
                transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
                authority_socket_path: authority_socket_path.to_string_lossy().into_owned(),
            },
        )?;
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
        let target_id = authority_command_target_id(&command).to_string();
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
        if let Err(error) = write_control_plane_envelope(writer, &envelope) {
            let _ = writer.shutdown(Shutdown::Both);
            *guard = None;
            ERROR_LOG.log(format!(
                "[diag-timing] send_command_to_host: write failed for target={target_id}: {error}"
            ));
            return Ok(AuthorityHostSignal::Closed);
        }
        ERROR_LOG.log(format!(
            "[diag-timing] send_command_to_host: sent command to target={target_id}"
        ));
        Ok(AuthorityHostSignal::Ready)
    }

    fn shutdown_authority_host(&self, host: &SessionSyncAuthorityHost) {
        host.running.store(false, Ordering::Relaxed);
        let writer = match host.writer.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => {
                ERROR_LOG
                    .log("[session-sync] authority writer mutex poisoned, recovering".to_string());
                poisoned.into_inner().take()
            }
        };
        if let Some(writer) = writer {
            let _ = writer.shutdown(Shutdown::Both);
        }
    }
}

fn authority_command_target_id(command: &RemoteAuthorityCommand) -> &str {
    match command {
        RemoteAuthorityCommand::OpenMirror(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::CloseMirror(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::RawPtyInput(payload) => payload.target_id.as_str(),
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

fn live_authority_session_socket_path(socket_name: &str, session_name: &str) -> PathBuf {
    remote_session_sync_owner_socket_path(socket_name).with_extension(format!(
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

fn find_socket_for_session(target_session_name: &str) -> Option<String> {
    let backend = EmbeddedTmuxBackend::from_build_env().ok()?;
    let sockets = backend.discover_waitagent_sockets().ok()?;
    for socket_name in &sockets {
        let sessions = backend.list_sessions_on_socket(socket_name).ok()?;
        if sessions
            .iter()
            .any(|s| s.address.session_id() == target_session_name)
        {
            return Some(socket_name.as_str().to_string());
        }
        let pane_backed = backend
            .list_local_target_content_pane_sessions(socket_name)
            .ok()?;
        if pane_backed
            .iter()
            .any(|s| s.address.session_id() == target_session_name)
        {
            return Some(socket_name.as_str().to_string());
        }
    }
    None
}

fn spawn_in_process_authority_target_host(
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    writer_ready: Arc<Condvar>,
    network: RemoteNetworkConfig,
    current_executable: PathBuf,
    command: RemoteAuthorityTargetHostCommand,
) -> Result<(), LifecycleError> {
    let gateway = EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?;
    let runtime = RemoteAuthorityTargetHostRuntime::new(
        gateway,
        SessionSyncAuthorityPublicationGateway::new(network.clone()),
        current_executable,
    )
    .with_network(network);
    let authority_socket_path = PathBuf::from(&command.authority_socket_path);
    let target_id_for_log = command.target_id.clone();
    thread::spawn(move || {
        let run_result = runtime.run_target_host(command);
        if let Err(ref error) = run_result {
            ERROR_LOG.log(format!(
                "[session-sync] authority target host for target={target_id_for_log} exited with error: {error}"
            ));
        }
        running.store(false, Ordering::Relaxed);
        let writer_val = match writer.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => {
                ERROR_LOG.log(
                    "[session-sync] authority writer mutex poisoned during host cleanup, recovering".to_string()
                );
                poisoned.into_inner().take()
            }
        };
        if let Some(writer) = writer_val {
            let _ = writer.shutdown(Shutdown::Both);
        }
        writer_ready.notify_all();
        let _ = UnixStream::connect(&authority_socket_path);
    });
    Ok(())
}

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
        let _ = std::fs::remove_file(&socket_path);
    });
}

fn bind_live_authority_listener(socket_path: &Path) -> Result<UnixListener, io::Error> {
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn bridge_live_authority_stream(
    mut host_stream: UnixStream,
    node_id: String,
    bridge_session_id: Arc<RwLock<String>>,
    output_route: SessionSyncAuthorityOutputRoute,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    writer_ready: Arc<Condvar>,
) -> Result<(), LifecycleError> {
    use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
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
    ERROR_LOG.log("[diag-timing] bridge_live_authority_stream: writer set, ready signalled, starting forward_host_output_to_owner".to_string());
    let result = forward_host_output(
        host_reader,
        writer.clone(),
        node_id,
        bridge_session_id,
        output_route,
        running.clone(),
    );
    ERROR_LOG.log(format!("[diag-timing] bridge_live_authority_stream: forward_host_output_to_owner exited, result={:?}", result));
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
                forward_host_envelope(&output_route, &node_id, &session_instance_id, envelope)?;
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
                ERROR_LOG.log(format!(
                    "[diag-timing] forward_host_output_to_session: unexpected authority frame {other:?}, exiting"
                ));
                return Err(remote_session_sync_error(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unexpected authority frame {other:?}"),
                )));
            }
            Err(ref e) if e.is_read_timeout() => {
                if let Some(sent_at) = keepalive_sent_at {
                    if sent_at.elapsed() >= ping_timeout {
                        let session_instance_id = bridge_session_id
                            .read()
                            .map(|g| g.clone())
                            .unwrap_or_default();
                        ERROR_LOG.log(format!(
                            "[diag-timing] forward_host_output: keepalive timed out for node={node_id} session={session_instance_id}, declaring host dead"
                        ));
                        return Err(remote_session_sync_error(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "authority host keepalive timed out",
                        )));
                    }
                    continue;
                }
                if last_received.elapsed() >= ping_interval {
                    if let Err(error) = send_authority_ping(&writer) {
                        ERROR_LOG.log(format!(
                            "[diag-timing] forward_host_output: failed to send keepalive ping for node={node_id}: {error}"
                        ));
                        return Err(error);
                    }
                    keepalive_sent_at = Some(Instant::now());
                }
            }
            Err(e) => {
                ERROR_LOG.log(format!(
                    "[diag-timing] forward_host_output_to_session: read_authority_transport_frame failed: {e}, exiting"
                ));
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
            ERROR_LOG.log(format!(
                "[diag-timing] forward_host_output: forwarding envelope type={} to session-sync owner",
                envelope.payload.message_type()
            ));
            if let Err(e) = session_event_tx.send(
                crate::runtime::remote_node_session_sync_runtime::SessionSyncEvent::AuthorityHostOutput(envelope),
            ) {
                ERROR_LOG.log(format!(
                    "[diag-timing] forward_host_output: owner event send failed: {e}, exiting"
                ));
                return Err(remote_session_sync_error(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    e.to_string(),
                )));
            }
        }
        SessionSyncAuthorityOutputRoute::IngressEvent(ingress_event_tx) => {
            ERROR_LOG.log(format!(
                "[diag-timing] forward_host_output: forwarding envelope type={} to ingress owner",
                envelope.payload.message_type()
            ));
            if let Err(e) = ingress_event_tx.send(
                crate::runtime::remote_node::remote_node_ingress_server_runtime::InternalEvent::AuthorityHostOutput {
                    node_id: node_id.to_string(),
                    session_instance_id: session_instance_id.to_string(),
                    envelope,
                },
            ) {
                ERROR_LOG.log(format!(
                    "[diag-timing] forward_host_output: ingress event send failed: {e}, exiting"
                ));
                return Err(remote_session_sync_error(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    e.to_string(),
                )));
            }
        }
    }
    Ok(())
}

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
/// catalog instead of querying a tmux server.
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
                crate::runtime::ratatui_node::state_event::StateEvent::CreateAuthorityHostSession {
                    request_id: node_id.to_string(),
                    cols,
                    rows,
                    reply_tx,
                },
            )
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to send create-authority-host-session event".to_string(),
                    io::Error::new(io::ErrorKind::Other, error.to_string()),
                )
            })?;
        let created = reply_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| {
                LifecycleError::Io(
                    "state event loop did not reply to create-authority-host-session".to_string(),
                    io::Error::new(io::ErrorKind::Other, error.to_string()),
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
                .authority_host_sessions
                .lock()
                .map_err(|error| {
                    LifecycleError::Io(
                        "ratatui authority host sessions mutex poisoned".to_string(),
                        io::Error::new(io::ErrorKind::Other, error.to_string()),
                    )
                })?;
            guard.get(&session_name).cloned().ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "ratatui authority host session `{session_name}` does not exist"
                ))
            })?
        };

        let (host_end, listener_end) = UnixStream::pair().map_err(|error| {
            LifecycleError::Io(
                "failed to create authority host socket pair".to_string(),
                error,
            )
        })?;
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
        self.shared
            .authority_host_io_sender()
            .send(crate::runtime::ratatui_node::authority_host_io_loop::AuthorityHostIoRequest::SetOutputSender {
                session_id: session_name.clone(),
                output_tx,
            })
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to set authority host output sender".to_string(),
                    io::Error::new(io::ErrorKind::Other, error.to_string()),
                )
            })?;
        let io_tx = self.shared.authority_host_io_sender();

        let writer = Arc::new(Mutex::new(None));
        let writer_ready = Arc::new(Condvar::new());
        let running = Arc::new(AtomicBool::new(true));
        let bridge_session_id = Arc::new(RwLock::new(bound_session_instance_id.clone()));

        spawn_ratatui_authority_listener(
            host_end.try_clone().map_err(|error| {
                LifecycleError::Io("failed to clone host stream".to_string(), error)
            })?,
            listener_end.try_clone().map_err(|error| {
                LifecycleError::Io("failed to clone listener stream".to_string(), error)
            })?,
            node_id.clone(),
            bridge_session_id.clone(),
            output_route,
            running.clone(),
            writer.clone(),
            writer_ready.clone(),
        );

        spawn_ratatui_authority_target_host(
            listener_end,
            host_end,
            session,
            target_id.to_string(),
            node_id,
            running.clone(),
            io_tx,
            output_rx,
        );

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
            host.writer.lock().map_or(false, |g| g.is_some())
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
        if let Err(error) = write_control_plane_envelope(writer, &envelope) {
            let _ = writer.shutdown(Shutdown::Both);
            *guard = None;
            ERROR_LOG.log(format!(
                "[diag-timing] send_command_to_host: write failed for target={target_id}: {error}"
            ));
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

fn spawn_ratatui_authority_listener(
    host_stream: UnixStream,
    _listener_stream: UnixStream,
    node_id: String,
    bridge_session_id: Arc<RwLock<String>>,
    output_route: SessionSyncAuthorityOutputRoute,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    writer_ready: Arc<Condvar>,
) {
    thread::spawn(move || {
        // The ratatui authority host is backed by an internal UnixStream::pair.
        // The listener owns one end of the pair and uses it for both writing
        // viewer commands and reading PTY output from the target host.
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

fn spawn_ratatui_authority_target_host(
    mut listener_stream: UnixStream,
    _host_stream: UnixStream,
    session: Arc<crate::runtime::ratatui_node::authority_host_session::RatatuiAuthorityHostSession>,
    target_id: String,
    _node_id: String,
    running: Arc<AtomicBool>,
    io_tx: crate::runtime::ratatui_node::authority_host_io_loop::AuthorityHostIoHandle,
    output_rx: mpsc::Receiver<Vec<u8>>,
) {
    thread::spawn(move || {
        let session_id = session.session_id.clone();
        // The target host owns the other end of the UnixStream::pair. It reads
        // viewer commands from this end and forwards them to AuthorityHostIoLoop.
        // PTY output arrives on output_rx and is framed as RawPtyOutput back to
        // the listener, which forwards it over the gRPC bridge.

        let mirror_active = Arc::new(AtomicBool::new(false));
        let mut output_stream = listener_stream
            .try_clone()
            .expect("failed to clone listener stream for output pump");
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
        while running.load(Ordering::Relaxed) {
            match read_authority_transport_frame(&mut listener_stream) {
                Ok(AuthorityTransportFrame::ControlPlane(envelope)) => match envelope.payload {
                    ControlPlanePayload::OpenMirrorRequest(payload) => {
                        viewer_console_id = Some(payload.console_id.clone());
                        session.resize_for_console(
                            &io_tx,
                            payload.cols as u16,
                            payload.rows as u16,
                            payload.console_id,
                        );
                        mirror_active.store(true, Ordering::Relaxed);
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
                    _ => {}
                },
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
                    break;
                }
            }
        }
        if let Some(console_id) = viewer_console_id.take() {
            session.unregister_console(&io_tx, console_id);
        }
        running.store(false, Ordering::Relaxed);
    });
}

fn forward_ratatui_host_output(
    mut listener_stream: UnixStream,
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
                forward_host_envelope(&output_route, &node_id, &session_instance_id, envelope)?;
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
pub(crate) fn authority_host_signal(host: &SessionSyncAuthorityHost) -> AuthorityHostSignal {
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

#[cfg(test)]
pub(crate) fn deliver_command_to_ready_host(
    host: &SessionSyncAuthorityHost,
    command: RemoteAuthorityCommand,
) -> Result<AuthorityHostSignal, LifecycleError> {
    const AUTHORITY_HOST_READY_TIMEOUT: Duration = Duration::from_secs(5);
    let target_id = authority_command_target_id(&command).to_string();
    let mut guard = match host.writer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    while guard.is_none() && host.running.load(Ordering::Relaxed) {
        let wait_result = host
            .writer_ready
            .wait_timeout(guard, AUTHORITY_HOST_READY_TIMEOUT)
            .map_err(|_| {
                LifecycleError::Protocol(
                    "authority writer mutex poisoned while waiting for ready signal".to_string(),
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
    if let Err(error) = write_control_plane_envelope(writer, &envelope) {
        let _ = writer.shutdown(Shutdown::Both);
        *guard = None;
        ERROR_LOG.log(format!(
            "[diag-timing] send_command_to_host: write failed for target={target_id}: {error}"
        ));
        return Ok(AuthorityHostSignal::Closed);
    }
    Ok(AuthorityHostSignal::Ready)
}
