use crate::cli::{prepend_global_network_args, RemoteNetworkConfig, RemoteSessionSyncOwnerCommand};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    NodeSessionEnvelope as GrpcNodeSessionEnvelope, RouteContext, TargetExited, TargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, OutboundNodeSessionRequest,
    RemoteNodeSessionHandle, RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxSessionGateway, TmuxSocketName};
use crate::lifecycle::LifecycleError;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION_SYNC_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SESSION_SYNC_RECONNECT_DELAY: Duration = Duration::from_millis(500);
const REMOTE_SESSION_SYNC_OWNER_READY_RETRIES: usize = 20;
const REMOTE_SESSION_SYNC_OWNER_READY_SLEEP: Duration = Duration::from_millis(25);

pub trait LocalSessionCatalog: Send + 'static {
    type Error: ToString;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error>;
}

#[derive(Clone)]
pub struct SocketScopedLocalSessionCatalog<G> {
    gateway: G,
    socket_name: TmuxSocketName,
}

impl<G> SocketScopedLocalSessionCatalog<G> {
    pub fn new(gateway: G, socket_name: TmuxSocketName) -> Self {
        Self {
            gateway,
            socket_name,
        }
    }
}

impl<G> LocalSessionCatalog for SocketScopedLocalSessionCatalog<G>
where
    G: TmuxSessionGateway + Send + 'static,
    G::Error: ToString,
{
    type Error = G::Error;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        let sessions = self.gateway.list_sessions_on_socket(&self.socket_name)?;
        Ok(exportable_local_sessions_for_socket(
            sessions,
            self.socket_name.as_str(),
        ))
    }
}

pub trait OutboundRemoteNodeTransport: Clone + Send + 'static {
    type Guard: Send + 'static;
    type Error: ToString;

    fn connect_outbound(
        &self,
        request: OutboundNodeSessionRequest,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<Self::Guard, Self::Error>;
}

impl OutboundRemoteNodeTransport for GrpcRemoteNodeTransport {
    type Guard = GrpcRemoteNodeTransportGuard;
    type Error = crate::infra::remote_grpc_transport::RemoteNodeTransportError;

    fn connect_outbound(
        &self,
        request: OutboundNodeSessionRequest,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<Self::Guard, Self::Error> {
        RemoteNodeTransport::connect_outbound(self, request, event_tx)
    }
}

pub struct RemoteNodeSessionSyncRuntime<G, T = GrpcRemoteNodeTransport> {
    gateway: G,
    transport: T,
    network: RemoteNetworkConfig,
    poll_interval: Duration,
    reconnect_delay: Duration,
}

pub struct RemoteNodeSessionSyncGuard {
    stop_tx: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl RemoteNodeSessionSyncRuntime<SocketScopedLocalSessionCatalog<EmbeddedTmuxBackend>> {
    pub fn from_build_env_with_network_and_socket(
        network: RemoteNetworkConfig,
        socket_name: &str,
    ) -> Result<Self, LifecycleError> {
        Ok(Self::new(
            SocketScopedLocalSessionCatalog::new(
                EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?,
                TmuxSocketName::new(socket_name),
            ),
            GrpcRemoteNodeTransport::new(),
            network,
        ))
    }

    pub fn run_owner(
        command: RemoteSessionSyncOwnerCommand,
        network: RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let socket_path = remote_session_sync_owner_socket_path(&command.socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(remote_session_sync_error)?;
        listener
            .set_nonblocking(true)
            .map_err(remote_session_sync_error)?;
        let _guard =
            Self::from_build_env_with_network_and_socket(network, &command.socket_name)?.start()?;
        while backend_socket_still_exists(&command.socket_name) {
            let _ = drain_owner_ping(&listener);
            thread::sleep(SESSION_SYNC_POLL_INTERVAL);
        }
        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    pub fn ensure_owner_running(
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let socket_path = remote_session_sync_owner_socket_path(socket_name);
        if remote_session_sync_owner_available(&socket_path) {
            return Ok(());
        }
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        spawn_waitagent_sidecar(
            &current_executable,
            remote_session_sync_owner_args(socket_name, network),
        )
        .map_err(remote_session_sync_error)?;
        for _ in 0..REMOTE_SESSION_SYNC_OWNER_READY_RETRIES {
            if remote_session_sync_owner_available(&socket_path) {
                return Ok(());
            }
            thread::sleep(REMOTE_SESSION_SYNC_OWNER_READY_SLEEP);
        }
        Err(LifecycleError::Protocol(format!(
            "remote session sync owner for socket `{socket_name}` did not become ready"
        )))
    }
}

impl<G, T> RemoteNodeSessionSyncRuntime<G, T>
where
    G: LocalSessionCatalog,
    T: OutboundRemoteNodeTransport,
{
    pub fn new(gateway: G, transport: T, network: RemoteNetworkConfig) -> Self {
        Self {
            gateway,
            transport,
            network,
            poll_interval: SESSION_SYNC_POLL_INTERVAL,
            reconnect_delay: SESSION_SYNC_RECONNECT_DELAY,
        }
    }

    pub fn start(self) -> Result<RemoteNodeSessionSyncGuard, LifecycleError> {
        let endpoint_uri = self.network.connect_endpoint_uri().ok_or_else(|| {
            LifecycleError::Protocol("remote session sync requires `--connect`".to_string())
        })?;
        let node_id = self.network.advertised_node_id();
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_remote_session_sync_loop(
                self.gateway,
                self.transport,
                node_id,
                endpoint_uri,
                self.poll_interval,
                self.reconnect_delay,
                stop_rx,
            );
        });
        Ok(RemoteNodeSessionSyncGuard {
            stop_tx: Some(stop_tx),
            worker: Some(worker),
        })
    }
}

impl Drop for RemoteNodeSessionSyncGuard {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_remote_session_sync_loop<G, T>(
    gateway: G,
    transport: T,
    node_id: String,
    endpoint_uri: String,
    poll_interval: Duration,
    reconnect_delay: Duration,
    stop_rx: mpsc::Receiver<()>,
) where
    G: LocalSessionCatalog,
    T: OutboundRemoteNodeTransport,
{
    let mut next_message_id = 0_u64;
    loop {
        if should_stop(&stop_rx) {
            return;
        }

        let (event_tx, event_rx) = mpsc::channel();
        let _transport_guard = match transport.connect_outbound(
            OutboundNodeSessionRequest {
                node_id: node_id.clone(),
                endpoint_uri: endpoint_uri.clone(),
            },
            event_tx,
        ) {
            Ok(guard) => guard,
            Err(_) => {
                if wait_or_stop(&stop_rx, reconnect_delay) {
                    return;
                }
                continue;
            }
        };

        let mut active_session = None;
        let mut synced_sessions = HashMap::<String, ManagedSessionRecord>::new();
        let mut should_reconnect = false;

        while !should_reconnect {
            if let Ok(event) = event_rx.recv_timeout(poll_interval) {
                should_reconnect |= handle_transport_event(event, &mut active_session);
                while let Ok(event) = event_rx.try_recv() {
                    should_reconnect |= handle_transport_event(event, &mut active_session);
                }
            }

            if should_stop(&stop_rx) {
                return;
            }

            let Some(session_handle) = active_session.as_ref() else {
                continue;
            };
            if let Err(_) = sync_local_sessions(
                &gateway,
                &node_id,
                session_handle,
                &mut synced_sessions,
                &mut next_message_id,
            ) {
                should_reconnect = true;
            }
        }

        if wait_or_stop(&stop_rx, reconnect_delay) {
            return;
        }
    }
}

fn handle_transport_event(
    event: RemoteNodeTransportEvent,
    active_session: &mut Option<RemoteNodeSessionHandle>,
) -> bool {
    match event {
        RemoteNodeTransportEvent::SessionOpened { session } => {
            *active_session = Some(session);
            false
        }
        RemoteNodeTransportEvent::EnvelopeReceived { .. } => false,
        RemoteNodeTransportEvent::SessionClosed { .. }
        | RemoteNodeTransportEvent::TransportFailed { .. } => {
            *active_session = None;
            true
        }
    }
}

fn sync_local_sessions<G>(
    gateway: &G,
    node_id: &str,
    session_handle: &RemoteNodeSessionHandle,
    synced_sessions: &mut HashMap<String, ManagedSessionRecord>,
    next_message_id: &mut u64,
) -> Result<(), io::Error>
where
    G: LocalSessionCatalog,
{
    let local_sessions = match gateway.list_local_sessions() {
        Ok(sessions) => sessions,
        Err(_) => return Ok(()),
    };
    let current_sessions = local_sessions_by_local_id(local_sessions);
    let delta = compute_session_sync_delta(synced_sessions, &current_sessions);

    for session in &delta.publish {
        next_message_id_increment(next_message_id);
        session_handle
            .send(remote_session_published_envelope(
                node_id,
                session_handle.session_instance_id(),
                *next_message_id,
                session,
            ))
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
    }

    for previous in &delta.exit {
        next_message_id_increment(next_message_id);
        session_handle
            .send(remote_session_exited_envelope(
                node_id,
                session_handle.session_instance_id(),
                *next_message_id,
                previous.address.session_id(),
            ))
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
    }

    *synced_sessions = current_sessions;
    Ok(())
}

fn local_sessions_by_local_id(
    sessions: Vec<ManagedSessionRecord>,
) -> HashMap<String, ManagedSessionRecord> {
    sessions
        .into_iter()
        .map(|session| (session.address.id().as_str().to_string(), session))
        .collect()
}

fn exportable_local_sessions_for_socket(
    sessions: Vec<ManagedSessionRecord>,
    socket_name: &str,
) -> Vec<ManagedSessionRecord> {
    sessions
        .into_iter()
        .filter(|session| session.address.server_id() == socket_name)
        .filter(|session| session.is_target_host())
        .collect()
}

#[derive(Debug)]
struct SessionSyncDelta {
    publish: Vec<ManagedSessionRecord>,
    exit: Vec<ManagedSessionRecord>,
}

fn compute_session_sync_delta(
    previous: &HashMap<String, ManagedSessionRecord>,
    current: &HashMap<String, ManagedSessionRecord>,
) -> SessionSyncDelta {
    let publish = current
        .iter()
        .filter_map(|(local_id, session)| {
            if previous.get(local_id) == Some(session) {
                None
            } else {
                Some(session.clone())
            }
        })
        .collect::<Vec<_>>();
    let exit = previous
        .iter()
        .filter_map(|(local_id, session)| {
            if current.contains_key(local_id) {
                None
            } else {
                Some(session.clone())
            }
        })
        .collect::<Vec<_>>();
    SessionSyncDelta { publish, exit }
}

fn next_message_id_increment(next_message_id: &mut u64) {
    *next_message_id = next_message_id.saturating_add(1);
}

fn remote_session_published_envelope(
    node_id: &str,
    session_instance_id: &str,
    message_sequence: u64,
    session: &ManagedSessionRecord,
) -> GrpcNodeSessionEnvelope {
    let target_id = format!("remote-peer:{node_id}:{}", session.address.session_id());
    GrpcNodeSessionEnvelope {
        message_id: format!("{node_id}-session-sync-{message_sequence}"),
        sent_at: Some(timestamp_now()),
        session_instance_id: session_instance_id.to_string(),
        correlation_id: None,
        route: Some(RouteContext {
            authority_node_id: Some(node_id.to_string()),
            target_id: Some(target_id.clone()),
            attachment_id: None,
            console_id: None,
            console_host_id: None,
            session_id: Some(session.address.session_id().to_string()),
        }),
        body: Some(Body::TargetPublished(TargetPublished {
            target_id,
            authority_node_id: node_id.to_string(),
            transport: "tmux".to_string(),
            transport_session_id: session.address.session_id().to_string(),
            selector: session.selector.clone(),
            availability: session.availability.as_str().to_string(),
            command_name: session.command_name.clone(),
            current_path: session
                .current_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            attached_count: Some(session.attached_clients as u64),
            session_role: session.session_role.map(|role| role.as_str().to_string()),
            workspace_key: session.workspace_key.clone(),
            window_count: Some(session.window_count as u64),
            task_state: Some(session.task_state.as_str().to_string()),
        })),
    }
}

fn remote_session_exited_envelope(
    node_id: &str,
    session_instance_id: &str,
    message_sequence: u64,
    transport_session_id: &str,
) -> GrpcNodeSessionEnvelope {
    let target_id = format!("remote-peer:{node_id}:{transport_session_id}");
    GrpcNodeSessionEnvelope {
        message_id: format!("{node_id}-session-sync-{message_sequence}"),
        sent_at: Some(timestamp_now()),
        session_instance_id: session_instance_id.to_string(),
        correlation_id: None,
        route: Some(RouteContext {
            authority_node_id: Some(node_id.to_string()),
            target_id: Some(target_id.clone()),
            attachment_id: None,
            console_id: None,
            console_host_id: None,
            session_id: Some(transport_session_id.to_string()),
        }),
        body: Some(Body::TargetExited(TargetExited {
            target_id,
            transport_session_id: transport_session_id.to_string(),
        })),
    }
}

fn timestamp_now() -> prost_types::Timestamp {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: now.as_secs() as i64,
        nanos: now.subsec_nanos() as i32,
    }
}

fn should_stop(stop_rx: &mpsc::Receiver<()>) -> bool {
    stop_rx.try_recv().is_ok()
}

fn wait_or_stop(stop_rx: &mpsc::Receiver<()>, duration: Duration) -> bool {
    stop_rx.recv_timeout(duration).is_ok()
}

fn remote_session_sync_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to start remote session sync runtime".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn remote_session_sync_owner_args(socket_name: &str, network: &RemoteNetworkConfig) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-session-sync-owner".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

fn remote_session_sync_owner_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-session-sync-owner-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

fn remote_session_sync_owner_available(socket_path: &Path) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

fn drain_owner_ping(listener: &UnixListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok((_stream, _addr)) => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn backend_socket_still_exists(socket_name: &str) -> bool {
    let socket_path = crate::infra::tmux::tmux_socket_dir().join(socket_name);
    if !socket_path.exists() {
        return false;
    }
    EmbeddedTmuxBackend::from_build_env()
        .map(|backend| backend.socket_is_live(&TmuxSocketName::new(socket_name)))
        .unwrap_or(false)
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        compute_session_sync_delta, exportable_local_sessions_for_socket,
        local_sessions_by_local_id, remote_session_exited_envelope,
        remote_session_published_envelope, remote_session_sync_owner_available,
        remote_session_sync_owner_socket_path, LocalSessionCatalog, OutboundRemoteNodeTransport,
        RemoteNodeSessionSyncRuntime,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_transport::{
        OutboundNodeSessionRequest, RemoteNodeSessionHandle, RemoteNodeTransportEvent,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn session_sync_delta_publishes_new_and_removed_sessions() {
        let previous = HashMap::from([(
            "local-tmux:wa-1:shell-old".to_string(),
            session("wa-1", "shell-old"),
        )]);
        let current = local_sessions_by_local_id(vec![
            session("wa-1", "shell-1"),
            session("wa-1", "shell-2"),
        ]);

        let delta = compute_session_sync_delta(&previous, &current);

        assert_eq!(delta.publish.len(), 2);
        assert_eq!(delta.exit.len(), 1);
        assert_eq!(delta.exit[0].address.session_id(), "shell-old");
    }

    #[test]
    fn remote_session_published_envelope_uses_remote_peer_identity() {
        let envelope = remote_session_published_envelope(
            "10.0.0.2",
            "server-session-1",
            7,
            &session("wa-1", "shell-1"),
        );

        let Some(Body::TargetPublished(payload)) = envelope.body else {
            panic!("expected target_published body");
        };
        assert_eq!(payload.target_id, "remote-peer:10.0.0.2:shell-1");
        assert_eq!(payload.transport_session_id, "shell-1");
    }

    #[test]
    fn remote_session_exited_envelope_uses_remote_peer_identity() {
        let envelope = remote_session_exited_envelope("10.0.0.2", "server-session-1", 8, "shell-1");

        let Some(Body::TargetExited(payload)) = envelope.body else {
            panic!("expected target_exited body");
        };
        assert_eq!(payload.target_id, "remote-peer:10.0.0.2:shell-1");
        assert_eq!(payload.transport_session_id, "shell-1");
    }

    #[test]
    fn runtime_start_publishes_local_sessions_after_session_open() {
        let receiver_slot = Arc::new(Mutex::new(None));
        let runtime = RemoteNodeSessionSyncRuntime {
            gateway: FakeGateway {
                sessions: vec![session("wa-1", "shell-1")],
            },
            transport: FakeTransport {
                receiver_slot: receiver_slot.clone(),
            },
            network: RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
            },
            poll_interval: Duration::from_millis(10),
            reconnect_delay: Duration::from_millis(10),
        };

        let guard = runtime.start().expect("runtime should start");
        let start = std::time::Instant::now();
        let envelope = loop {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("timed out waiting for outbound session sync envelope");
            }
            if let Some(envelope) = try_take_envelope(&receiver_slot) {
                break envelope;
            }
            thread::sleep(Duration::from_millis(10));
        };

        let Some(Body::TargetPublished(payload)) = envelope.body else {
            panic!("expected target_published body");
        };
        assert_eq!(payload.transport_session_id, "shell-1");
        drop(guard);
    }

    #[test]
    fn exportable_local_sessions_for_socket_keeps_only_target_hosts_on_current_socket() {
        let sessions = exportable_local_sessions_for_socket(
            vec![
                session_with_role("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
                session_with_role("wa-1", "shell-1", WorkspaceSessionRole::TargetHost),
                session_with_role("wa-2", "shell-2", WorkspaceSessionRole::TargetHost),
            ],
            "wa-1",
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].address.server_id(), "wa-1");
        assert_eq!(sessions[0].address.session_id(), "shell-1");
        assert!(sessions[0].is_target_host());
    }

    #[test]
    fn remote_session_sync_owner_available_observes_bound_owner_socket() {
        let socket_name = format!("wa-test-sync-owner-{}", std::process::id());
        let socket_path = remote_session_sync_owner_socket_path(&socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        assert!(!remote_session_sync_owner_available(&socket_path));
        let listener = UnixListener::bind(&socket_path).expect("owner socket should bind");
        assert!(remote_session_sync_owner_available(&socket_path));
        drop(listener);
        let _ = fs::remove_file(&socket_path);
    }

    #[derive(Clone)]
    struct FakeGateway {
        sessions: Vec<ManagedSessionRecord>,
    }

    impl LocalSessionCatalog for FakeGateway {
        type Error = &'static str;

        fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            Ok(self.sessions.clone())
        }
    }

    #[derive(Clone)]
    struct FakeTransport {
        receiver_slot: Arc<
            Mutex<
                Option<
                    tokio::sync::mpsc::UnboundedReceiver<
                        crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope,
                    >,
                >,
            >,
        >,
    }

    struct FakeGuard;

    impl OutboundRemoteNodeTransport for FakeTransport {
        type Guard = FakeGuard;
        type Error = &'static str;

        fn connect_outbound(
            &self,
            request: OutboundNodeSessionRequest,
            event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
        ) -> Result<Self::Guard, Self::Error> {
            let (handle, receiver) =
                RemoteNodeSessionHandle::new_for_tests(request.node_id, "server-session-1");
            *self
                .receiver_slot
                .lock()
                .expect("receiver slot mutex should not be poisoned") = Some(receiver);
            event_tx
                .send(RemoteNodeTransportEvent::SessionOpened { session: handle })
                .map_err(|_| "failed to deliver session open event")?;
            Ok(FakeGuard)
        }
    }

    fn try_take_envelope(
        receiver_slot: &Arc<
            Mutex<
                Option<
                    tokio::sync::mpsc::UnboundedReceiver<
                        crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope,
                    >,
                >,
            >,
        >,
    ) -> Option<crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope> {
        receiver_slot
            .lock()
            .expect("receiver slot mutex should not be poisoned")
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
    }

    fn session(socket_name: &str, session_id: &str) -> ManagedSessionRecord {
        session_with_role(socket_name, session_id, WorkspaceSessionRole::TargetHost)
    }

    fn session_with_role(
        socket_name: &str,
        session_id: &str,
        session_role: WorkspaceSessionRole,
    ) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket_name, session_id),
            selector: Some(format!("{socket_name}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some(session_id.to_string()),
            session_role: Some(session_role),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
