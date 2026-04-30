use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    NodeSessionEnvelope as GrpcNodeSessionEnvelope, RouteContext, TargetExited, TargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, OutboundNodeSessionRequest,
    RemoteNodeSessionHandle, RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxSessionGateway};
use crate::lifecycle::LifecycleError;
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION_SYNC_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SESSION_SYNC_RECONNECT_DELAY: Duration = Duration::from_millis(500);

pub trait LocalSessionCatalog: Send + 'static {
    type Error: ToString;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error>;
}

impl LocalSessionCatalog for EmbeddedTmuxBackend {
    type Error = crate::infra::tmux::TmuxError;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        self.list_sessions()
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

pub struct RemoteNodeSessionSyncRuntime<G = EmbeddedTmuxBackend, T = GrpcRemoteNodeTransport> {
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

impl RemoteNodeSessionSyncRuntime {
    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self::new(
            EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?,
            GrpcRemoteNodeTransport::new(),
            network,
        ))
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

#[cfg(test)]
mod tests {
    use super::{
        compute_session_sync_delta, local_sessions_by_local_id, remote_session_exited_envelope,
        remote_session_published_envelope, LocalSessionCatalog, OutboundRemoteNodeTransport,
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
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket_name, session_id),
            selector: Some(format!("{socket_name}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some(session_id.to_string()),
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
