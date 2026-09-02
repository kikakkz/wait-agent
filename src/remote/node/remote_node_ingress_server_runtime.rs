// Legacy tmux-era node ingress runtime kept during the ratatui migration; many items are currently unused.

use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    ApplyPtyResize, CloseMirrorRequest, CreateSessionRequest,
    NodeSessionEnvelope as GrpcNodeSessionEnvelope, OpenMirrorRequest, PasteFileRequest,
    RawPtyInput, RouteContext, TargetExited as GrpcTargetExited,
    TargetPublicationAck as GrpcTargetPublicationAck,
    TargetPublicationAckStatus as GrpcTargetPublicationAckStatus,
    TargetPublished as GrpcTargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, OutboundNodeSessionRequest,
    RemoteNodeSessionHandle, RemoteNodeTransport, RemoteNodeTransportError,
    RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{
    BootstrapMode, ControlPlanePayload, CreateSessionAcceptedPayload, CreateSessionRejectedPayload,
    PasteFileRequestPayload, ProtocolEnvelope, TargetExitedPayload, TargetPublicationAckPayload,
    TargetPublicationAckStatus, TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_node_session_envelope, write_node_session_envelope,
};
use crate::lifecycle::LifecycleError;
use crate::platform::remote_ipc::{
    authority_endpoint_from_id_string, authority_endpoint_id_string, cleanup_remote_listener,
    discover_authority_endpoints, remote_listener_is_running, remote_node_ingress_owner_addr,
    remote_node_ingress_startup_lock_path, remote_ready_addr, stable_socket_hash,
    RemoteControlAddr, RemoteControlListener, RemoteControlStream,
};
use crate::process::current_executable::current_waitagent_executable;
use crate::process::startup_lock::StartupLock;
use crate::process::workspace::sidecar_process_runtime::spawn_waitagent_sidecar_child;
use crate::ratatui_node::runtime::probe_server_can_reach_peer;
use crate::remote::authority::remote_authority_transport_runtime::{
    authority_target_component, RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
use crate::remote::node::remote_node_session_runtime::{
    map_inbound_grpc_authority_event, map_outbound_grpc_envelope,
};
use crate::remote::node::remote_node_session_sync_runtime::{
    compute_session_sync_delta, observe_local_session_catalog, send_source_publication,
    LocalAuthorityHostBackend, LocalCatalogChangeRequest, LocalSessionCatalog, LocalTargetFactory,
    RatatuiLocalAuthorityHostBackend, RatatuiLocalSessionCatalog, RatatuiLocalTargetFactory,
    SessionSyncAuthorityManager, SessionSyncMode, SourcePublicationTracker,
};
#[cfg(target_os = "linux")]
use crate::remote::node::remote_workspace_socket_registry_runtime::workspace_socket_registry_path;
use crate::remote::publication::remote_target_publication_backend::RemoteTargetPublicationBackend;
use crate::remote::publication::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::os::fd::RawFd;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BRIDGE_DISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(25);
const BRIDGE_DISCOVERY_RETRY_ATTEMPTS: u8 = 20;
// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
const REMOTE_NODE_INGRESS_OWNER_IDLE_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const OWNER_CONTROL_MAGIC: &[u8; 4] = b"waOC";
const OWNER_CONTROL_REPLY_OK: u8 = 0;
const OWNER_CONTROL_REPLY_PENDING: u8 = 1;
const OWNER_CONTROL_REPLY_ERROR: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthoritySocketReadyReply {
    status: AuthoritySocketReadyStatus,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthoritySocketReadyStatus {
    Registered,
    Pending,
    Error,
}

pub struct RemoteNodeIngressServerRuntime<
    B: RemoteTargetPublicationBackend = crate::remote::publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend,
    F: LocalTargetFactory = RatatuiLocalTargetFactory,
    A: LocalAuthorityHostBackend = RatatuiLocalAuthorityHostBackend,
    G: LocalSessionCatalog = RatatuiLocalSessionCatalog,
> {
    publication_runtime: RemoteTargetPublicationRuntime<B>,
    target_factory: F,
    authority_backend: A,
    local_session_catalog: G,
    network: RemoteNetworkConfig,
}

pub struct RemoteNodeIngressServerGuard {
    transport_guard: Option<GrpcRemoteNodeTransportGuard>,
    worker: Option<thread::JoinHandle<()>>,
    shutdown_tx: Option<mpsc::Sender<InternalEvent>>,
}

struct ActiveAuthoritySocketBridge {
    target_component: String,
    transport: Arc<RemoteAuthorityTransportRuntime>,
}

struct ActiveNodeIngressSession {
    session: RemoteNodeSessionHandle,
    bridges: HashMap<RemoteControlAddr, ActiveAuthoritySocketBridge>,
    published_fingerprints: HashMap<String, String>,
    source_publication_tracker: SourcePublicationTracker,
    observed_sessions: HashMap<String, ManagedSessionRecord>,
    published_sessions: HashMap<String, ManagedSessionRecord>,
    observed_initialized: bool,
    next_message_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PublicationRevisionKey {
    node_id: String,
    node_instance_id: String,
    target_id: String,
}

#[derive(Debug, Default)]
struct ReceiverPublicationRevisionTable {
    latest_applied: HashMap<PublicationRevisionKey, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationRevisionDecision {
    Legacy,
    Apply,
    Stale,
}

impl ReceiverPublicationRevisionTable {
    fn decision(
        &self,
        node_id: &str,
        node_instance_id: &str,
        target_id: &str,
        revision: u64,
    ) -> PublicationRevisionDecision {
        if node_instance_id.is_empty() || revision == 0 {
            return PublicationRevisionDecision::Legacy;
        }
        let key = PublicationRevisionKey {
            node_id: node_id.to_string(),
            node_instance_id: node_instance_id.to_string(),
            target_id: target_id.to_string(),
        };
        match self.latest_applied.get(&key) {
            Some(latest) if revision <= *latest => PublicationRevisionDecision::Stale,
            _ => PublicationRevisionDecision::Apply,
        }
    }

    fn mark_applied(
        &mut self,
        node_id: &str,
        node_instance_id: &str,
        target_id: &str,
        revision: u64,
    ) {
        if node_instance_id.is_empty() || revision == 0 {
            return;
        }
        let key = PublicationRevisionKey {
            node_id: node_id.to_string(),
            node_instance_id: node_instance_id.to_string(),
            target_id: target_id.to_string(),
        };
        self.latest_applied
            .entry(key)
            .and_modify(|latest| *latest = (*latest).max(revision))
            .or_insert(revision);
    }
}

pub(crate) enum InternalEvent {
    BridgeClosed {
        node_id: String,
        endpoint: RemoteControlAddr,
    },
    AuthorityCommandReceived {
        node_id: String,
        session_instance_id: String,
        endpoint: RemoteControlAddr,
        command: RemoteAuthorityCommand,
    },
    AuthorityHostOutput {
        node_id: String,
        session_instance_id: String,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    },
    LocalCreateSession {
        envelope: GrpcNodeSessionEnvelope,
        reply_tx: mpsc::Sender<GrpcNodeSessionEnvelope>,
    },
    LocalCreateSessionTimedOut {
        request_id: String,
    },
    SocketDirChanged,
    AuthoritySocketReady {
        node_id: String,
        endpoint: RemoteControlAddr,
        id_file: PathBuf,
        reply_tx: mpsc::Sender<AuthoritySocketReadyReply>,
    },
    RegisterWorkspaceSocket {
        socket_name: String,
        reply_tx: mpsc::Sender<AuthoritySocketReadyReply>,
    },
    UnregisterWorkspaceSocket {
        socket_name: String,
        reply_tx: mpsc::Sender<AuthoritySocketReadyReply>,
    },
    ShutdownOwner {
        reply_tx: mpsc::Sender<AuthoritySocketReadyReply>,
    },
    RetrySocketDiscovery {
        attempts_remaining: u8,
    },
    InitiateOutboundConnection {
        request: OutboundNodeSessionRequest,
    },
    CloseNodeIngressSession {
        node_id: String,
    },
    LocalCatalogChanged,
    Shutdown,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnerLifecycleEvent {
    WorkspaceRegistered(String),
    WorkspaceUnregistered(String),
    WorkspaceRegistryChanged(BTreeSet<String>),
    ShutdownRequested,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl<B, F, A, G> RemoteNodeIngressServerRuntime<B, F, A, G>
where
    B: RemoteTargetPublicationBackend,
    F: LocalTargetFactory,
    A: LocalAuthorityHostBackend,
    G: LocalSessionCatalog + Clone,
    LifecycleError: From<<A as LocalAuthorityHostBackend>::Error>,
{
    pub fn new_with_backends(
        network: RemoteNetworkConfig,
        publication_runtime: RemoteTargetPublicationRuntime<B>,
        target_factory: F,
        authority_backend: A,
        local_session_catalog: G,
    ) -> Self {
        RemoteNodeIngressServerRuntime {
            publication_runtime,
            target_factory,
            authority_backend,
            local_session_catalog,
            network,
        }
    }

    pub fn run_owner(&self, ready_socket: Option<&str>) -> Result<(), LifecycleError> {
        let addr = remote_node_ingress_owner_addr(&self.network);
        let startup =
            (|| -> Result<(RemoteControlListener, RemoteNodeIngressServerGuard), LifecycleError> {
                let listener =
                    RemoteControlListener::bind(&addr).map_err(remote_node_ingress_error)?;
                let (_catalog_tx, catalog_rx) = mpsc::channel::<LocalCatalogChangeRequest>();
                let guard = self.start(catalog_rx)?;
                if guard.owner_event_sender().is_none() {
                    return Err(LifecycleError::Protocol(
                        "remote node ingress owner did not expose local control channel"
                            .to_string(),
                    ));
                }
                Ok((listener, guard))
            })();
        let (listener, guard) = match startup {
            Ok(startup) => startup,
            Err(error) => {
                let _ = notify_owner_ready(ready_socket, Err(error.to_string()));
                return Err(error);
            }
        };
        let Some(owner_tx) = guard.owner_event_sender() else {
            let error = LifecycleError::Protocol(
                "remote node ingress owner did not expose local control channel".to_string(),
            );
            let _ = notify_owner_ready(ready_socket, Err(error.to_string()));
            return Err(error);
        };
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        let _workspace_registry_watcher = match start_workspace_registry_lifecycle_watcher(
            self.network.clone(),
            lifecycle_tx.clone(),
        ) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] workspace registry watcher failed: {error}"
                ));
                None
            }
        };
        let _owner_acceptor = start_owner_control_acceptor(listener, &owner_tx, lifecycle_tx);
        if let Err(error) = notify_owner_ready(ready_socket, Ok(())) {
            ERROR_LOG.log(format!(
                "[remote-node-ingress] ready notification failed: {error}"
            ));
        }
        let initial_workspace_sockets = live_workspace_sockets(&self.network)?;
        let mut live_workspace_sockets = initial_workspace_sockets.clone();
        let mut saw_workspace = !initial_workspace_sockets.is_empty();
        while let Some(event) = next_owner_lifecycle_event(&lifecycle_rx, saw_workspace) {
            if event == OwnerLifecycleEvent::ShutdownRequested {
                break;
            }
            apply_owner_lifecycle_event(&mut live_workspace_sockets, &mut saw_workspace, event);
            if saw_workspace && live_workspace_sockets.is_empty() {
                break;
            }
        }
        cleanup_remote_listener(&addr);
        Ok(())
    }

    pub fn ensure_owner_running(
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let addr = remote_node_ingress_owner_addr(network);
        if remote_node_ingress_owner_available(&addr) {
            register_workspace_socket_with_owner(network, socket_name)?;
            return Ok(());
        }
        let lock_path = remote_node_ingress_startup_lock_path(network);
        let Some(_startup_lock) =
            StartupLock::try_acquire(&lock_path).map_err(remote_node_ingress_error)?
        else {
            let _startup_lock =
                StartupLock::acquire(&lock_path).map_err(remote_node_ingress_error)?;
            if remote_node_ingress_owner_available(&addr) {
                register_workspace_socket_with_owner(network, socket_name)?;
                return Ok(());
            }
            return Err(LifecycleError::Protocol(format!(
                "remote node ingress owner for listener `{}` was not ready after startup lock {} released",
                network.listener_addr(),
                lock_path.display()
            )));
        };
        if remote_node_ingress_owner_available(&addr) {
            register_workspace_socket_with_owner(network, socket_name)?;
            return Ok(());
        }
        let current_executable = current_waitagent_executable()?;
        let ready_addr = remote_ready_addr();
        let ready_listener =
            RemoteControlListener::bind(&ready_addr).map_err(remote_node_ingress_error)?;
        // On Windows the listener resolves the ephemeral port; the child must
        // be handed the concrete address, not the pre-bind `127.0.0.1:0`.
        let bound_ready_addr = ready_listener.local_addr().clone();
        let child = spawn_waitagent_sidecar_child(
            &current_executable,
            remote_node_ingress_owner_args(network, Some(&bound_ready_addr)),
        )
        .map_err(remote_node_ingress_error)?;
        let ready = wait_for_owner_ready(ready_listener, &bound_ready_addr, child);
        cleanup_remote_listener(&bound_ready_addr);
        ready?;
        register_workspace_socket_with_owner(network, socket_name)
    }

    pub fn unregister_owner_workspace_socket(
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        if socket_name == "__shared__" || socket_name.is_empty() {
            return Ok(());
        }
        let addr = remote_node_ingress_owner_addr(network);
        if !remote_node_ingress_owner_available(&addr) {
            return Ok(());
        }
        unregister_workspace_socket_with_owner(network, socket_name)
    }

    #[cfg(test)]
    pub fn shutdown_owner(network: &RemoteNetworkConfig) -> Result<(), LifecycleError> {
        let addr = remote_node_ingress_owner_addr(network);
        if !remote_node_ingress_owner_available(&addr) {
            return Ok(());
        }
        shutdown_owner_with_control_socket(network)
    }

    pub fn start(
        &self,
        local_catalog_rx: mpsc::Receiver<LocalCatalogChangeRequest>,
    ) -> Result<RemoteNodeIngressServerGuard, LifecycleError> {
        let transport = match (
            self.network.node_cert_path.as_ref(),
            self.network.node_key_path.as_ref(),
        ) {
            (Some(cert), Some(key)) => GrpcRemoteNodeTransport::with_tls(cert, key),
            _ => GrpcRemoteNodeTransport::new(),
        };
        let (transport_tx, transport_rx) = mpsc::channel();
        let (internal_tx, internal_rx) = mpsc::channel();
        let transport_guard = transport
            .listen_inbound(self.network.listener_addr(), transport_tx.clone())
            .map_err(remote_node_ingress_error)?;
        let publication_runtime = self.publication_runtime.clone();
        let target_factory = self.target_factory.clone();
        let authority_backend = self.authority_backend.clone();
        let local_session_catalog = self.local_session_catalog.clone();
        let network = self.network.clone();
        let shutdown_tx = internal_tx.clone();
        let worker = thread::spawn(move || {
            run_node_ingress_server_loop(
                publication_runtime,
                target_factory,
                authority_backend,
                local_session_catalog,
                network,
                RunNodeIngressServerLoopArgs {
                    transport_rx,
                    transport_tx,
                    internal_rx,
                    internal_tx,
                    local_catalog_rx,
                    start_authority_socket_watcher: true,
                },
            );
        });
        Ok(RemoteNodeIngressServerGuard {
            transport_guard: Some(transport_guard),
            worker: Some(worker),
            shutdown_tx: Some(shutdown_tx),
        })
    }
}

impl<B, F, A, G> crate::ports::session_creation::SessionCreationPort
    for RemoteNodeIngressServerRuntime<B, F, A, G>
where
    B: RemoteTargetPublicationBackend,
    F: LocalTargetFactory + Sync,
    A: LocalAuthorityHostBackend + Sync,
    G: LocalSessionCatalog + Clone + Sync,
    LifecycleError: From<<A as LocalAuthorityHostBackend>::Error>,
{
    fn create_session(
        &self,
        request: crate::ports::session_creation::RemoteSessionCreationRequest,
    ) -> Result<
        crate::domain::session_catalog::ManagedSessionRecord,
        crate::ports::session_creation::RemoteSessionCreationError,
    > {
        use crate::infra::remote_protocol::{
            ControlPlanePayload, CreateSessionRequestPayload, NodeSessionChannel,
            NodeSessionEnvelope, ProtocolEnvelope, REMOTE_PROTOCOL_VERSION,
        };
        use crate::infra::remote_transport_codec::{
            read_node_session_envelope, write_node_session_envelope,
        };
        use crate::ports::session_creation::RemoteSessionCreationError;
        use std::time::Duration;

        const DEFAULT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

        Self::ensure_owner_running("__shared__", &self.network)
            .map_err(|error| RemoteSessionCreationError::Transport(error.to_string()))?;
        let mut stream =
            RemoteControlStream::connect(&remote_node_ingress_owner_addr(&self.network))
                .map_err(|error| RemoteSessionCreationError::Transport(error.to_string()))?;
        stream
            .set_read_timeout(Some(DEFAULT_ACCEPT_TIMEOUT))
            .map_err(|error| RemoteSessionCreationError::Transport(error.to_string()))?;

        let request_id = format!(
            "local-create-session-{}-{}",
            std::process::id(),
            now_millis()
        );
        let payload = CreateSessionRequestPayload {
            request_id: request_id.clone(),
            authority_node_id: request.authority_node_id.clone(),
            cwd_hint: request
                .cwd_hint
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            cols: request.cols,
            rows: request.rows,
        };
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: request_id.clone(),
            message_type: "create_session_request",
            timestamp: format!("{}Z", now_millis()),
            sender_id: "waitagent-local-create-session".to_string(),
            correlation_id: Some(request_id.clone()),
            session_id: None,
            target_id: None,
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::CreateSessionRequest(payload),
        };
        write_node_session_envelope(
            &mut stream,
            &NodeSessionEnvelope {
                channel: NodeSessionChannel::Authority,
                envelope,
            },
        )
        .map_err(|error| RemoteSessionCreationError::Transport(error.to_string()))?;

        let reply = read_node_session_envelope(&mut stream)
            .map_err(|error| RemoteSessionCreationError::Transport(error.to_string()))?;
        match reply.envelope.payload {
            ControlPlanePayload::CreateSessionAccepted(accepted) => {
                Ok(accepted_target_record(&request, &accepted))
            }
            ControlPlanePayload::CreateSessionRejected(rejected) => {
                Err(RemoteSessionCreationError::Rejected {
                    code: rejected.code,
                    message: rejected.message,
                })
            }
            other => Err(RemoteSessionCreationError::Protocol(format!(
                "unexpected create-session reply payload `{}`",
                other.message_type()
            ))),
        }
    }
}

fn accepted_target_record(
    request: &crate::ports::session_creation::RemoteSessionCreationRequest,
    accepted: &crate::infra::remote_protocol::CreateSessionAcceptedPayload,
) -> crate::domain::session_catalog::ManagedSessionRecord {
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;

    ManagedSessionRecord {
        address: ManagedSessionAddress::remote_peer(
            request.authority_node_id.clone(),
            accepted.session_id.clone(),
        ),
        selector: Some(format!(
            "{}:{}",
            request.authority_node_id, accepted.session_id
        )),
        availability: SessionAvailability::Online,
        workspace_dir: request.cwd_hint.clone(),
        workspace_key: Some(accepted.session_id.clone()),
        session_role: Some(WorkspaceSessionRole::TargetHost),
        opened_by: Vec::new(),
        attached_clients: 0,
        window_count: 1,
        command_name: Some("bash".to_string()),
        display_command_name: None,
        agent_command_name: None,
        current_path: request.cwd_hint.clone(),
        task_state: ManagedSessionTaskState::Input,
    }
}

pub(crate) fn notify_authority_socket_ready(
    network: &RemoteNetworkConfig,
    node_id: &str,
    addr: &RemoteControlAddr,
    marker: &Path,
) -> io::Result<()> {
    RemoteNodeIngressServerRuntime::<
        crate::remote::publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend,
        RatatuiLocalTargetFactory,
        RatatuiLocalAuthorityHostBackend,
    >::ensure_owner_running("__shared__", network)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
    let mut stream = RemoteControlStream::connect(&remote_node_ingress_owner_addr(network))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let id_string = authority_endpoint_id_string(addr, marker);
    write_owner_control_authority_socket_ready(&mut stream, node_id, &id_string)?;
    match read_owner_control_reply(&mut stream)? {
        AuthoritySocketReadyReply {
            status: AuthoritySocketReadyStatus::Registered,
            ..
        } => Ok(()),
        AuthoritySocketReadyReply {
            status: AuthoritySocketReadyStatus::Pending,
            message,
        } => Err(io::Error::new(io::ErrorKind::WouldBlock, message)),
        AuthoritySocketReadyReply {
            status: AuthoritySocketReadyStatus::Error,
            message,
        } => Err(io::Error::new(io::ErrorKind::Other, message)),
    }
}

fn register_workspace_socket_with_owner(
    network: &RemoteNetworkConfig,
    socket_name: &str,
) -> Result<(), LifecycleError> {
    if socket_name == "__shared__" || socket_name.is_empty() {
        return Ok(());
    }
    let mut stream = RemoteControlStream::connect(&remote_node_ingress_owner_addr(network))
        .map_err(remote_node_ingress_error)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(remote_node_ingress_error)?;
    write_owner_control_register_workspace_socket(&mut stream, socket_name)
        .map_err(remote_node_ingress_error)?;
    match read_owner_control_reply(&mut stream).map_err(remote_node_ingress_error)? {
        AuthoritySocketReadyReply {
            status: AuthoritySocketReadyStatus::Registered,
            ..
        } => Ok(()),
        AuthoritySocketReadyReply { message, .. } => Err(LifecycleError::Protocol(format!(
            "remote node ingress owner rejected workspace socket registration `{socket_name}`: {message}"
        ))),
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn unregister_workspace_socket_with_owner(
    network: &RemoteNetworkConfig,
    socket_name: &str,
) -> Result<(), LifecycleError> {
    let mut stream = RemoteControlStream::connect(&remote_node_ingress_owner_addr(network))
        .map_err(remote_node_ingress_error)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(remote_node_ingress_error)?;
    write_owner_control_unregister_workspace_socket(&mut stream, socket_name)
        .map_err(remote_node_ingress_error)?;
    match read_owner_control_reply(&mut stream).map_err(remote_node_ingress_error)? {
        AuthoritySocketReadyReply {
            status: AuthoritySocketReadyStatus::Registered,
            ..
        } => Ok(()),
        AuthoritySocketReadyReply { message, .. } => Err(LifecycleError::Protocol(format!(
            "remote node ingress owner rejected workspace socket unregistration `{socket_name}`: {message}"
        ))),
    }
}

#[cfg(test)]
fn shutdown_owner_with_control_socket(network: &RemoteNetworkConfig) -> Result<(), LifecycleError> {
    let mut stream = RemoteControlStream::connect(&remote_node_ingress_owner_addr(network))
        .map_err(remote_node_ingress_error)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(remote_node_ingress_error)?;
    write_owner_control_shutdown(&mut stream).map_err(remote_node_ingress_error)?;
    match read_owner_control_reply(&mut stream).map_err(remote_node_ingress_error)? {
        AuthoritySocketReadyReply {
            status: AuthoritySocketReadyStatus::Registered,
            ..
        } => Ok(()),
        AuthoritySocketReadyReply { message, .. } => Err(LifecycleError::Protocol(format!(
            "remote node ingress owner rejected shutdown: {message}"
        ))),
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn notify_owner_ready(ready_socket: Option<&str>, result: Result<(), String>) -> io::Result<()> {
    let Some(ready_socket) = ready_socket else {
        return Ok(());
    };
    let addr = RemoteControlAddr::from_arg_string(ready_socket)?;
    let mut stream = RemoteControlStream::connect(&addr)?;
    match result {
        Ok(()) => stream.write_all(b"ok\n")?,
        Err(error) => {
            stream.write_all(b"err\t")?;
            stream.write_all(error.as_bytes())?;
            stream.write_all(b"\n")?;
        }
    }
    stream.flush()
}

fn wait_for_owner_ready(
    listener: RemoteControlListener,
    ready_addr: &RemoteControlAddr,
    mut child: std::process::Child,
) -> Result<(), LifecycleError> {
    enum OwnerReadyEvent {
        Ready(io::Result<String>),
        Exited(io::Result<std::process::ExitStatus>),
    }

    let (event_tx, event_rx) = mpsc::channel();
    let ready_tx = event_tx.clone();
    thread::spawn(move || {
        let response = listener.accept().and_then(|(mut stream, _)| {
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        let _ = ready_tx.send(OwnerReadyEvent::Ready(response));
    });

    thread::spawn(move || {
        let status = child.wait();
        let _ = event_tx.send(OwnerReadyEvent::Exited(status));
    });

    match event_rx.recv() {
        Ok(OwnerReadyEvent::Ready(Ok(response))) => {
            let response = response.trim();
            if response == "ok" {
                return Ok(());
            }
            if let Some(error) = response.strip_prefix("err\t") {
                return Err(LifecycleError::Protocol(format!(
                    "remote node ingress owner failed to start: {error}"
                )));
            }
            Err(LifecycleError::Protocol(format!(
                "remote node ingress owner sent invalid ready response `{response}`"
            )))
        }
        Ok(OwnerReadyEvent::Ready(Err(error))) => Err(remote_node_ingress_error(error)),
        Ok(OwnerReadyEvent::Exited(Ok(status))) => Err(LifecycleError::Protocol(format!(
            "remote node ingress owner exited before reporting ready: {status}"
        ))),
        Ok(OwnerReadyEvent::Exited(Err(error))) => Err(remote_node_ingress_error(error)),
        Err(_) => Err(LifecycleError::Protocol(format!(
            "remote node ingress owner ready socket `{}` closed before reporting ready",
            ready_addr.to_arg_string()
        ))),
    }
}

fn remote_node_ingress_owner_args(
    network: &RemoteNetworkConfig,
    ready_socket: Option<&RemoteControlAddr>,
) -> Vec<String> {
    let mut args = vec![
        "__remote-node-ingress-server".to_string(),
        "--socket-name".to_string(),
        "__shared__".to_string(),
    ];
    if let Some(ready_socket) = ready_socket {
        args.push("--ready-socket".to_string());
        args.push(ready_socket.to_arg_string());
    }
    prepend_global_network_args(args, network)
}

fn remote_node_ingress_owner_available(addr: &RemoteControlAddr) -> bool {
    remote_listener_is_running(addr)
}

/// Accept loop for owner control listeners.
///
/// Implemented by the cross-platform [`RemoteControlListener`] and, for the
/// single-process ratatui runtime that still binds a raw `UnixListener`, by
/// `UnixListener` itself.
pub(crate) trait OwnerControlAccept {
    type Stream: Read + Write + Send + 'static;

    fn accept_control(&self) -> io::Result<Self::Stream>;
}

impl OwnerControlAccept for RemoteControlListener {
    type Stream = RemoteControlStream;

    fn accept_control(&self) -> io::Result<Self::Stream> {
        self.accept().map(|(stream, _)| stream)
    }
}

#[cfg(unix)]
impl OwnerControlAccept for UnixListener {
    type Stream = UnixStream;

    fn accept_control(&self) -> io::Result<Self::Stream> {
        self.accept().map(|(stream, _)| stream)
    }
}

pub(crate) fn start_owner_control_acceptor<L>(
    listener: L,
    owner_tx: &mpsc::Sender<InternalEvent>,
    lifecycle_tx: mpsc::Sender<OwnerLifecycleEvent>,
) -> thread::JoinHandle<()>
where
    L: OwnerControlAccept + Send + 'static,
{
    let owner_tx = owner_tx.clone();
    thread::spawn(move || {
        while let Ok(stream) = listener.accept_control() {
            handle_owner_stream(stream, owner_tx.clone(), lifecycle_tx.clone());
        }
    })
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn next_owner_lifecycle_event(
    lifecycle_rx: &mpsc::Receiver<OwnerLifecycleEvent>,
    saw_workspace: bool,
) -> Option<OwnerLifecycleEvent> {
    if saw_workspace {
        lifecycle_rx.recv().ok()
    } else {
        lifecycle_rx
            .recv_timeout(REMOTE_NODE_INGRESS_OWNER_IDLE_STARTUP_TIMEOUT)
            .ok()
    }
}

fn handle_owner_stream<S>(
    mut stream: S,
    owner_tx: mpsc::Sender<InternalEvent>,
    lifecycle_tx: mpsc::Sender<OwnerLifecycleEvent>,
) where
    S: Read + Write + Send + 'static,
{
    thread::spawn(move || {
        let mut prefix = [0_u8; 4];
        if stream.read_exact(&mut prefix).is_err() {
            return;
        }
        let request = if &prefix == OWNER_CONTROL_MAGIC {
            match read_owner_control_message(&mut stream) {
                Ok(OwnerControlMessage::AuthoritySocketReady {
                    node_id,
                    endpoint,
                    id_file,
                }) => {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    if owner_tx
                        .send(InternalEvent::AuthoritySocketReady {
                            node_id,
                            endpoint,
                            id_file,
                            reply_tx,
                        })
                        .is_err()
                    {
                        let _ = write_owner_control_reply(
                            &mut stream,
                            &AuthoritySocketReadyReply {
                                status: AuthoritySocketReadyStatus::Error,
                                message: "remote-node-ingress owner loop is closed".to_string(),
                            },
                        );
                        return;
                    }
                    let reply = reply_rx
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap_or_else(|_| AuthoritySocketReadyReply {
                            status: AuthoritySocketReadyStatus::Pending,
                            message: "authority socket registration is pending".to_string(),
                        });
                    let _ = write_owner_control_reply(&mut stream, &reply);
                }
                Ok(OwnerControlMessage::RegisterWorkspaceSocket { socket_name }) => {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    let registered_socket_name = socket_name.clone();
                    if owner_tx
                        .send(InternalEvent::RegisterWorkspaceSocket {
                            socket_name,
                            reply_tx,
                        })
                        .is_err()
                    {
                        let _ = write_owner_control_reply(
                            &mut stream,
                            &AuthoritySocketReadyReply {
                                status: AuthoritySocketReadyStatus::Error,
                                message: "remote-node-ingress owner loop is closed".to_string(),
                            },
                        );
                        return;
                    }
                    let reply = reply_rx
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap_or_else(|_| AuthoritySocketReadyReply {
                            status: AuthoritySocketReadyStatus::Pending,
                            message: "workspace socket registration is pending".to_string(),
                        });
                    if reply.status == AuthoritySocketReadyStatus::Registered {
                        let _ = lifecycle_tx.send(OwnerLifecycleEvent::WorkspaceRegistered(
                            registered_socket_name,
                        ));
                    }
                    let _ = write_owner_control_reply(&mut stream, &reply);
                }
                Ok(OwnerControlMessage::UnregisterWorkspaceSocket { socket_name }) => {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    let unregistered_socket_name = socket_name.clone();
                    if owner_tx
                        .send(InternalEvent::UnregisterWorkspaceSocket {
                            socket_name,
                            reply_tx,
                        })
                        .is_err()
                    {
                        let _ = write_owner_control_reply(
                            &mut stream,
                            &AuthoritySocketReadyReply {
                                status: AuthoritySocketReadyStatus::Error,
                                message: "remote-node-ingress owner loop is closed".to_string(),
                            },
                        );
                        return;
                    }
                    let reply = reply_rx
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap_or_else(|_| AuthoritySocketReadyReply {
                            status: AuthoritySocketReadyStatus::Pending,
                            message: "workspace socket unregistration is pending".to_string(),
                        });
                    if reply.status == AuthoritySocketReadyStatus::Registered {
                        let _ = lifecycle_tx.send(OwnerLifecycleEvent::WorkspaceUnregistered(
                            unregistered_socket_name,
                        ));
                    }
                    let _ = write_owner_control_reply(&mut stream, &reply);
                }
                Ok(OwnerControlMessage::Shutdown) => {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    if owner_tx
                        .send(InternalEvent::ShutdownOwner { reply_tx })
                        .is_err()
                    {
                        let _ = write_owner_control_reply(
                            &mut stream,
                            &AuthoritySocketReadyReply {
                                status: AuthoritySocketReadyStatus::Error,
                                message: "remote-node-ingress owner loop is closed".to_string(),
                            },
                        );
                        return;
                    }
                    let reply = reply_rx
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap_or_else(|_| AuthoritySocketReadyReply {
                            status: AuthoritySocketReadyStatus::Pending,
                            message: "remote-node-ingress shutdown is pending".to_string(),
                        });
                    if reply.status == AuthoritySocketReadyStatus::Registered {
                        let _ = lifecycle_tx.send(OwnerLifecycleEvent::ShutdownRequested);
                    }
                    let _ = write_owner_control_reply(&mut stream, &reply);
                }
                Err(_) => {}
            }
            return;
        } else {
            let mut request_reader = Cursor::new(prefix).chain(&mut stream);
            let Ok(request) = read_node_session_envelope(&mut request_reader) else {
                return;
            };
            request
        };
        let Some(Body::CreateSessionRequest(payload)) =
            map_outbound_grpc_envelope_for_local_request(request).body
        else {
            return;
        };
        let authority_node_id = payload.authority_node_id.clone();
        let request_id = payload.request_id.clone();

        let grpc = local_create_session_request_grpc_envelope(authority_node_id, payload);
        let (reply_tx, reply_rx) = mpsc::channel();
        if owner_tx
            .send(InternalEvent::LocalCreateSession {
                envelope: grpc,
                reply_tx,
            })
            .is_err()
        {
            let _ = write_create_session_rejected_to_stream(
                &mut stream,
                request_id,
                "remote node ingress owner is not running",
            );
            return;
        }
        let wait_started = std::time::Instant::now();
        match reply_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(reply) => {
                if let Some(envelope) = map_local_reply_from_grpc(reply) {
                    let _ = write_node_session_envelope(&mut stream, &envelope);
                }
            }
            Err(_) => {
                ERROR_LOG.log_error(format!(
                    "owner timed out waiting create-session reply id={request_id} after {:?}",
                    wait_started.elapsed()
                ));
                let _ = owner_tx.send(InternalEvent::LocalCreateSessionTimedOut {
                    request_id: request_id.clone(),
                });
                let _ = write_create_session_rejected_to_stream(
                    &mut stream,
                    request_id,
                    "timed out waiting for create-session reply from remote node",
                );
            }
        }
    });
}

enum OwnerControlMessage {
    AuthoritySocketReady {
        node_id: String,
        endpoint: RemoteControlAddr,
        id_file: PathBuf,
    },
    RegisterWorkspaceSocket {
        socket_name: String,
    },
    UnregisterWorkspaceSocket {
        socket_name: String,
    },
    Shutdown,
}
fn write_owner_control_authority_socket_ready(
    writer: &mut impl Write,
    node_id: &str,
    id_string: &str,
) -> io::Result<()> {
    writer.write_all(OWNER_CONTROL_MAGIC)?;
    writer.write_all(&[1])?;
    write_owner_control_string(writer, node_id)?;
    write_owner_control_string(writer, id_string)?;
    writer.flush()
}

fn write_owner_control_register_workspace_socket(
    writer: &mut impl Write,
    socket_name: &str,
) -> io::Result<()> {
    writer.write_all(OWNER_CONTROL_MAGIC)?;
    writer.write_all(&[2])?;
    write_owner_control_string(writer, socket_name)?;
    writer.flush()
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn write_owner_control_unregister_workspace_socket(
    writer: &mut impl Write,
    socket_name: &str,
) -> io::Result<()> {
    writer.write_all(OWNER_CONTROL_MAGIC)?;
    writer.write_all(&[3])?;
    write_owner_control_string(writer, socket_name)?;
    writer.flush()
}

#[cfg(test)]
fn write_owner_control_shutdown(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(OWNER_CONTROL_MAGIC)?;
    writer.write_all(&[4])?;
    writer.flush()
}

fn write_owner_control_reply(
    writer: &mut impl Write,
    reply: &AuthoritySocketReadyReply,
) -> io::Result<()> {
    let status = match reply.status {
        AuthoritySocketReadyStatus::Registered => OWNER_CONTROL_REPLY_OK,
        AuthoritySocketReadyStatus::Pending => OWNER_CONTROL_REPLY_PENDING,
        AuthoritySocketReadyStatus::Error => OWNER_CONTROL_REPLY_ERROR,
    };
    writer.write_all(&[status])?;
    write_owner_control_string(writer, &reply.message)?;
    writer.flush()
}

fn read_owner_control_reply(reader: &mut impl Read) -> io::Result<AuthoritySocketReadyReply> {
    let mut status = [0_u8; 1];
    reader.read_exact(&mut status)?;
    let message = read_owner_control_string(reader)?;
    let status = match status[0] {
        OWNER_CONTROL_REPLY_OK => AuthoritySocketReadyStatus::Registered,
        OWNER_CONTROL_REPLY_PENDING => AuthoritySocketReadyStatus::Pending,
        OWNER_CONTROL_REPLY_ERROR => AuthoritySocketReadyStatus::Error,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown authority socket ready reply status",
            ))
        }
    };
    Ok(AuthoritySocketReadyReply { status, message })
}

fn read_owner_control_message(reader: &mut impl Read) -> io::Result<OwnerControlMessage> {
    let mut tag = [0_u8; 1];
    reader.read_exact(&mut tag)?;
    match tag[0] {
        1 => {
            let node_id = read_owner_control_string(reader)?;
            let id_string = read_owner_control_string(reader)?;
            let (endpoint, id_file) = authority_endpoint_from_id_string(&id_string)?;
            Ok(OwnerControlMessage::AuthoritySocketReady {
                node_id,
                endpoint,
                id_file,
            })
        }
        2 => {
            let socket_name = read_owner_control_string(reader)?;
            Ok(OwnerControlMessage::RegisterWorkspaceSocket { socket_name })
        }
        3 => {
            let socket_name = read_owner_control_string(reader)?;
            Ok(OwnerControlMessage::UnregisterWorkspaceSocket { socket_name })
        }
        4 => Ok(OwnerControlMessage::Shutdown),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown remote-node-ingress owner control message",
        )),
    }
}

fn write_owner_control_string(writer: &mut impl Write, value: &str) -> io::Result<()> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "owner control string too long")
    })?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(bytes)
}

fn read_owner_control_string(reader: &mut impl Read) -> io::Result<String> {
    let mut len_bytes = [0_u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "owner control string is not valid UTF-8",
        )
    })
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn live_workspace_sockets(
    _network: &RemoteNetworkConfig,
) -> Result<BTreeSet<String>, LifecycleError> {
    // Preserved for ratatui remote path; tmux dependency to be removed in a later phase.
    // In the ratatui-only build there are no tmux workspace sockets to enumerate.
    Ok(BTreeSet::new())
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn apply_owner_lifecycle_event(
    live_workspace_sockets: &mut BTreeSet<String>,
    saw_workspace: &mut bool,
    event: OwnerLifecycleEvent,
) {
    match event {
        OwnerLifecycleEvent::WorkspaceRegistered(socket_name) => {
            if socket_name != "__shared__" && !socket_name.is_empty() {
                live_workspace_sockets.insert(socket_name);
                *saw_workspace = true;
            }
        }
        OwnerLifecycleEvent::WorkspaceUnregistered(socket_name) => {
            live_workspace_sockets.remove(&socket_name);
        }
        OwnerLifecycleEvent::WorkspaceRegistryChanged(sockets) => {
            if !sockets.is_empty() {
                *saw_workspace = true;
            }
            *live_workspace_sockets = sockets;
        }
        OwnerLifecycleEvent::ShutdownRequested => {}
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn start_workspace_registry_lifecycle_watcher(
    network: RemoteNetworkConfig,
    lifecycle_tx: mpsc::Sender<OwnerLifecycleEvent>,
) -> io::Result<thread::JoinHandle<()>> {
    #[cfg(target_os = "linux")]
    {
        start_workspace_registry_inotify_watcher(network, lifecycle_tx)
    }

    #[cfg(not(target_os = "linux"))]
    {
        start_workspace_registry_polling_watcher(network, lifecycle_tx)
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
#[cfg(target_os = "linux")]
fn start_workspace_registry_inotify_watcher(
    network: RemoteNetworkConfig,
    lifecycle_tx: mpsc::Sender<OwnerLifecycleEvent>,
) -> io::Result<thread::JoinHandle<()>> {
    let registry_path = workspace_socket_registry_path(&network);
    let registry_dir = registry_path.parent().unwrap_or_else(|| Path::new("/tmp"));
    fs::create_dir_all(registry_dir)?;
    let watched_name = registry_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_string();
    let path = registry_dir.to_string_lossy().into_owned();
    // SAFETY: `inotify_init1` creates a fresh kernel object; the returned fd is
    // owned by this function and passed to the worker thread.
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let c_path = std::ffi::CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid workspace socket dir"))?;
    // SAFETY: `c_path` is a valid nul-terminated string and `fd` is a valid
    // inotify instance returned above.
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            c_path.as_ptr(),
            libc::IN_CREATE | libc::IN_CLOSE_WRITE | libc::IN_DELETE | libc::IN_MOVED_TO,
        )
    };
    if wd < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: `fd` was successfully created above; close it before returning.
        unsafe { libc::close(fd) };
        return Err(error);
    }

    Ok(thread::spawn(move || {
        let event_size = std::mem::size_of::<libc::inotify_event>();
        let mut buf = [0u8; 4096];

        loop {
            // SAFETY: `fd` is a valid inotify instance and `buf` is a live,
            // mutable buffer of its full length.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }

            let mut off = 0;
            while off + event_size <= n as usize {
                // SAFETY: kernel wrote a complete inotify_event into the buffer.
                let event = unsafe { &*(buf[off..].as_ptr() as *const libc::inotify_event) };
                let name_len = event.len as usize;
                let name_off = off + event_size;
                if name_len > 0 && name_off + name_len <= n as usize {
                    let end = buf[name_off..name_off + name_len]
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(name_len);
                    if let Ok(name) = std::str::from_utf8(&buf[name_off..name_off + end]) {
                        if name == watched_name {
                            let sockets = live_workspace_sockets(&network).unwrap_or_default();
                            if lifecycle_tx
                                .send(OwnerLifecycleEvent::WorkspaceRegistryChanged(sockets))
                                .is_err()
                            {
                                // SAFETY: `fd` is a valid inotify instance owned by this thread.
                                unsafe { libc::close(fd) };
                                return;
                            }
                        }
                    }
                }
                off += event_size + name_len;
            }
        }

        // SAFETY: `fd` is a valid inotify instance owned by this thread; close
        // it when the watcher exits.
        unsafe { libc::close(fd) };
    }))
}

#[cfg(not(target_os = "linux"))]
fn start_workspace_registry_polling_watcher(
    network: RemoteNetworkConfig,
    lifecycle_tx: mpsc::Sender<OwnerLifecycleEvent>,
) -> io::Result<thread::JoinHandle<()>> {
    Ok(thread::spawn(move || {
        let mut previous = live_workspace_sockets(&network).unwrap_or_default();
        loop {
            thread::sleep(Duration::from_millis(50));
            let current = live_workspace_sockets(&network).unwrap_or_default();
            if current == previous {
                continue;
            }
            previous = current.clone();
            if lifecycle_tx
                .send(OwnerLifecycleEvent::WorkspaceRegistryChanged(current))
                .is_err()
            {
                return;
            }
        }
    }))
}

/// Handle returned by `start_socket_watcher`. The caller wakes the watcher by
/// writing to a dedicated shutdown pipe so the blocking `poll` returns and the
/// thread can exit cleanly.
struct SocketWatcherHandle {
    inotify_fd: RawFd,
    shutdown_write: RawFd,
    worker: thread::JoinHandle<()>,
}

/// Watches the temp directory for new authority socket files and sends
/// [`InternalEvent::SocketDirChanged`] through the channel when one appears.
///
/// Linux production uses a blocking inotify fd so bridge discovery is driven by
/// kernel filesystem events, not periodic refresh scans.
fn start_socket_watcher(
    internal_tx: mpsc::Sender<InternalEvent>,
) -> io::Result<SocketWatcherHandle> {
    #[cfg(target_os = "linux")]
    {
        start_inotify_watcher(internal_tx)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = internal_tx;
        Err(io::Error::other(
            "remote node ingress server requires Linux inotify for event-driven authority discovery",
        ))
    }
}

/// Linux inotify-based watcher.
#[cfg(target_os = "linux")]
fn start_inotify_watcher(
    internal_tx: mpsc::Sender<InternalEvent>,
) -> io::Result<SocketWatcherHandle> {
    // SAFETY: `inotify_init1` creates a fresh kernel object; the returned fd is
    // owned by this function and stored in the handle returned to the caller.
    let inotify_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if inotify_fd == -1 {
        return Err(io::Error::last_os_error());
    }

    let dir = std::env::temp_dir();
    let dir_path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::other("temp_dir contains interior null byte"))?;

    // SAFETY: `dir_path` is a valid nul-terminated string and `inotify_fd` is a
    // valid inotify instance returned above.
    let wd = unsafe {
        libc::inotify_add_watch(
            inotify_fd,
            dir_path.as_ptr(),
            libc::IN_CREATE | libc::IN_MOVED_TO,
        )
    };
    if wd == -1 {
        // SAFETY: `inotify_fd` was successfully created above; close it before returning.
        unsafe { libc::close(inotify_fd) };
        return Err(io::Error::last_os_error());
    }

    let mut pipe_fds = [-1; 2];
    // SAFETY: `pipe_fds` is a two-element array of valid `RawFd`s; `pipe` fills
    // both elements on success.
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } == -1 {
        // SAFETY: `inotify_fd` was successfully created above.
        unsafe { libc::close(inotify_fd) };
        return Err(io::Error::last_os_error());
    }
    let shutdown_read = pipe_fds[0];
    let shutdown_write = pipe_fds[1];

    // Mark the write end close-on-exec so it is not inherited by sidecars.
    // SAFETY: `shutdown_write` is a valid pipe fd returned by `pipe` above.
    unsafe {
        let flags = libc::fcntl(shutdown_write, libc::F_GETFD, 0);
        if flags >= 0 {
            libc::fcntl(shutdown_write, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }

    let worker = thread::spawn(move || {
        let event_size = std::mem::size_of::<libc::inotify_event>();
        let mut buf = [0u8; 4096];

        loop {
            let mut fds = [
                libc::pollfd {
                    fd: inotify_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: shutdown_read,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: `fds` is a two-element array of valid `pollfd`s that
            // remain borrowed for the duration of the call.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
            if ready <= 0 {
                break;
            }

            if fds[1].revents != 0 {
                // Shutdown requested: drain the pipe and exit.
                let mut drain = [0u8; 16];
                // SAFETY: `shutdown_read` is a valid pipe fd and `drain` is a
                // live, mutable buffer.
                unsafe {
                    libc::read(
                        shutdown_read,
                        drain.as_mut_ptr() as *mut libc::c_void,
                        drain.len(),
                    )
                };
                break;
            }

            if fds[0].revents == 0 {
                continue;
            }

            // SAFETY: `inotify_fd` is a valid inotify instance and `buf` is a
            // live, mutable buffer of its full length.
            let n =
                unsafe { libc::read(inotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }

            let mut off = 0;
            while off + event_size <= n as usize {
                // SAFETY: kernel wrote valid inotify_event into the buffer
                let event = unsafe { &*(buf[off..].as_ptr() as *const libc::inotify_event) };
                let name_len = event.len as usize;
                let name_off = off + event_size;
                if name_len > 0 && name_off + name_len <= n as usize {
                    let end = buf[name_off..name_off + name_len]
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(name_len);
                    if let Ok(name) = std::str::from_utf8(&buf[name_off..name_off + end]) {
                        if name.starts_with("waitagent-remote-") && name.ends_with(".sock") {
                            let _ = internal_tx.send(InternalEvent::SocketDirChanged);
                        }
                    }
                }
                off += event_size + name_len;
            }
        }

        // SAFETY: `inotify_fd` and `shutdown_read` are valid fds owned by this
        // worker thread; close them when the watcher exits.
        unsafe {
            libc::close(inotify_fd);
            libc::close(shutdown_read);
        }
    });

    Ok(SocketWatcherHandle {
        inotify_fd,
        shutdown_write,
        worker,
    })
}

impl RemoteNodeIngressServerGuard {
    pub fn owner_event_sender(&self) -> Option<mpsc::Sender<InternalEvent>> {
        self.shutdown_tx.clone()
    }
}

impl Drop for RemoteNodeIngressServerGuard {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(InternalEvent::Shutdown);
        }
        let _ = self.transport_guard.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum IngressServerEvent {
    Transport(RemoteNodeTransportEvent),
    Internal(InternalEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngressEventPriority {
    High,
    Low,
}

struct RunNodeIngressServerLoopArgs {
    transport_rx: mpsc::Receiver<RemoteNodeTransportEvent>,
    transport_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    internal_rx: mpsc::Receiver<InternalEvent>,
    internal_tx: mpsc::Sender<InternalEvent>,
    local_catalog_rx: mpsc::Receiver<LocalCatalogChangeRequest>,
    start_authority_socket_watcher: bool,
}

fn run_node_ingress_server_loop<
    B: RemoteTargetPublicationBackend,
    F: LocalTargetFactory,
    A: LocalAuthorityHostBackend,
    G: LocalSessionCatalog,
>(
    publication_runtime: RemoteTargetPublicationRuntime<B>,
    target_factory: F,
    authority_backend: A,
    local_session_catalog: G,
    network: RemoteNetworkConfig,
    args: RunNodeIngressServerLoopArgs,
) where
    LifecycleError: From<<A as LocalAuthorityHostBackend>::Error>,
{
    let RunNodeIngressServerLoopArgs {
        transport_rx,
        transport_tx,
        internal_rx,
        internal_tx,
        local_catalog_rx,
        start_authority_socket_watcher,
    } = args;
    let mut sessions = HashMap::<String, ActiveNodeIngressSession>::new();
    let mut authority_manager = SessionSyncAuthorityManager::with_ingress_events(
        network.clone(),
        None,
        internal_tx.clone(),
        target_factory,
        authority_backend,
    );
    let mut pending_create_sessions =
        HashMap::<String, mpsc::Sender<GrpcNodeSessionEnvelope>>::new();
    let mut registered_workspace_sockets = BTreeSet::<String>::new();
    let (event_tx, event_rx) = mpsc::channel::<IngressServerEvent>();
    let outbound_transport_tx = transport_tx.clone();

    let _transport_bridge = {
        let event_tx = event_tx.clone();
        thread::spawn(move || {
            while let Ok(event) = transport_rx.recv() {
                if event_tx.send(IngressServerEvent::Transport(event)).is_err() {
                    return;
                }
            }
        })
    };
    let _internal_bridge = {
        let event_tx = event_tx.clone();
        thread::spawn(move || {
            while let Ok(event) = internal_rx.recv() {
                if event_tx.send(IngressServerEvent::Internal(event)).is_err() {
                    return;
                }
            }
        })
    };
    let _catalog_bridge = {
        let event_tx = event_tx.clone();
        thread::spawn(move || {
            while let Ok(_request) = local_catalog_rx.recv() {
                if event_tx
                    .send(IngressServerEvent::Internal(
                        InternalEvent::LocalCatalogChanged,
                    ))
                    .is_err()
                {
                    return;
                }
            }
        })
    };
    drop(event_tx);
    let watcher = if start_authority_socket_watcher {
        match start_socket_watcher(internal_tx.clone()) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] authority socket watcher failed: {error}"
                ));
                return;
            }
        }
    } else {
        None
    };

    let mut high_priority_events = VecDeque::<IngressServerEvent>::new();
    let mut low_priority_events = VecDeque::<IngressServerEvent>::new();
    let mut publication_revisions = ReceiverPublicationRevisionTable::default();
    let mut socket_discovery_retry_scheduled = false;
    let mut closed_session_instances = HashSet::<String>::new();
    let mut outbound_guards = HashMap::<String, GrpcRemoteNodeTransportGuard>::new();
    let mut pending_outbound_guards = HashMap::<String, GrpcRemoteNodeTransportGuard>::new();
    let mut pending_outbound_dials = HashSet::<String>::new();
    let outbound_transport = GrpcRemoteNodeTransport::new();
    let (outbound_guard_tx, outbound_guard_rx) = mpsc::channel::<(
        String,
        Result<GrpcRemoteNodeTransportGuard, RemoteNodeTransportError>,
    )>();

    loop {
        while let Ok((node_id, result)) = outbound_guard_rx.try_recv() {
            pending_outbound_dials.remove(&node_id);
            match result {
                Ok(guard) => {
                    pending_outbound_guards.insert(node_id, guard);
                }
                Err(error) => {
                    // The worker thread already logged the error; nothing else
                    // to do here.
                    let _ = error;
                }
            }
        }
        drain_ingress_events(
            &event_rx,
            &mut high_priority_events,
            &mut low_priority_events,
        );
        let event = match next_ingress_event(&mut high_priority_events, &mut low_priority_events) {
            Some(event) => event,
            None => match event_rx.recv() {
                Ok(event) => {
                    enqueue_ingress_event(
                        &mut high_priority_events,
                        &mut low_priority_events,
                        event,
                    );
                    drain_ingress_events(
                        &event_rx,
                        &mut high_priority_events,
                        &mut low_priority_events,
                    );
                    match next_ingress_event(&mut high_priority_events, &mut low_priority_events) {
                        Some(event) => event,
                        None => continue,
                    }
                }
                Err(_) => break,
            },
        };

        match event {
            IngressServerEvent::Transport(event) => handle_transport_event(
                event,
                HandleTransportEventArgs {
                    publication_runtime: &publication_runtime,
                    authority_manager: &mut authority_manager,
                    sessions: &mut sessions,
                    pending_create_sessions: &mut pending_create_sessions,
                    outbound_guards: &mut outbound_guards,
                    pending_outbound_guards: &mut pending_outbound_guards,
                    registered_workspace_sockets: &registered_workspace_sockets,
                    publication_revisions: &mut publication_revisions,
                    internal_tx: internal_tx.clone(),
                    socket_discovery_retry_scheduled: &mut socket_discovery_retry_scheduled,
                    closed_session_instances: &mut closed_session_instances,
                    local_session_catalog: &local_session_catalog,
                },
            ),
            IngressServerEvent::Internal(InternalEvent::Shutdown) => break,
            IngressServerEvent::Internal(InternalEvent::ShutdownOwner { reply_tx }) => {
                let _ = reply_tx.send(AuthoritySocketReadyReply {
                    status: AuthoritySocketReadyStatus::Registered,
                    message: "remote-node-ingress owner shutting down".to_string(),
                });
                break;
            }
            IngressServerEvent::Internal(InternalEvent::LocalCreateSession {
                envelope,
                reply_tx,
            }) => {
                handle_local_create_session_request(
                    &mut sessions,
                    &mut pending_create_sessions,
                    envelope,
                    reply_tx,
                );
            }
            IngressServerEvent::Internal(InternalEvent::LocalCreateSessionTimedOut {
                request_id,
            }) => {
                pending_create_sessions.remove(&request_id);
            }
            IngressServerEvent::Internal(InternalEvent::InitiateOutboundConnection { request }) => {
                // Do not block the single ingress loop on the TCP/TLS handshake and
                // server hello exchange. Spawn a short-lived worker and drain the
                // resulting guard through outbound_guard_rx on the next iterations.
                //
                // Ignore duplicate requests for the same node: only one outbound dial
                // may be in flight at a time, and a new dial is pointless while an
                // active session already exists.
                let node_id = request.node_id.clone();
                if has_active_ingress_session_for_node(&sessions, &node_id)
                    || pending_outbound_dials.contains(&node_id)
                {
                    continue;
                }
                pending_outbound_dials.insert(node_id.clone());

                let outbound_transport = outbound_transport.clone();
                let outbound_guard_tx = outbound_guard_tx.clone();
                let outbound_transport_tx = outbound_transport_tx.clone();
                thread::spawn(move || {
                    let result =
                        outbound_transport.connect_outbound(request, outbound_transport_tx);
                    if let Err(error) = &result {
                        ERROR_LOG.log(format!(
                            "[remote-node-ingress] outbound connection failed: {error}"
                        ));
                    }
                    let _ = outbound_guard_tx.send((node_id, result));
                });
            }
            IngressServerEvent::Internal(InternalEvent::CloseNodeIngressSession { node_id }) => {
                close_ingress_sessions_for_node(
                    &publication_runtime,
                    &mut sessions,
                    &mut outbound_guards,
                    &mut pending_outbound_guards,
                    &mut pending_outbound_dials,
                    &mut closed_session_instances,
                    &node_id,
                );
            }
            IngressServerEvent::Internal(event) => {
                handle_internal_event(
                    &mut sessions,
                    &mut registered_workspace_sockets,
                    internal_tx.clone(),
                    &mut socket_discovery_retry_scheduled,
                    &local_session_catalog,
                    event,
                );
            }
        }
    }
    if let Some(handle) = watcher {
        let signal = [1u8];
        // SAFETY: `handle.shutdown_write` is a valid pipe fd owned by this
        // function; writing a single byte wakes the worker's blocking `poll`.
        unsafe {
            libc::write(
                handle.shutdown_write,
                signal.as_ptr() as *const libc::c_void,
                signal.len(),
            );
        }
        let _ = handle.worker.join();
        // SAFETY: `handle` owns both fds; close them after the worker has
        // exited.
        unsafe {
            libc::close(handle.shutdown_write);
            libc::close(handle.inotify_fd);
        }
    }
}

fn drain_ingress_events(
    event_rx: &mpsc::Receiver<IngressServerEvent>,
    high_priority_events: &mut VecDeque<IngressServerEvent>,
    low_priority_events: &mut VecDeque<IngressServerEvent>,
) {
    while let Ok(event) = event_rx.try_recv() {
        enqueue_ingress_event(high_priority_events, low_priority_events, event);
    }
}

fn next_ingress_event(
    high_priority_events: &mut VecDeque<IngressServerEvent>,
    low_priority_events: &mut VecDeque<IngressServerEvent>,
) -> Option<IngressServerEvent> {
    high_priority_events
        .pop_front()
        .or_else(|| low_priority_events.pop_front())
}

fn enqueue_ingress_event(
    high_priority_events: &mut VecDeque<IngressServerEvent>,
    low_priority_events: &mut VecDeque<IngressServerEvent>,
    event: IngressServerEvent,
) {
    match ingress_event_priority(&event) {
        IngressEventPriority::High => high_priority_events.push_back(event),
        IngressEventPriority::Low => low_priority_events.push_back(event),
    }
}

fn ingress_event_priority(event: &IngressServerEvent) -> IngressEventPriority {
    match event {
        IngressServerEvent::Internal(_) => IngressEventPriority::High,
        IngressServerEvent::Transport(RemoteNodeTransportEvent::EnvelopeReceived {
            envelope,
            ..
        }) => match envelope.body.as_ref() {
            Some(Body::TargetPublished(_)) | Some(Body::Heartbeat(_)) => IngressEventPriority::Low,
            _ => IngressEventPriority::High,
        },
        IngressServerEvent::Transport(_) => IngressEventPriority::High,
    }
}

struct HandleTransportEventArgs<'a, B, F, A, G>
where
    B: RemoteTargetPublicationBackend,
    F: LocalTargetFactory,
    A: LocalAuthorityHostBackend,
    G: LocalSessionCatalog,
{
    publication_runtime: &'a RemoteTargetPublicationRuntime<B>,
    authority_manager: &'a mut SessionSyncAuthorityManager<F, A>,
    sessions: &'a mut HashMap<String, ActiveNodeIngressSession>,
    pending_create_sessions: &'a mut HashMap<String, mpsc::Sender<GrpcNodeSessionEnvelope>>,
    outbound_guards: &'a mut HashMap<String, GrpcRemoteNodeTransportGuard>,
    pending_outbound_guards: &'a mut HashMap<String, GrpcRemoteNodeTransportGuard>,
    registered_workspace_sockets: &'a BTreeSet<String>,
    publication_revisions: &'a mut ReceiverPublicationRevisionTable,
    internal_tx: mpsc::Sender<InternalEvent>,
    socket_discovery_retry_scheduled: &'a mut bool,
    closed_session_instances: &'a mut HashSet<String>,
    local_session_catalog: &'a G,
}

fn handle_transport_event<
    B: RemoteTargetPublicationBackend,
    F: LocalTargetFactory,
    A: LocalAuthorityHostBackend,
    G: LocalSessionCatalog,
>(
    event: RemoteNodeTransportEvent,
    args: HandleTransportEventArgs<'_, B, F, A, G>,
) where
    LifecycleError: From<<A as LocalAuthorityHostBackend>::Error>,
{
    let HandleTransportEventArgs {
        publication_runtime,
        authority_manager,
        sessions,
        pending_create_sessions,
        outbound_guards,
        pending_outbound_guards,
        registered_workspace_sockets,
        publication_revisions,
        internal_tx,
        socket_discovery_retry_scheduled,
        closed_session_instances,
        local_session_catalog,
    } = args;
    match event {
        RemoteNodeTransportEvent::SessionOpened { session } => {
            let session_instance_id = session.session_instance_id().to_string();
            closed_session_instances.remove(&session_instance_id);

            let node_id = session.node_id().to_string();

            // If this session was opened by an outbound dial, the guard is waiting
            // in pending_outbound_guards. Move it to outbound_guards keyed by the
            // real session instance id so it can be dropped when the session closes.
            let is_outbound_dial = pending_outbound_guards.remove(&node_id).map(|guard| {
                outbound_guards.insert(session_instance_id.clone(), guard);
            });

            // For inbound `--connect` peers, probe whether the control host can
            // reach the peer's listening port. The result is recorded so the
            // state loop can choose a shorter offline timeout for LAN peers.
            if is_outbound_dial.is_none() {
                if let Some((host, port)) = parse_peer_node_id(&node_id) {
                    let publication_runtime = publication_runtime.clone();
                    let probe_node_id = node_id.clone();
                    let probe_host = host.clone();
                    thread::spawn(move || {
                        let reachable = probe_server_can_reach_peer(&probe_host, port);
                        if let Err(error) = publication_runtime
                            .record_inbound_remote_node_connection(
                                &probe_node_id,
                                &probe_host,
                                port,
                                reachable,
                            )
                        {
                            ERROR_LOG.log_error(format!(
                                "ingress server: failed to record inbound peer reachability for {probe_node_id}: {error}"
                            ));
                        }
                    });
                }
            }

            let was_offline = !has_active_ingress_session_for_node(sessions, &node_id);

            let mut active = ActiveNodeIngressSession {
                session,
                bridges: HashMap::new(),
                published_fingerprints: HashMap::new(),
                source_publication_tracker: SourcePublicationTracker::new(),
                observed_sessions: HashMap::new(),
                published_sessions: HashMap::new(),
                observed_initialized: false,
                next_message_id: 0,
            };

            // Publish the current local session catalog as a full baseline to the
            // new peer. This makes outbound-dial peers behave like `--connect`
            // peers: the control plane can see existing sessions immediately.
            active.source_publication_tracker.on_connected();
            if let Err(error) = publish_local_catalog_baseline(
                local_session_catalog,
                &node_id,
                &session_instance_id,
                &mut active,
            ) {
                ERROR_LOG.log_error(format!(
                    "ingress server: failed to publish baseline to {node_id}: {error}"
                ));
            }

            let outcome = refresh_authority_bridges(&node_id, &mut active, internal_tx.clone());
            if outcome.pending > 0 {
                schedule_socket_discovery_retry(
                    internal_tx,
                    BRIDGE_DISCOVERY_RETRY_ATTEMPTS,
                    socket_discovery_retry_scheduled,
                );
            }
            sessions.insert(session_instance_id, active);

            if was_offline {
                if let Err(error) = publication_runtime.signal_remote_node_online(&node_id) {
                    ERROR_LOG.log_error(format!(
                        "ingress server: failed to signal remote node online {node_id}: {error}"
                    ));
                }
            }
        }
        RemoteNodeTransportEvent::EnvelopeReceived {
            node_id,
            session_instance_id,
            envelope: boxed_envelope,
        } => {
            let envelope = *boxed_envelope;
            if closed_session_instances.contains(&session_instance_id) {
                return;
            }
            if let Some(request_id) = grpc_create_session_reply_request_id(&envelope) {
                let matched = pending_create_sessions.contains_key(&request_id);
                ERROR_LOG.log(format!(
                    "[ingress-create-session] recv reply id={request_id} matched={matched} session={session_instance_id}"
                ));
                if let Some(reply_tx) = pending_create_sessions.remove(&request_id) {
                    let _ = reply_tx.send(envelope);
                    return;
                }
            }
            let is_authority_event = matches!(
                envelope.body.as_ref(),
                Some(Body::OpenMirrorRequest(_))
                    | Some(Body::CloseMirrorRequest(_))
                    | Some(Body::ApplyPtyResize(_))
                    | Some(Body::RawPtyInput(_))
                    | Some(Body::PasteFileRequest(_))
                    | Some(Body::CreateSessionRequest(_))
            );
            if is_authority_event {
                if let Some(body_name) = envelope.body.as_ref().map(|b| match b {
                    Body::CreateSessionRequest(_) => "CreateSessionRequest",
                    _ => "other-authority-event",
                }) {
                    ERROR_LOG.log(format!(
                        "[ingress-create-session] recv authority event {body_name} session={session_instance_id}"
                    ));
                }
                if let Some(active) = sessions.get(&session_instance_id) {
                    if let Some(event) = map_inbound_grpc_authority_event(envelope) {
                        authority_manager.handle_event(&active.session, event);
                    }
                }
                return;
            }
            if let Some(active) = sessions.get_mut(&session_instance_id) {
                let _ = route_transport_envelope(
                    publication_runtime,
                    &node_id,
                    envelope,
                    Some(active),
                    registered_workspace_sockets,
                    publication_revisions,
                );
            } else {
                let _ = route_transport_envelope(
                    publication_runtime,
                    &node_id,
                    envelope,
                    None,
                    registered_workspace_sockets,
                    publication_revisions,
                );
            }
        }
        RemoteNodeTransportEvent::SessionClosed {
            node_id,
            session_instance_id,
            ..
        } => {
            sessions.remove(&session_instance_id);
            closed_session_instances.insert(session_instance_id.clone());
            // Drop the outbound transport guard for this session so the worker
            // thread exits and the TCP connection is closed.
            outbound_guards.remove(&session_instance_id);
            pending_outbound_guards.remove(&node_id);
            // Do NOT stop authority hosts here. An authority target host's
            // lifetime belongs to the remote target session, not to the inbound
            // gRPC session that happens to be routing for it. Stopping the host
            // breaks keyboard input across a transient disconnect. A stale host
            // is still replaced when a new client session attaches to the same
            // target via SessionSyncAuthorityManager::ensure_authority_host.
            mark_discovered_node_offline_if_last_ingress_session(
                publication_runtime,
                sessions,
                node_id.as_str(),
            );
        }
        RemoteNodeTransportEvent::TransportFailed {
            node_id,
            session_instance_id,
            ..
        } => {
            if let Some(session_instance_id) = session_instance_id {
                sessions.remove(&session_instance_id);
                closed_session_instances.insert(session_instance_id.clone());
                outbound_guards.remove(&session_instance_id);
                // Authority hosts are not tied to the inbound gRPC session; see
                // the SessionClosed branch above.
            }
            if let Some(node_id) = node_id {
                pending_outbound_guards.remove(&node_id);
                mark_discovered_node_offline_if_last_ingress_session(
                    publication_runtime,
                    sessions,
                    node_id.as_str(),
                );
            }
        }
    }
}

fn publish_local_catalog_baseline<G: LocalSessionCatalog>(
    local_session_catalog: &G,
    node_id: &str,
    session_instance_id: &str,
    active: &mut ActiveNodeIngressSession,
) -> Result<(), crate::infra::remote_grpc_transport::RemoteNodeTransportError> {
    observe_local_session_catalog(
        local_session_catalog,
        node_id,
        &mut active.observed_sessions,
        &mut active.observed_initialized,
    );

    for session in active.observed_sessions.values() {
        let publication = active.source_publication_tracker.on_baseline_state(
            node_id,
            session_instance_id,
            &mut active.next_message_id,
            session,
        );
        send_source_publication(
            &active.session,
            &mut active.source_publication_tracker,
            &publication,
        )?;
    }

    active.published_sessions = active.observed_sessions.clone();
    Ok(())
}

fn publish_local_catalog_delta<G: LocalSessionCatalog>(
    local_session_catalog: &G,
    node_id: &str,
    session_instance_id: &str,
    active: &mut ActiveNodeIngressSession,
) -> Result<(), crate::infra::remote_grpc_transport::RemoteNodeTransportError> {
    let changed = observe_local_session_catalog(
        local_session_catalog,
        node_id,
        &mut active.observed_sessions,
        &mut active.observed_initialized,
    );
    if !changed {
        return Ok(());
    }

    let delta = compute_session_sync_delta(
        &active.published_sessions,
        &active.observed_sessions,
        SessionSyncMode::Delta,
    );

    for session in &delta.publish {
        let Some(publication) = active.source_publication_tracker.on_state_changed(
            node_id,
            session_instance_id,
            &mut active.next_message_id,
            session,
        ) else {
            continue;
        };
        send_source_publication(
            &active.session,
            &mut active.source_publication_tracker,
            &publication,
        )?;
    }

    for session in &delta.exit {
        let publication = active.source_publication_tracker.on_target_exited(
            node_id,
            session_instance_id,
            &mut active.next_message_id,
            session.address.session_id(),
        );
        send_source_publication(
            &active.session,
            &mut active.source_publication_tracker,
            &publication,
        )?;
    }

    active.published_sessions = active.observed_sessions.clone();
    Ok(())
}

fn has_active_ingress_session_for_node(
    sessions: &HashMap<String, ActiveNodeIngressSession>,
    node_id: &str,
) -> bool {
    sessions
        .values()
        .any(|active| active.session.node_id() == node_id)
}

fn close_ingress_sessions_for_node<B: RemoteTargetPublicationBackend>(
    publication_runtime: &RemoteTargetPublicationRuntime<B>,
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    outbound_guards: &mut HashMap<String, GrpcRemoteNodeTransportGuard>,
    pending_outbound_guards: &mut HashMap<String, GrpcRemoteNodeTransportGuard>,
    pending_outbound_dials: &mut HashSet<String>,
    closed_session_instances: &mut HashSet<String>,
    node_id: &str,
) {
    let removed_instance_ids: Vec<String> = sessions
        .iter()
        .filter(|(_session_instance_id, active)| active.session.node_id() == node_id)
        .map(|(session_instance_id, _active)| session_instance_id.clone())
        .collect();
    for session_instance_id in &removed_instance_ids {
        sessions.remove(session_instance_id);
        closed_session_instances.insert(session_instance_id.clone());
        outbound_guards.remove(session_instance_id);
    }
    pending_outbound_guards.remove(node_id);
    pending_outbound_dials.remove(node_id);
    mark_discovered_node_offline_if_last_ingress_session(publication_runtime, sessions, node_id);
}

fn mark_discovered_node_offline_if_last_ingress_session<B: RemoteTargetPublicationBackend>(
    publication_runtime: &RemoteTargetPublicationRuntime<B>,
    sessions: &HashMap<String, ActiveNodeIngressSession>,
    node_id: &str,
) {
    if has_active_ingress_session_for_node(sessions, node_id) {
        return;
    }
    if let Err(error) = publication_runtime.mark_discovered_remote_node_offline(node_id) {
        ERROR_LOG.log_error(format!(
            "ingress server: failed to mark discovered node offline {node_id}: {error}"
        ));
    }
    if let Err(error) = publication_runtime.signal_remote_node_offline(node_id) {
        ERROR_LOG.log_error(format!(
            "ingress server: failed to signal remote node offline {node_id}: {error}"
        ));
    }
}

fn handle_local_create_session_request(
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    pending_create_sessions: &mut HashMap<String, mpsc::Sender<GrpcNodeSessionEnvelope>>,
    envelope: GrpcNodeSessionEnvelope,
    reply_tx: mpsc::Sender<GrpcNodeSessionEnvelope>,
) {
    let Some(authority_node_id) = envelope
        .route
        .as_ref()
        .and_then(|route| route.authority_node_id.clone())
    else {
        let _ = reply_tx.send(local_create_session_rejected_grpc_envelope(
            String::new(),
            "missing authority node id".to_string(),
        ));
        return;
    };
    let Some(active) = sessions
        .values()
        .find(|active| active.session.node_id() == authority_node_id)
    else {
        let request_id = grpc_create_session_request_id(&envelope).unwrap_or_default();
        ERROR_LOG.log(format!(
            "[ingress-create-session] authority `{authority_node_id}` not connected, rejecting id={request_id}"
        ));
        let _ = reply_tx.send(local_create_session_rejected_grpc_envelope(
            request_id,
            format!("remote authority `{authority_node_id}` is not connected"),
        ));
        return;
    };
    let request_id = grpc_create_session_request_id(&envelope).unwrap_or_default();
    let session_instance_id = active.session.session_instance_id().to_string();

    ERROR_LOG.log(format!(
        "[ingress-create-session] sending request id={request_id} to node={authority_node_id} session={session_instance_id}"
    ));
    pending_create_sessions.insert(request_id.clone(), reply_tx);
    let send_started = std::time::Instant::now();
    if active.session.send(envelope).is_err() {
        ERROR_LOG.log_error(format!(
            "[ingress-create-session] failed sending create-session request id={request_id} after {:?}",
            send_started.elapsed()
        ));
        if let Some(reply_tx) = pending_create_sessions.remove(&request_id) {
            let _ = reply_tx.send(local_create_session_rejected_grpc_envelope(
                request_id,
                format!("failed to send create-session request to `{authority_node_id}`"),
            ));
        }
    }
}

fn grpc_create_session_request_id(envelope: &GrpcNodeSessionEnvelope) -> Option<String> {
    match envelope.body.as_ref() {
        Some(Body::CreateSessionRequest(payload)) => Some(payload.request_id.clone()),
        _ => None,
    }
}

fn grpc_create_session_reply_request_id(envelope: &GrpcNodeSessionEnvelope) -> Option<String> {
    match envelope.body.as_ref() {
        Some(Body::CreateSessionAccepted(payload)) => Some(payload.request_id.clone()),
        Some(Body::CreateSessionRejected(payload)) => Some(payload.request_id.clone()),
        _ => None,
    }
}

/// Parse a peer node id of the form `{host}#{port}` into its components.
/// Returns `None` if the id does not follow this convention.
fn parse_peer_node_id(node_id: &str) -> Option<(String, u16)> {
    let (host, port) = node_id.rsplit_once('#')?;
    let port = port.parse().ok()?;
    Some((host.to_string(), port))
}

fn map_outbound_grpc_envelope_for_local_request(
    request: crate::infra::remote_protocol::NodeSessionEnvelope,
) -> GrpcNodeSessionEnvelope {
    match map_outbound_grpc_envelope("local-create-session", request.channel, &request.envelope) {
        Ok(envelope) => envelope,
        Err(_) => local_create_session_rejected_grpc_envelope(
            String::new(),
            "failed to map local create-session request".to_string(),
        ),
    }
}

fn local_create_session_request_grpc_envelope(
    authority_node_id: String,
    payload: CreateSessionRequest,
) -> GrpcNodeSessionEnvelope {
    GrpcNodeSessionEnvelope {
        message_id: format!("local-create-session-{}", payload.request_id),
        sent_at: None,
        session_instance_id: String::new(),
        correlation_id: Some(payload.request_id.clone()),
        route: Some(RouteContext {
            authority_node_id: Some(authority_node_id),
            target_id: None,
            attachment_id: None,
            console_id: None,
            console_host_id: None,
            session_id: None,
        }),
        body: Some(Body::CreateSessionRequest(payload)),
    }
}

fn map_local_reply_from_grpc(
    reply: GrpcNodeSessionEnvelope,
) -> Option<crate::infra::remote_protocol::NodeSessionEnvelope> {
    let payload = match reply.body? {
        Body::CreateSessionAccepted(payload) => {
            ControlPlanePayload::CreateSessionAccepted(CreateSessionAcceptedPayload {
                request_id: payload.request_id,
                session_id: payload.session_id,
                target_id: payload.target_id,
            })
        }
        Body::CreateSessionRejected(payload) => {
            ControlPlanePayload::CreateSessionRejected(CreateSessionRejectedPayload {
                request_id: payload.request_id,
                code: "create_session_failed",
                message: payload.reason,
            })
        }
        _ => return None,
    };
    Some(crate::infra::remote_protocol::NodeSessionEnvelope {
        channel: crate::infra::remote_protocol::NodeSessionChannel::Authority,
        envelope: ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: reply.message_id,
            message_type: payload.message_type(),
            timestamp: format!("{}Z", now_millis()),
            sender_id: "waitagent-remote-node-ingress-owner".to_string(),
            correlation_id: reply.correlation_id,
            session_id: None,
            target_id: None,
            attachment_id: None,
            console_id: None,
            payload,
        },
    })
}

fn write_create_session_rejected_to_stream(
    stream: &mut impl Write,
    request_id: String,
    message: impl Into<String>,
) -> io::Result<()> {
    let payload = ControlPlanePayload::CreateSessionRejected(CreateSessionRejectedPayload {
        request_id,
        code: "create_session_failed",
        message: message.into(),
    });
    write_node_session_envelope(
        stream,
        &crate::infra::remote_protocol::NodeSessionEnvelope {
            channel: crate::infra::remote_protocol::NodeSessionChannel::Authority,
            envelope: ProtocolEnvelope {
                protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                message_id: format!("local-create-session-rejected-{}", now_millis()),
                message_type: payload.message_type(),
                timestamp: format!("{}Z", now_millis()),
                sender_id: "waitagent-remote-node-ingress-owner".to_string(),
                correlation_id: None,
                session_id: None,
                target_id: None,
                attachment_id: None,
                console_id: None,
                payload,
            },
        },
    )
    .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
}

fn local_create_session_rejected_grpc_envelope(
    request_id: String,
    message: String,
) -> GrpcNodeSessionEnvelope {
    GrpcNodeSessionEnvelope {
        message_id: format!("local-create-session-rejected-{}", now_millis()),
        sent_at: None,
        session_instance_id: String::new(),
        correlation_id: Some(request_id.clone()),
        route: None,
        body: Some(Body::CreateSessionRejected(
            crate::infra::remote_grpc_proto::v1::CreateSessionRejected {
                request_id,
                reason: message,
                status: None,
            },
        )),
    }
}

fn handle_internal_event<G: LocalSessionCatalog>(
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    registered_workspace_sockets: &mut BTreeSet<String>,
    internal_tx: mpsc::Sender<InternalEvent>,
    socket_discovery_retry_scheduled: &mut bool,
    local_session_catalog: &G,
    event: InternalEvent,
) {
    match event {
        InternalEvent::BridgeClosed { node_id, endpoint } => {
            for active in sessions.values_mut() {
                if active.session.node_id() == node_id {
                    active.bridges.remove(&endpoint);
                }
            }
        }
        InternalEvent::AuthorityCommandReceived {
            node_id,
            session_instance_id,
            endpoint,
            command,
        } => {
            ERROR_LOG.log(format!(
                "[remote-node-ingress] authority command received node={node_id} session_instance_id={session_instance_id} endpoint={endpoint} command={command:?}",
            ));
            let Some(active) = sessions.get(&session_instance_id) else {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] dropping authority command for node={node_id} session_instance_id={session_instance_id} endpoint={endpoint} because no active session is open",
                ));
                return;
            };
            let envelope = match map_authority_command_to_grpc(&active.session, command) {
                Ok(envelope) => envelope,
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[remote-node-ingress] failed to map authority command for node={node_id} session_instance_id={session_instance_id} endpoint={endpoint}: {error}",
                    ));
                    return;
                }
            };
            if let Err(error) = active.session.send(envelope) {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] failed to send authority command for node={node_id} session_instance_id={session_instance_id} endpoint={endpoint}: {error}",
                ));
            }
        }
        InternalEvent::AuthorityHostOutput {
            node_id,
            session_instance_id,
            envelope,
        } => {
            let Some(active) = sessions.get(&session_instance_id) else {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] dropping authority output for node={node_id} session_instance_id={session_instance_id} type={} because no active session is open",
                    envelope.payload.message_type()
                ));
                return;
            };
            let grpc = match map_outbound_grpc_envelope(
                active.session.node_id(),
                crate::infra::remote_protocol::NodeSessionChannel::Authority,
                &envelope,
            ) {
                Ok(grpc) => grpc,
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[remote-node-ingress] failed to map authority output for node={node_id} session_instance_id={session_instance_id} type={}: {error}",
                        envelope.payload.message_type()
                    ));
                    return;
                }
            };

            if let Err(error) = active.session.send(grpc) {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] failed to send authority output for node={node_id} session_instance_id={session_instance_id} type={}: {error}",
                    envelope.payload.message_type()
                ));
            }
        }
        InternalEvent::AuthoritySocketReady {
            node_id,
            endpoint,
            id_file,
            reply_tx,
        } => {
            let outcome = refresh_authority_bridge_for_socket(
                sessions,
                internal_tx,
                &node_id,
                &endpoint,
                &id_file,
                socket_discovery_retry_scheduled,
            );
            let reply = authority_socket_ready_reply(&node_id, &id_file, outcome);
            let _ = reply_tx.send(reply);
        }
        InternalEvent::RegisterWorkspaceSocket {
            socket_name,
            reply_tx,
        } => {
            registered_workspace_sockets.insert(socket_name);
            let _ = reply_tx.send(AuthoritySocketReadyReply {
                status: AuthoritySocketReadyStatus::Registered,
                message: "workspace socket registered".to_string(),
            });
        }
        InternalEvent::UnregisterWorkspaceSocket {
            socket_name,
            reply_tx,
        } => {
            registered_workspace_sockets.remove(&socket_name);
            let _ = reply_tx.send(AuthoritySocketReadyReply {
                status: AuthoritySocketReadyStatus::Registered,
                message: "workspace socket unregistered".to_string(),
            });
        }
        InternalEvent::SocketDirChanged => {
            refresh_authority_bridges_for_sessions(
                sessions,
                internal_tx,
                BRIDGE_DISCOVERY_RETRY_ATTEMPTS,
                socket_discovery_retry_scheduled,
            );
        }
        InternalEvent::RetrySocketDiscovery { attempts_remaining } => {
            *socket_discovery_retry_scheduled = false;
            refresh_authority_bridges_for_sessions(
                sessions,
                internal_tx,
                attempts_remaining,
                socket_discovery_retry_scheduled,
            );
        }
        InternalEvent::LocalCatalogChanged => {
            for active in sessions.values_mut() {
                let node_id = active.session.node_id().to_string();
                let session_instance_id = active.session.session_instance_id().to_string();
                if let Err(error) = publish_local_catalog_delta(
                    local_session_catalog,
                    &node_id,
                    &session_instance_id,
                    active,
                ) {
                    ERROR_LOG.log_error(format!(
                        "ingress server: failed to publish delta to {node_id}: {error}"
                    ));
                }
            }
        }
        InternalEvent::Shutdown
        | InternalEvent::ShutdownOwner { .. }
        | InternalEvent::LocalCreateSession { .. }
        | InternalEvent::LocalCreateSessionTimedOut { .. }
        | InternalEvent::InitiateOutboundConnection { .. }
        | InternalEvent::CloseNodeIngressSession { .. } => {}
    }
}

fn send_publication_ack(
    session: Option<&ActiveNodeIngressSession>,
    node_id: &str,
    node_instance_id: &str,
    target_id: &str,
    revision: u64,
    status: GrpcTargetPublicationAckStatus,
    message: Option<String>,
) {
    if node_instance_id.is_empty() || revision == 0 {
        return;
    }
    let Some(session) = session else {
        return;
    };
    let envelope = GrpcNodeSessionEnvelope {
        message_id: format!("publication-ack-{}", now_millis()),
        sent_at: None,
        session_instance_id: session.session.session_instance_id().to_string(),
        correlation_id: None,
        route: Some(RouteContext {
            authority_node_id: Some(node_id.to_string()),
            target_id: Some(target_id.to_string()),
            attachment_id: None,
            console_id: None,
            console_host_id: None,
            session_id: None,
        }),
        body: Some(Body::TargetPublicationAck(GrpcTargetPublicationAck {
            node_id: node_id.to_string(),
            node_instance_id: node_instance_id.to_string(),
            target_id: target_id.to_string(),
            revision,
            status: status as i32,
            message,
        })),
    };
    if let Err(error) = session.session.send(envelope) {
        ERROR_LOG.log_error(format!(
            "failed to send publication ack node={node_id} target={target_id} revision={revision}: {error}"
        ));
    }
}

fn target_published_fingerprint(payload: &GrpcTargetPublished) -> String {
    [
        payload.target_id.clone(),
        payload.transport_session_id.clone(),
        payload.availability.clone(),
        format!("{:?}", payload.selector),
        payload.transport.clone(),
        format!("{:?}", payload.command_name),
        format!("{:?}", payload.current_path),
        format!("{:?}", payload.attached_count),
        format!("{:?}", payload.session_role),
        format!("{:?}", payload.window_count),
        format!("{:?}", payload.task_state),
        payload.node_instance_id.clone(),
        payload.revision.to_string(),
    ]
    .join("\u{1f}")
}

fn route_transport_envelope<B: RemoteTargetPublicationBackend>(
    publication_runtime: &RemoteTargetPublicationRuntime<B>,
    node_id: &str,
    envelope: GrpcNodeSessionEnvelope,
    session: Option<&mut ActiveNodeIngressSession>,
    registered_workspace_sockets: &BTreeSet<String>,
    publication_revisions: &mut ReceiverPublicationRevisionTable,
) -> Result<(), LifecycleError> {
    match envelope.body.as_ref() {
        Some(Body::TargetPublished(payload)) => handle_target_published(
            publication_runtime,
            node_id,
            &envelope,
            payload,
            session,
            publication_revisions,
        ),
        Some(Body::TargetPublicationAck(payload)) => {
            handle_target_publication_ack(publication_runtime, node_id, &envelope, payload, session)
        }

        Some(Body::TargetExited(payload)) => handle_target_exited(
            publication_runtime,
            node_id,
            &envelope,
            payload,
            session,
            registered_workspace_sockets,
            publication_revisions,
        ),
        Some(Body::TargetOutput(payload)) => {
            handle_target_output(node_id, &envelope, payload, session)
        }
        Some(Body::RawPtyOutput(payload)) => {
            handle_raw_pty_output(node_id, &envelope, payload, session)
        }
        Some(Body::MirrorBootstrapChunk(payload)) => {
            handle_mirror_bootstrap_chunk(node_id, &envelope, payload, session)
        }
        Some(Body::MirrorBootstrapComplete(payload)) => {
            handle_mirror_bootstrap_complete(node_id, &envelope, payload, session)
        }
        Some(Body::OpenMirrorRequest(payload)) => {
            handle_open_mirror_request(node_id, &envelope, payload, session)
        }
        Some(Body::CloseMirrorRequest(payload)) => {
            handle_close_mirror_request(node_id, &envelope, payload, session)
        }
        Some(Body::ApplyPtyResize(payload)) => {
            handle_apply_pty_resize(node_id, &envelope, payload, session)
        }
        Some(Body::PtyResizeApplied(payload)) => {
            handle_pty_resize_applied(node_id, &envelope, payload, session)
        }
        Some(Body::TargetGeometryChanged(payload)) => {
            handle_target_geometry_changed(node_id, &envelope, payload, session)
        }
        Some(Body::RawPtyInput(payload)) => {
            handle_raw_pty_input(node_id, &envelope, payload, session)
        }
        Some(Body::PasteFileRequest(payload)) => {
            handle_paste_file_request(node_id, &envelope, payload, session)
        }
        Some(Body::Heartbeat(_)) | Some(Body::ClientHello(_)) | Some(Body::ServerHello(_)) => {
            Ok(())
        }
        _ => Ok(()),
    }
}

fn handle_target_published<B: RemoteTargetPublicationBackend>(
    publication_runtime: &RemoteTargetPublicationRuntime<B>,
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &GrpcTargetPublished,
    mut session: Option<&mut ActiveNodeIngressSession>,
    publication_revisions: &mut ReceiverPublicationRevisionTable,
) -> Result<(), LifecycleError> {
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    match publication_revisions.decision(
        node_id,
        &payload.node_instance_id,
        &target_id,
        payload.revision,
    ) {
        PublicationRevisionDecision::Stale => {
            send_publication_ack(
                session.as_deref(),
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
                GrpcTargetPublicationAckStatus::StaleRevision,
                None,
            );
            return Ok(());
        }
        PublicationRevisionDecision::Legacy | PublicationRevisionDecision::Apply => {}
    }
    if let Some(active) = session.as_deref_mut() {
        let fingerprint = target_published_fingerprint(payload);
        if active
            .published_fingerprints
            .get(&payload.transport_session_id)
            == Some(&fingerprint)
        {
            send_publication_ack(
                session.as_deref(),
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
                GrpcTargetPublicationAckStatus::Applied,
                None,
            );
            return Ok(());
        }
        active
            .published_fingerprints
            .insert(payload.transport_session_id.clone(), fingerprint);
    }
    let mapped = map_target_published_envelope(node_id, envelope, payload)
        .map_err(remote_node_ingress_error)?;
    match publication_runtime.apply_discovered_remote_session_envelope(node_id, mapped) {
        Ok(()) => {
            publication_revisions.mark_applied(
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
            );
            send_publication_ack(
                session.as_deref(),
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
                GrpcTargetPublicationAckStatus::Applied,
                None,
            );
            Ok(())
        }
        Err(error) => {
            send_publication_ack(
                session.as_deref(),
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
                GrpcTargetPublicationAckStatus::Failed,
                Some(error.to_string()),
            );
            Err(error)
        }
    }
}

fn handle_target_publication_ack<B: RemoteTargetPublicationBackend>(
    publication_runtime: &RemoteTargetPublicationRuntime<B>,
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &GrpcTargetPublicationAck,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let mapped = map_target_publication_ack_envelope(node_id, envelope, payload)
        .map_err(remote_node_ingress_error)?;
    if let Some(active) = session {
        if let crate::infra::remote_protocol::ControlPlanePayload::TargetPublicationAck(
            ref ack_payload,
        ) = mapped.payload
        {
            active.source_publication_tracker.on_ack(ack_payload);
        }
    }
    publication_runtime.apply_discovered_remote_session_envelope(node_id, mapped)
}

fn handle_target_exited<B: RemoteTargetPublicationBackend>(
    publication_runtime: &RemoteTargetPublicationRuntime<B>,
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &GrpcTargetExited,
    session: Option<&mut ActiveNodeIngressSession>,
    registered_workspace_sockets: &BTreeSet<String>,
    publication_revisions: &mut ReceiverPublicationRevisionTable,
) -> Result<(), LifecycleError> {
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    match publication_revisions.decision(
        node_id,
        &payload.node_instance_id,
        &target_id,
        payload.revision,
    ) {
        PublicationRevisionDecision::Stale => {
            send_publication_ack(
                session.as_deref(),
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
                GrpcTargetPublicationAckStatus::StaleRevision,
                None,
            );
            return Ok(());
        }
        PublicationRevisionDecision::Legacy | PublicationRevisionDecision::Apply => {}
    }
    let _t_exit = std::time::Instant::now();
    ERROR_LOG.log_error(format!(
        "ingress_server route_transport_envelope: received TargetExited node={node_id} session={}",
        payload.transport_session_id
    ));
    let mapped = map_target_exited_envelope(node_id, envelope, payload);
    let _t_apply = std::time::Instant::now();
    match publication_runtime.apply_discovered_remote_session_envelope_for_sockets(
        node_id,
        mapped,
        &registered_workspace_sockets
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
    ) {
        Ok(()) => {
            publication_revisions.mark_applied(
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
            );
            send_publication_ack(
                session.as_deref(),
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
                GrpcTargetPublicationAckStatus::Applied,
                None,
            );
        }
        Err(error) => {
            send_publication_ack(
                session.as_deref(),
                node_id,
                &payload.node_instance_id,
                &target_id,
                payload.revision,
                GrpcTargetPublicationAckStatus::Failed,
                Some(error.to_string()),
            );
            return Err(error);
        }
    }

    ERROR_LOG.log_error(format!(
        "ingress_server: applied TargetExited to live workspaces node={node_id} session={}",
        payload.transport_session_id
    ));
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.transport_session_id, &payload.target_id))
        .unwrap_or_else(|| payload.transport_session_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_payload(
                session_id,
                target_id,
                ControlPlanePayload::TargetExited(TargetExitedPayload {
                    transport_session_id: payload.transport_session_id.clone(),
                    node_instance_id: payload.node_instance_id.clone(),
                    revision: payload.revision,
                    authority_host_session_name: None,
                }),
            )
        },
    )
}

fn handle_target_output(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &crate::infra::remote_grpc_proto::v1::TargetOutput,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let stream = known_output_stream(&payload.stream).map_err(remote_node_ingress_error)?;
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_target_output(
                session_id,
                target_id,
                payload.output_seq,
                stream,
                payload.output_bytes.clone(),
            )
        },
    )
}

fn handle_raw_pty_output(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &crate::infra::remote_grpc_proto::v1::RawPtyOutput,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };

    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_raw_pty_output(
                session_id,
                target_id,
                payload.output_seq,
                payload.output_bytes.clone(),
            )
        },
    )
}

fn handle_mirror_bootstrap_chunk(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &crate::infra::remote_grpc_proto::v1::MirrorBootstrapChunk,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let stream = known_output_stream(&payload.stream).map_err(remote_node_ingress_error)?;
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_mirror_bootstrap_chunk(
                session_id,
                target_id,
                payload.chunk_seq,
                stream,
                payload.output_bytes.clone(),
            )
        },
    )
}

fn handle_mirror_bootstrap_complete(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &crate::infra::remote_grpc_proto::v1::MirrorBootstrapComplete,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_mirror_bootstrap_complete(
                session_id,
                target_id,
                payload.last_chunk_seq,
                payload.alternate_screen_active,
                payload.application_cursor_keys,
                payload.cursor_visible,
            )
        },
    )
}

fn handle_open_mirror_request(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &OpenMirrorRequest,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_open_mirror_request(
                crate::remote::authority::remote_authority_transport_runtime::SendOpenMirrorRequestArgs {
                    session_id,
                    target_id,
                    console_id: &payload.console_id,
                    cols: payload.cols as usize,
                    rows: payload.rows as usize,
                    raw_pty_passthrough: payload.raw_pty_passthrough,
                    bootstrap_mode: if payload.bootstrap_mode_visible_only {
                        BootstrapMode::VisibleOnly
                    } else {
                        BootstrapMode::Full
                    },
                },
            )
        },
    )
}

fn handle_close_mirror_request(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &CloseMirrorRequest,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_close_mirror_request(session_id, target_id)
        },
    )
}

fn handle_apply_pty_resize(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &ApplyPtyResize,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_apply_resize(
                session_id,
                target_id,
                payload.cols as usize,
                payload.rows as usize,
                payload.resize_epoch,
                payload.resize_authority_console_id.clone(),
            )
        },
    )
}

fn handle_pty_resize_applied(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &crate::infra::remote_grpc_proto::v1::PtyResizeApplied,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_resize_applied(
                session_id,
                target_id,
                payload.cols as usize,
                payload.rows as usize,
                payload.resize_epoch,
                payload.resize_authority_console_id.clone(),
            )
        },
    )
}

fn handle_target_geometry_changed(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &crate::infra::remote_grpc_proto::v1::TargetGeometryChanged,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_target_geometry_changed(
                session_id,
                target_id,
                payload.cols as usize,
                payload.rows as usize,
            )
        },
    )
}

fn handle_raw_pty_input(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &RawPtyInput,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_raw_pty_input(
                crate::remote::authority::remote_authority_transport_runtime::SendRawPtyInputArgs {
                    session_id,
                    target_id,
                    console_id: &payload.console_id,
                    attachment_id: &payload.attachment_id,
                    console_host_id: &payload.console_host_id,
                    input_seq: payload.input_seq,
                    input_bytes: payload.input_bytes.clone(),
                },
            )
        },
    )
}

fn handle_paste_file_request(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &PasteFileRequest,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    let Some(session) = session else {
        return Ok(());
    };
    let session_id = route_session_id(envelope)
        .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
        .unwrap_or_else(|| payload.target_id.clone());
    let target_id = route_target_id(envelope).unwrap_or_else(|| payload.target_id.clone());
    bridge_output_to_authority_transports(
        node_id,
        session,
        session_id,
        target_id,
        |transport, session_id, target_id| {
            transport.send_payload(
                session_id,
                target_id,
                ControlPlanePayload::PasteFileRequest(PasteFileRequestPayload {
                    session_id: session_id.to_string(),
                    target_id: target_id.to_string(),
                    filename_hint: payload.filename_hint.clone(),
                    file_id: payload.file_id.clone(),
                    total_chunks: payload.total_chunks,
                    chunk_index: payload.chunk_index,
                    chunk_bytes: payload.chunk_bytes.clone(),
                }),
            )
        },
    )
}

fn bridge_output_to_authority_transports<F>(
    node_id: &str,
    session: &mut ActiveNodeIngressSession,
    session_id: String,
    target_id: String,
    mut deliver: F,
) -> Result<(), LifecycleError>
where
    F: FnMut(
        &RemoteAuthorityTransportRuntime,
        &str,
        &str,
    ) -> Result<
        (),
        crate::remote::authority::remote_authority_transport_runtime::RemoteAuthorityTransportError,
    >,
{
    let target_component = authority_target_component(node_id, &session_id);
    let mut stale = Vec::new();
    for (endpoint, bridge) in &session.bridges {
        if bridge.target_component != target_component {
            continue;
        }
        if let Err(error) = deliver(&bridge.transport, &session_id, &target_id) {
            let _ = error;
            stale.push(endpoint.clone());
        }
    }
    for endpoint in stale {
        session.bridges.remove(&endpoint);
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct BridgeRefreshOutcome {
    connected: usize,
    pending: usize,
    already_registered: usize,
    invalid: usize,
}

fn refresh_authority_bridge_for_socket(
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    internal_tx: mpsc::Sender<InternalEvent>,
    node_id: &str,
    endpoint: &RemoteControlAddr,
    id_file: &Path,
    socket_discovery_retry_scheduled: &mut bool,
) -> BridgeRefreshOutcome {
    let mut total = BridgeRefreshOutcome::default();
    for active in sessions.values_mut() {
        if active.session.node_id() != node_id {
            continue;
        }
        let outcome =
            refresh_authority_bridge_path(node_id, active, endpoint, id_file, internal_tx.clone());
        total.connected += outcome.connected;
        total.pending += outcome.pending;
        total.already_registered += outcome.already_registered;
        total.invalid += outcome.invalid;
    }
    if total.pending > 0 {
        schedule_socket_discovery_retry(
            internal_tx,
            BRIDGE_DISCOVERY_RETRY_ATTEMPTS,
            socket_discovery_retry_scheduled,
        );
    }
    total
}

fn authority_socket_ready_reply(
    node_id: &str,
    id_file: &Path,
    outcome: BridgeRefreshOutcome,
) -> AuthoritySocketReadyReply {
    if outcome.connected > 0 || outcome.already_registered > 0 {
        return AuthoritySocketReadyReply {
            status: AuthoritySocketReadyStatus::Registered,
            message: "registered".to_string(),
        };
    }
    if outcome.pending > 0 {
        return AuthoritySocketReadyReply {
            status: AuthoritySocketReadyStatus::Pending,
            message: format!(
                "authority socket bridge for node {node_id} is pending: {}",
                id_file.display()
            ),
        };
    }
    AuthoritySocketReadyReply {
        status: AuthoritySocketReadyStatus::Error,
        message: format!(
            "authority socket bridge for node {node_id} was not registered: {}",
            id_file.display()
        ),
    }
}

fn refresh_authority_bridges_for_sessions(
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    internal_tx: mpsc::Sender<InternalEvent>,
    retry_budget: u8,
    socket_discovery_retry_scheduled: &mut bool,
) {
    let mut pending = 0usize;
    for active in sessions.values_mut() {
        let node_id = active.session.node_id().to_string();
        let outcome = refresh_authority_bridges(&node_id, active, internal_tx.clone());
        pending += outcome.pending;
    }
    if pending > 0 {
        schedule_socket_discovery_retry(
            internal_tx,
            retry_budget,
            socket_discovery_retry_scheduled,
        );
    }
}

pub(super) fn schedule_socket_discovery_retry(
    internal_tx: mpsc::Sender<InternalEvent>,
    retry_budget: u8,
    socket_discovery_retry_scheduled: &mut bool,
) {
    if retry_budget == 0 || *socket_discovery_retry_scheduled {
        return;
    }
    *socket_discovery_retry_scheduled = true;
    thread::spawn(move || {
        thread::sleep(BRIDGE_DISCOVERY_RETRY_DELAY);
        let _ = internal_tx.send(InternalEvent::RetrySocketDiscovery {
            attempts_remaining: retry_budget.saturating_sub(1),
        });
    });
}

fn refresh_authority_bridge_path(
    node_id: &str,
    session: &mut ActiveNodeIngressSession,
    endpoint: &RemoteControlAddr,
    id_file: &Path,
    internal_tx: mpsc::Sender<InternalEvent>,
) -> BridgeRefreshOutcome {
    let mut outcome = BridgeRefreshOutcome::default();
    if session.bridges.contains_key(endpoint) {
        outcome.already_registered += 1;
        return outcome;
    }
    let Some(target_component) = id_file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .and_then(|name| extract_target_component(&name, node_id))
    else {
        outcome.invalid += 1;
        return outcome;
    };
    let transport = match RemoteAuthorityTransportRuntime::connect(endpoint, node_id) {
        Ok(transport) => transport,
        Err(_) => {
            outcome.pending += 1;
            return outcome;
        }
    };
    let transport = Arc::new(transport);
    spawn_authority_bridge_reader(
        node_id.to_string(),
        session.session.session_instance_id().to_string(),
        endpoint.clone(),
        transport.clone(),
        internal_tx,
    );
    session.bridges.insert(
        endpoint.clone(),
        ActiveAuthoritySocketBridge {
            target_component,
            transport,
        },
    );
    outcome.connected += 1;
    ERROR_LOG.log(format!(
        "[remote-node-ingress] registered authority bridge for node={node_id} endpoint={endpoint}"
    ));
    outcome
}

fn refresh_authority_bridges(
    node_id: &str,
    session: &mut ActiveNodeIngressSession,
    internal_tx: mpsc::Sender<InternalEvent>,
) -> BridgeRefreshOutcome {
    let mut outcome = BridgeRefreshOutcome::default();
    let Ok(endpoints) = discover_authority_endpoints(node_id) else {
        return outcome;
    };
    for (endpoint, id_file) in endpoints {
        if session.bridges.contains_key(&endpoint) {
            continue;
        }
        let Some(target_component) = id_file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .and_then(|name| extract_target_component(&name, node_id))
        else {
            continue;
        };
        let transport = match RemoteAuthorityTransportRuntime::connect(&endpoint, node_id) {
            Ok(transport) => transport,
            Err(_) => {
                outcome.pending += 1;
                continue;
            }
        };
        let transport = Arc::new(transport);
        spawn_authority_bridge_reader(
            node_id.to_string(),
            session.session.session_instance_id().to_string(),
            endpoint.clone(),
            transport.clone(),
            internal_tx.clone(),
        );
        session.bridges.insert(
            endpoint,
            ActiveAuthoritySocketBridge {
                target_component,
                transport,
            },
        );
        outcome.connected += 1;
    }
    if outcome.connected > 0 {
        ERROR_LOG.log(format!(
            "[remote-node-ingress] registered {} authority bridge(s) for node={node_id}",
            outcome.connected
        ));
    }
    outcome
}

fn spawn_authority_bridge_reader(
    node_id: String,
    session_instance_id: String,
    endpoint: RemoteControlAddr,
    reader: Arc<RemoteAuthorityTransportRuntime>,
    internal_tx: mpsc::Sender<InternalEvent>,
) {
    thread::spawn(move || {
        while let Ok(command) = reader.recv_command() {
            ERROR_LOG.log(format!(
                "[remote-node-ingress] bridge reader recv_command node={node_id} session_instance_id={session_instance_id} endpoint={endpoint} command={command:?}",
            ));
            if internal_tx
                .send(InternalEvent::AuthorityCommandReceived {
                    node_id: node_id.clone(),
                    session_instance_id: session_instance_id.clone(),
                    endpoint: endpoint.clone(),
                    command,
                })
                .is_err()
            {
                return;
            }
        }
        let _ = internal_tx.send(InternalEvent::BridgeClosed { node_id, endpoint });
    });
}

fn extract_target_component(file_name: &str, authority_id: &str) -> Option<String> {
    let trimmed = file_name
        .trim_end_matches(".sock")
        .trim_end_matches(".port");
    let authority_hash = stable_socket_hash(&[authority_id]);
    let mut parts = trimmed.rsplitn(3, '-');
    let target_hash = parts.next()?;
    let encoded_authority_hash = parts.next()?;
    let _prefix = parts.next()?;
    if encoded_authority_hash != authority_hash {
        return None;
    }
    Some(target_hash.to_string())
}

fn map_authority_command_to_grpc(
    session: &RemoteNodeSessionHandle,
    command: RemoteAuthorityCommand,
) -> Result<GrpcNodeSessionEnvelope, io::Error> {
    let (route, body) = match command {
        RemoteAuthorityCommand::RawPtyInput(payload) => (
            Some(RouteContext {
                authority_node_id: Some(session.node_id().to_string()),
                target_id: Some(payload.target_id.clone()),
                attachment_id: Some(payload.attachment_id.clone()),
                console_id: Some(payload.console_id.clone()),
                console_host_id: Some(payload.console_host_id.clone()),
                session_id: Some(payload.session_id.clone()),
            }),
            Some(Body::RawPtyInput(RawPtyInput {
                attachment_id: payload.attachment_id,
                target_id: payload.target_id,
                console_id: payload.console_id,
                console_host_id: payload.console_host_id,
                input_seq: payload.input_seq,
                session_id: payload.session_id,
                input_bytes: payload.input_bytes,
            })),
        ),
        RemoteAuthorityCommand::PasteFile(payload) => (
            Some(RouteContext {
                authority_node_id: Some(session.node_id().to_string()),
                target_id: Some(payload.target_id.clone()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some(payload.session_id.clone()),
            }),
            Some(Body::PasteFileRequest(PasteFileRequest {
                target_id: payload.target_id,
                session_id: payload.session_id,
                filename_hint: payload.filename_hint,
                file_id: payload.file_id,
                total_chunks: payload.total_chunks,
                chunk_index: payload.chunk_index,
                chunk_bytes: payload.chunk_bytes,
            })),
        ),
        RemoteAuthorityCommand::ApplyResize(payload) => (
            Some(RouteContext {
                authority_node_id: Some(session.node_id().to_string()),
                target_id: Some(payload.target_id.clone()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some(payload.session_id.clone()),
            }),
            Some(Body::ApplyPtyResize(ApplyPtyResize {
                target_id: payload.target_id,
                resize_epoch: payload.resize_epoch,
                resize_authority_console_id: payload.resize_authority_console_id,
                cols: payload.cols as u32,
                rows: payload.rows as u32,
                session_id: payload.session_id,
            })),
        ),
        RemoteAuthorityCommand::OpenMirror(payload) => (
            Some(RouteContext {
                authority_node_id: Some(session.node_id().to_string()),
                target_id: Some(payload.target_id.clone()),
                attachment_id: None,
                console_id: Some(payload.console_id.clone()),
                console_host_id: None,
                session_id: Some(payload.session_id.clone()),
            }),
            Some(Body::OpenMirrorRequest(OpenMirrorRequest {
                target_id: payload.target_id,
                session_id: payload.session_id,
                console_id: payload.console_id,
                cols: payload.cols as u32,
                rows: payload.rows as u32,
                raw_pty_passthrough: payload.raw_pty_passthrough,
                bootstrap_mode_visible_only: matches!(
                    payload.bootstrap_mode,
                    BootstrapMode::VisibleOnly
                ),
            })),
        ),
        RemoteAuthorityCommand::CloseMirror(payload) => (
            Some(RouteContext {
                authority_node_id: Some(session.node_id().to_string()),
                target_id: Some(payload.target_id.clone()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some(payload.session_id.clone()),
            }),
            Some(Body::CloseMirrorRequest(CloseMirrorRequest {
                target_id: payload.target_id,
                session_id: payload.session_id,
            })),
        ),
        RemoteAuthorityCommand::SyncRequest { .. } | RemoteAuthorityCommand::HeartbeatPing => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sync request/heartbeat is local to authority transport and cannot be mapped to gRPC",
            ));
        }
    };

    Ok(GrpcNodeSessionEnvelope {
        message_id: format!("{}-authority-{}", session.node_id(), now_millis()),
        sent_at: None,
        session_instance_id: session.session_instance_id().to_string(),
        correlation_id: None,
        route,
        body,
    })
}

fn map_target_published_envelope(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &GrpcTargetPublished,
) -> Result<ProtocolEnvelope<ControlPlanePayload>, io::Error> {
    Ok(ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: envelope.message_id.clone(),
        message_type: "target_published",
        timestamp: timestamp_string(envelope),
        sender_id: node_id.to_string(),
        correlation_id: envelope.correlation_id.clone(),
        session_id: route_session_id(envelope)
            .or_else(|| derive_session_id_from_target_id(&payload.target_id)),
        target_id: route_target_id(envelope).or_else(|| Some(payload.target_id.clone())),
        attachment_id: route_attachment_id(envelope),
        console_id: route_console_id(envelope),
        payload: ControlPlanePayload::TargetPublished(TargetPublishedPayload {
            transport_session_id: payload.transport_session_id.clone(),
            node_instance_id: payload.node_instance_id.clone(),
            revision: payload.revision,
            authority_host_session_name: None,
            selector: payload.selector.clone(),
            availability: known_availability(&payload.availability)?,
            session_role: payload
                .session_role
                .as_deref()
                .and_then(crate::domain::workspace::WorkspaceSessionRole::parse)
                .map(|role| role.as_str()),
            workspace_key: payload.workspace_key.clone(),
            command_name: payload.command_name.clone(),
            display_command_name: payload.display_command_name.clone(),
            current_path: payload.current_path.clone(),
            attached_clients: payload.attached_count.unwrap_or(0) as usize,
            window_count: payload.window_count.unwrap_or(0) as usize,
            task_state: payload
                .task_state
                .as_deref()
                .and_then(crate::domain::session_catalog::ManagedSessionTaskState::parse)
                .unwrap_or(crate::domain::session_catalog::ManagedSessionTaskState::Unknown)
                .as_str(),
        }),
    })
}

fn map_target_exited_envelope(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &GrpcTargetExited,
) -> ProtocolEnvelope<ControlPlanePayload> {
    ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: envelope.message_id.clone(),
        message_type: "target_exited",
        timestamp: timestamp_string(envelope),
        sender_id: node_id.to_string(),
        correlation_id: envelope.correlation_id.clone(),
        session_id: route_session_id(envelope)
            .or_else(|| derive_session_id_from_target_id(&payload.target_id)),
        target_id: route_target_id(envelope).or_else(|| Some(payload.target_id.clone())),
        attachment_id: route_attachment_id(envelope),
        console_id: route_console_id(envelope),
        payload: ControlPlanePayload::TargetExited(TargetExitedPayload {
            transport_session_id: payload.transport_session_id.clone(),
            node_instance_id: payload.node_instance_id.clone(),
            revision: payload.revision,
            authority_host_session_name: None,
        }),
    }
}

fn map_target_publication_ack_envelope(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &GrpcTargetPublicationAck,
) -> Result<ProtocolEnvelope<ControlPlanePayload>, io::Error> {
    Ok(ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: envelope.message_id.clone(),
        message_type: "target_publication_ack",
        timestamp: timestamp_string(envelope),
        sender_id: node_id.to_string(),
        correlation_id: envelope.correlation_id.clone(),
        session_id: route_session_id(envelope),
        target_id: route_target_id(envelope).or_else(|| Some(payload.target_id.clone())),
        attachment_id: route_attachment_id(envelope),
        console_id: route_console_id(envelope),
        payload: ControlPlanePayload::TargetPublicationAck(TargetPublicationAckPayload {
            node_id: payload.node_id.clone(),
            node_instance_id: payload.node_instance_id.clone(),
            target_id: payload.target_id.clone(),
            revision: payload.revision,
            status: target_publication_ack_status(payload.status())?,
            message: payload.message.clone(),
        }),
    })
}

fn target_publication_ack_status(
    status: GrpcTargetPublicationAckStatus,
) -> Result<TargetPublicationAckStatus, io::Error> {
    match status {
        GrpcTargetPublicationAckStatus::Applied => Ok(TargetPublicationAckStatus::Applied),
        GrpcTargetPublicationAckStatus::StaleRevision => {
            Ok(TargetPublicationAckStatus::StaleRevision)
        }
        GrpcTargetPublicationAckStatus::Failed => Ok(TargetPublicationAckStatus::Failed),
        GrpcTargetPublicationAckStatus::Unspecified => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unspecified target publication ack status",
        )),
    }
}

fn known_output_stream(stream: &str) -> Result<&'static str, io::Error> {
    match stream {
        "pty" => Ok("pty"),
        "stdout" => Ok("stdout"),
        "stderr" => Ok("stderr"),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported grpc target output stream `{other}`"),
        )),
    }
}

fn known_availability(value: &str) -> Result<&'static str, io::Error> {
    match value {
        "online" => Ok("online"),
        "offline" => Ok("offline"),
        "exited" => Ok("exited"),
        "unknown" => Ok("unknown"),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported grpc target availability `{other}`"),
        )),
    }
}

fn route_target_id(envelope: &GrpcNodeSessionEnvelope) -> Option<String> {
    envelope
        .route
        .as_ref()
        .and_then(|route| route.target_id.clone())
}

fn route_session_id(envelope: &GrpcNodeSessionEnvelope) -> Option<String> {
    envelope
        .route
        .as_ref()
        .and_then(|route| route.session_id.clone())
}

fn route_attachment_id(envelope: &GrpcNodeSessionEnvelope) -> Option<String> {
    envelope
        .route
        .as_ref()
        .and_then(|route| route.attachment_id.clone())
}

fn route_console_id(envelope: &GrpcNodeSessionEnvelope) -> Option<String> {
    envelope
        .route
        .as_ref()
        .and_then(|route| route.console_id.clone())
}

fn payload_session_id(payload_session_id: &str, target_id: &str) -> Option<String> {
    if !payload_session_id.is_empty() {
        Some(payload_session_id.to_string())
    } else {
        derive_session_id_from_target_id(target_id)
    }
}

fn derive_session_id_from_target_id(target_id: &str) -> Option<String> {
    let target_id = target_id
        .strip_prefix("remote-peer:")
        .or_else(|| target_id.strip_prefix("local-tmux:"))
        .or_else(|| target_id.strip_prefix("remote:"))
        .unwrap_or(target_id);
    let (_, session_id) = target_id.rsplit_once(':')?;
    if session_id.is_empty() {
        None
    } else {
        Some(session_id.to_string())
    }
}

fn timestamp_string(envelope: &GrpcNodeSessionEnvelope) -> String {
    if let Some(timestamp) = envelope.sent_at.as_ref() {
        return format!("{}.{:09}Z", timestamp.seconds, timestamp.nanos.max(0));
    }
    format!("{}Z", now_millis())
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn remote_node_ingress_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to run remote node ingress server".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
