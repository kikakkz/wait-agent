// Legacy tmux-era session-sync runtime kept during the ratatui migration; most items are currently unused.

use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, OutboundNodeSessionRequest,
    RemoteNodeSessionHandle, RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::lifecycle::LifecycleError;
use crate::platform::remote_ipc::{
    cleanup_remote_listener, remote_ready_addr, remote_session_sync_owner_addr,
    remote_session_sync_startup_lock_path, RemoteControlAddr, RemoteControlListener,
    RemoteControlStream,
};
use crate::process::current_executable::current_waitagent_executable;
use crate::process::startup_lock::StartupLock;
use crate::process::workspace::sidecar_process_runtime::{
    spawn_waitagent_sidecar, spawn_waitagent_sidecar_child,
};
use crate::remote::authority::remote_authority_target_host_runtime::{
    wait_for_ready_socket, RemoteAuthorityPublicationGateway,
};
use crate::remote::authority::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::remote::node::remote_node_session_runtime::GrpcAuthorityEvent;
use crate::remote::publication::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Duration;

mod sync_helpers;
pub(crate) use sync_helpers::*;

mod local_session_sync_backends;
pub(crate) use local_session_sync_backends::*;

const SESSION_SYNC_RECONNECT_DELAY: Duration = Duration::from_millis(500);

pub(super) const SESSION_SYNC_AUTHORITY_ID: &str = "waitagent-session-sync-authority";
// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
pub(super) const LIVE_AUTHORITY_SERVER_ID: &str = "waitagent-live-authority-owner";

pub trait LocalSessionCatalog: Send + 'static {
    type Error: ToString;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error>;

    fn local_target_socket_name(&self) -> Option<&str> {
        None
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

pub trait LocalTargetExitObserver: Clone + Send + 'static {
    fn observe_local_target_exit(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;
}

#[derive(Clone)]
pub struct SidecarLocalTargetExitObserver {
    network: RemoteNetworkConfig,
    current_executable: PathBuf,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl SidecarLocalTargetExitObserver {
    pub fn from_build_env(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        Ok(Self {
            network,
            current_executable: current_waitagent_executable()?,
        })
    }
}

impl LocalTargetExitObserver for SidecarLocalTargetExitObserver {
    fn observe_local_target_exit(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        let args = prepend_global_network_args(
            vec![
                "__local-target-exited".to_string(),
                "--socket-name".to_string(),
                socket_name.to_string(),
                "--target-session-name".to_string(),
                target_session_name.to_string(),
                "--pane-id".to_string(),
                String::new(),
            ],
            &self.network,
        );
        spawn_waitagent_sidecar(&self.current_executable, args).map_err(remote_session_sync_error)
    }
}

pub struct RemoteNodeSessionSyncRuntime<
    G: LocalSessionCatalog = RatatuiLocalSessionCatalog,
    T: OutboundRemoteNodeTransport = GrpcRemoteNodeTransport,
    O: LocalTargetExitObserver = RatatuiLocalTargetExitObserver,
    F: LocalTargetFactory = RatatuiLocalTargetFactory,
    A: LocalAuthorityHostBackend = RatatuiLocalAuthorityHostBackend,
    P: crate::remote::publication::remote_target_publication_backend::RemoteTargetPublicationBackend = crate::remote::publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend,
> {
    gateway: G,
    transport: T,
    local_target_exit_observer: O,
    target_factory: F,
    authority_backend: A,
    publication_runtime: Option<RemoteTargetPublicationRuntime<P>>,
    network: RemoteNetworkConfig,
    reconnect_delay: Duration,
}

pub struct RemoteNodeSessionSyncGuard {
    stop_tx: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

pub(crate) struct SessionSyncAuthorityManager<F: LocalTargetFactory, A: LocalAuthorityHostBackend> {
    pub(crate) running_hosts: HashMap<String, SessionSyncAuthorityHost>,
    output_route: SessionSyncAuthorityOutputRoute,
    target_factory: F,
    authority_backend: A,
}

pub(crate) struct SessionSyncAuthorityHost {
    pub(crate) writer: Arc<Mutex<Option<RemoteControlStream>>>,
    pub(crate) running: Arc<AtomicBool>,
    pub(crate) writer_ready: Arc<Condvar>,
    pub(crate) bound_session_instance_id: String,
    /// The bridge forwards authority output using this session id. It is updated
    /// when a new inbound gRPC session takes over the same target, so the
    /// existing target host and bridge survive transient reconnects.
    pub(crate) bridge_session_id: Arc<RwLock<String>>,
}

#[derive(Clone)]
pub(crate) enum SessionSyncAuthorityOutputRoute {
    OwnerEvent(mpsc::Sender<SessionSyncEvent>),
    IngressEvent(
        mpsc::Sender<crate::remote::node::remote_node_ingress_server_runtime::InternalEvent>,
    ),
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
#[derive(Clone)]
pub(super) struct SessionSyncAuthorityPublicationGateway {
    network: RemoteNetworkConfig,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl SessionSyncAuthorityPublicationGateway {
    pub(super) fn new(network: RemoteNetworkConfig) -> Self {
        Self { network }
    }
}

impl RemoteAuthorityPublicationGateway for SessionSyncAuthorityPublicationGateway {
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        target_id: &str,
        transport_socket_path: &str,
        authority_socket_path: &std::path::Path,
    ) -> Result<(), LifecycleError> {
        use crate::remote::publication::remote_target_publication_runtime::{
            signal_publication_sender_live_session_registered, RemoteTargetPublicationRuntime,
        };
        let network = self.network.clone();
        let shared = crate::ratatui_node::SharedState::new(network.clone())?;
        let backend =
            crate::remote::publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend::new(
                shared,
                network.clone(),
            );
        let runtime = RemoteTargetPublicationRuntime::with_network_and_backend(network, backend)?;
        runtime.ensure_publication_sender_running(socket_name)?;
        signal_publication_sender_live_session_registered(
            socket_name,
            target_session_name,
            authority_id,
            target_id,
            transport_socket_path,
        )?;
        wait_for_ready_socket(authority_socket_path)
    }

    fn ensure_live_session_unregistered(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        crate::remote::publication::remote_target_publication_runtime::signal_publication_sender_live_session_unregistered(
            socket_name,
            target_session_name,
        )
    }

    fn signal_source_session_closed(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        use crate::remote::publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend;
        use crate::remote::publication::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
        let network = self.network.clone();
        let shared = crate::ratatui_node::SharedState::new(network.clone())?;
        let backend = RatatuiRemoteTargetPublicationBackend::new(shared, network.clone());
        let runtime = RemoteTargetPublicationRuntime::with_network_and_backend(network, backend)?;
        runtime.signal_source_session_closed(socket_name, target_session_name)
    }

    fn signal_local_runtime_changed(&self, socket_name: &str) -> Result<(), LifecycleError> {
        use crate::remote::publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend;
        use crate::remote::publication::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
        let network = self.network.clone();
        let shared = crate::ratatui_node::SharedState::new(network.clone())?;
        let backend = RatatuiRemoteTargetPublicationBackend::new(shared, network.clone());
        let runtime = RemoteTargetPublicationRuntime::with_network_and_backend(network, backend)?;
        runtime.signal_local_runtime_changed(socket_name)
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl RemoteNodeSessionSyncRuntime {
    pub fn ensure_owner_running(
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let _t_owner = std::time::Instant::now();
        let addr = remote_session_sync_owner_addr(socket_name);
        if remote_session_sync_owner_available(&addr) {
            return Ok(());
        }
        let lock_path = remote_session_sync_startup_lock_path(socket_name);
        let Some(_startup_lock) =
            StartupLock::try_acquire(&lock_path).map_err(remote_session_sync_error)?
        else {
            let _startup_lock =
                StartupLock::acquire(&lock_path).map_err(remote_session_sync_error)?;
            if remote_session_sync_owner_available(&addr) {
                return Ok(());
            }
            return Err(LifecycleError::Protocol(format!(
                "remote session sync owner for socket `{socket_name}` was not ready after startup lock {} released",
                lock_path.display()
            )));
        };
        if remote_session_sync_owner_available(&addr) {
            return Ok(());
        }
        cleanup_remote_listener(&addr);
        let current_executable = current_waitagent_executable()?;

        let ready_addr = remote_ready_addr();
        let ready_listener =
            RemoteControlListener::bind(&ready_addr).map_err(remote_session_sync_error)?;
        // On Windows the listener resolves the ephemeral port; the child must be
        // handed the concrete address, not the pre-bind `127.0.0.1:0`.
        let bound_ready_addr = ready_listener.local_addr().clone();
        let child = spawn_waitagent_sidecar_child(
            &current_executable,
            remote_session_sync_owner_args(socket_name, network, Some(&bound_ready_addr)),
        )
        .map_err(remote_session_sync_error)?;

        let ready =
            wait_for_remote_session_sync_owner_ready(ready_listener, &bound_ready_addr, child);
        cleanup_remote_listener(&bound_ready_addr);
        ready?;

        Ok(())
    }

    pub fn notify_local_catalog_changed(
        socket_name: &str,
        network: &RemoteNetworkConfig,
        reason: LocalCatalogChangeReason,
    ) -> Result<(), LifecycleError> {
        if network.connect_endpoint_uri().is_none() {
            return Ok(());
        }
        let addr = remote_session_sync_owner_addr(socket_name);
        match notify_remote_session_sync_owner(&addr, reason.clone()) {
            Ok(()) => Ok(()),
            Err(first_error) => {
                ERROR_LOG.log_error(format!(
                    "session_sync_notify retry socket={} reason={} first_error={}",
                    socket_name,
                    reason.as_str(),
                    first_error
                ));
                Self::ensure_owner_running(socket_name, network)?;
                notify_remote_session_sync_owner(&addr, reason)
            }
        }
    }

    pub fn signal_local_catalog_changed(
        socket_name: &str,
        network: &RemoteNetworkConfig,
        reason: LocalCatalogChangeReason,
    ) -> Result<(), LifecycleError> {
        if network.connect_endpoint_uri().is_none() {
            return Ok(());
        }
        let addr = remote_session_sync_owner_addr(socket_name);
        match signal_remote_session_sync_owner(&addr, reason.clone()) {
            Ok(()) => Ok(()),
            Err(first_error) => {
                ERROR_LOG.log_error(format!(
                    "session_sync_signal retry socket={} reason={} first_error={}",
                    socket_name,
                    reason.as_str(),
                    first_error
                ));
                Self::ensure_owner_running(socket_name, network)?;
                signal_remote_session_sync_owner(&addr, reason)
            }
        }
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn notify_remote_session_sync_owner_ready(
    ready_socket: Option<&str>,
    result: Result<(), String>,
) -> io::Result<()> {
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

fn wait_for_remote_session_sync_owner_ready(
    listener: RemoteControlListener,
    ready_addr: &RemoteControlAddr,
    mut child: std::process::Child,
) -> Result<(), LifecycleError> {
    enum SessionSyncOwnerReadyEvent {
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
        let _ = ready_tx.send(SessionSyncOwnerReadyEvent::Ready(response));
    });

    thread::spawn(move || {
        let status = child.wait();
        let _ = event_tx.send(SessionSyncOwnerReadyEvent::Exited(status));
    });

    match event_rx.recv() {
        Ok(SessionSyncOwnerReadyEvent::Ready(Ok(response))) => {
            let response = response.trim();
            if response == "ok" {
                return Ok(());
            }
            if let Some(error) = response.strip_prefix("err\t") {
                return Err(LifecycleError::Protocol(format!(
                    "remote session sync owner failed to start: {error}"
                )));
            }
            Err(LifecycleError::Protocol(format!(
                "remote session sync owner sent invalid ready response `{response}`"
            )))
        }
        Ok(SessionSyncOwnerReadyEvent::Ready(Err(error))) => Err(remote_session_sync_error(error)),
        Ok(SessionSyncOwnerReadyEvent::Exited(Ok(status))) => Err(LifecycleError::Protocol(
            format!("remote session sync owner exited before reporting ready: {status}"),
        )),
        Ok(SessionSyncOwnerReadyEvent::Exited(Err(error))) => Err(remote_session_sync_error(error)),
        Err(_) => Err(LifecycleError::Protocol(format!(
            "remote session sync owner ready socket `{}` closed before reporting ready",
            ready_addr.to_arg_string()
        ))),
    }
}

impl<G, T, O, F, A, P> RemoteNodeSessionSyncRuntime<G, T, O, F, A, P>
where
    G: LocalSessionCatalog,
    T: OutboundRemoteNodeTransport,
    O: LocalTargetExitObserver,
    F: LocalTargetFactory,
    A: LocalAuthorityHostBackend,
    P: crate::remote::publication::remote_target_publication_backend::RemoteTargetPublicationBackend,
    LifecycleError: From<<A as LocalAuthorityHostBackend>::Error>,
{
    pub fn new_with_backends(
        gateway: G,
        transport: T,
        local_target_exit_observer: O,
        target_factory: F,
        authority_backend: A,
        publication_runtime: Option<RemoteTargetPublicationRuntime<P>>,
        network: RemoteNetworkConfig,
    ) -> Self {
        Self {
            gateway,
            transport,
            local_target_exit_observer,
            target_factory,
            authority_backend,
            publication_runtime,
            network,
            reconnect_delay: SESSION_SYNC_RECONNECT_DELAY,
        }
    }

    pub fn start_with_local_catalog_changes(
        self,
        local_catalog_rx: mpsc::Receiver<LocalCatalogChangeRequest>,
    ) -> Result<RemoteNodeSessionSyncGuard, LifecycleError> {
        let endpoint_uri = self.network.connect_endpoint_uri().ok_or_else(|| {
            LifecycleError::Protocol("remote session sync requires `--connect`".to_string())
        })?;
        let node_id = self.network.advertised_node_id();
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_remote_session_sync_loop(
                RunRemoteSessionSyncLoopArgs {
                    gateway: self.gateway,
                    transport: self.transport,
                    network: self.network,
                    local_target_exit_observer: self.local_target_exit_observer,
                    target_factory: self.target_factory,
                    authority_backend: self.authority_backend,
                    publication_runtime: self.publication_runtime,
                    node_id,
                    endpoint_uri,
                    local_catalog_rx,
                    reconnect_delay: self.reconnect_delay,
                },
                stop_rx,
            );
        });
        Ok(RemoteNodeSessionSyncGuard {
            stop_tx: Some(stop_tx),
            worker: Some(worker),
        })
    }
}

fn create_session_reply_envelope(
    node_id: &str,
    correlation_id: Option<&str>,
    payload: crate::infra::remote_protocol::ControlPlanePayload,
) -> crate::infra::remote_protocol::ProtocolEnvelope<
    crate::infra::remote_protocol::ControlPlanePayload,
> {
    crate::infra::remote_protocol::ProtocolEnvelope {
        protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: format!("{node_id}-create-session-reply-{}", sync_now_millis()),
        message_type: payload.message_type(),
        timestamp: format!("{}Z", sync_now_millis()),
        sender_id: node_id.to_string(),
        correlation_id: correlation_id.map(str::to_string),
        session_id: match &payload {
            crate::infra::remote_protocol::ControlPlanePayload::CreateSessionAccepted(accepted) => {
                Some(accepted.session_id.clone())
            }
            _ => None,
        },
        target_id: match &payload {
            crate::infra::remote_protocol::ControlPlanePayload::CreateSessionAccepted(accepted) => {
                Some(accepted.target_id.clone())
            }
            _ => None,
        },
        attachment_id: None,
        console_id: None,
        payload,
    }
}

fn sync_now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

impl<F, A> SessionSyncAuthorityManager<F, A>
where
    F: LocalTargetFactory,
    A: LocalAuthorityHostBackend,
    LifecycleError: From<<A as LocalAuthorityHostBackend>::Error>,
{
    pub(super) fn with_ingress_events(
        _network: RemoteNetworkConfig,
        _local_target_socket_name: Option<String>,
        ingress_event_tx: mpsc::Sender<
            crate::remote::node::remote_node_ingress_server_runtime::InternalEvent,
        >,
        target_factory: F,
        authority_backend: A,
    ) -> Self {
        Self {
            running_hosts: HashMap::new(),
            output_route: SessionSyncAuthorityOutputRoute::IngressEvent(ingress_event_tx),
            target_factory,
            authority_backend,
        }
    }

    fn with_session_events(
        _network: RemoteNetworkConfig,
        _local_target_socket_name: Option<String>,
        session_event_tx: mpsc::Sender<SessionSyncEvent>,
        target_factory: F,
        authority_backend: A,
    ) -> Self {
        Self {
            running_hosts: HashMap::new(),
            output_route: SessionSyncAuthorityOutputRoute::OwnerEvent(session_event_tx),
            target_factory,
            authority_backend,
        }
    }

    pub(super) fn shutdown(&mut self) {
        for (_, host) in self.running_hosts.drain() {
            self.authority_backend.shutdown_authority_host(&host);
        }
    }

    pub(super) fn handle_event(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        event: GrpcAuthorityEvent,
    ) -> bool {
        match event {
            GrpcAuthorityEvent::Command(command) => {
                if let Err(error) = self.ensure_and_send_command(session_handle, command) {
                    ERROR_LOG.log(format!(
                        "[session-sync] failed to handle authority command: {error}"
                    ));
                }
                false
            }
            GrpcAuthorityEvent::CreateSessionRequest {
                payload,
                correlation_id,
            } => match self.handle_create_session_request(
                session_handle,
                payload,
                correlation_id.as_deref(),
            ) {
                Ok(()) => true,
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[session-sync] failed to handle create-session request: {error}"
                    ));
                    false
                }
            },
            GrpcAuthorityEvent::CreateSessionAccepted(_)
            | GrpcAuthorityEvent::CreateSessionRejected(_)
            | GrpcAuthorityEvent::TargetPublicationAck(_)
            | GrpcAuthorityEvent::MirrorAccepted
            | GrpcAuthorityEvent::MirrorRejected(_)
            | GrpcAuthorityEvent::Failed(_)
            | GrpcAuthorityEvent::Closed => false,
        }
    }

    fn handle_create_session_request(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        payload: crate::infra::remote_protocol::CreateSessionRequestPayload,
        correlation_id: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let started = std::time::Instant::now();
        ERROR_LOG.log(format!(
            "[session-sync] create-session request id={} node={} correlation={:?}",
            payload.request_id,
            session_handle.node_id(),
            correlation_id
        ));

        let result = self.create_local_target_for_create_session(session_handle, &payload);
        ERROR_LOG.log(format!(
            "[session-sync] create-session result id={} result={result:?} elapsed={:?}",
            payload.request_id,
            started.elapsed()
        ));

        match result {
            Ok(created) => session_handle
                .send(crate::remote::node::remote_node_session_runtime::map_outbound_grpc_envelope(
                    session_handle.node_id(),
                    crate::infra::remote_protocol::NodeSessionChannel::Authority,
                    &create_session_reply_envelope(
                        session_handle.node_id(),
                        correlation_id,
                        crate::infra::remote_protocol::ControlPlanePayload::CreateSessionAccepted(
                            crate::infra::remote_protocol::CreateSessionAcceptedPayload {
                                request_id: payload.request_id.clone(),
                                session_id: created.session_id,
                                target_id: created.target_id,
                            },
                        ),
                    ),
                )
                .map_err(remote_session_sync_error)?)
                .map_err(remote_session_sync_error),
            Err(error) => session_handle
                .send(crate::remote::node::remote_node_session_runtime::map_outbound_grpc_envelope(
                    session_handle.node_id(),
                    crate::infra::remote_protocol::NodeSessionChannel::Authority,
                    &create_session_reply_envelope(
                        session_handle.node_id(),
                        correlation_id,
                        crate::infra::remote_protocol::ControlPlanePayload::CreateSessionRejected(
                            crate::infra::remote_protocol::CreateSessionRejectedPayload {
                                request_id: payload.request_id.clone(),
                                code: "create_session_failed",
                                message: error.to_string(),
                            },
                        ),
                    ),
                )
                .map_err(remote_session_sync_error)?)
                .map_err(remote_session_sync_error),
        }
    }

    fn create_local_target_for_create_session(
        &self,
        session_handle: &RemoteNodeSessionHandle,
        payload: &crate::infra::remote_protocol::CreateSessionRequestPayload,
    ) -> Result<CreatedLocalTarget, LifecycleError> {
        if payload.authority_node_id != session_handle.node_id() {
            return Err(LifecycleError::Protocol(format!(
                "create-session request for authority `{}` reached `{}`",
                payload.authority_node_id,
                session_handle.node_id()
            )));
        }
        let cwd = payload
            .cwd_hint
            .as_deref()
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let cols = u16::try_from(payload.cols)
            .ok()
            .filter(|cols| *cols > 0)
            .unwrap_or(80);
        let rows = u16::try_from(payload.rows)
            .ok()
            .filter(|rows| *rows > 0)
            .unwrap_or(24);
        self.target_factory
            .create_local_target(session_handle.node_id(), &cwd, cols, rows)
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to create local target".to_string(),
                    io::Error::new(io::ErrorKind::Other, error.to_string()),
                )
            })
    }

    fn ensure_authority_host(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        target_id: &str,
    ) -> Result<(), LifecycleError> {
        let bound_session_instance_id = session_handle.session_instance_id().to_string();
        let should_remove_stale = if let Some(existing) = self.running_hosts.get(target_id) {
            if existing.running.load(Ordering::Relaxed) {
                // A healthy authority target host is already running for this
                // target. Reuse it: just point the bridge at the new inbound
                // gRPC session id so output is forwarded to the current client
                // session instead of tearing the host down.
                if let Some(existing) = self.running_hosts.get_mut(target_id) {
                    existing.bound_session_instance_id = bound_session_instance_id.clone();
                    if let Ok(mut guard) = existing.bridge_session_id.write() {
                        *guard = bound_session_instance_id;
                    }
                }
                return Ok(());
            }
            // Host has already exited; drop the stale entry so a new one can be
            // created below.
            true
        } else {
            false
        };
        if should_remove_stale {
            self.running_hosts.remove(target_id);
        }

        let host = self.authority_backend.spawn_authority_host(
            session_handle,
            target_id,
            self.output_route.clone(),
        )?;
        self.running_hosts.insert(target_id.to_string(), host);
        Ok(())
    }

    fn ensure_and_send_command(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        command: RemoteAuthorityCommand,
    ) -> Result<(), LifecycleError> {
        let target_id = authority_command_target_id(&command).to_string();
        // Always run the authority-host binding check so a stale host bound to a dead
        // gRPC session is replaced before we deliver the command.
        if !target_id.is_empty() {
            self.ensure_authority_host(session_handle, &target_id)?;
        }
        self.deliver_with_host_rebuild(session_handle, &target_id, command, false)
    }

    fn deliver_with_host_rebuild(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        target_id: &str,
        command: RemoteAuthorityCommand,
        rebuilt: bool,
    ) -> Result<(), LifecycleError> {
        let signal = self
            .running_hosts
            .get(target_id)
            .map(|host| self.authority_backend.authority_host_signal(host))
            .unwrap_or(AuthorityHostSignal::Closed);

        if matches!(signal, AuthorityHostSignal::Closed) {
            if rebuilt {
                return Err(LifecycleError::Protocol(format!(
                    "authority host for `{target_id}` closed before accepting command"
                )));
            }
            self.running_hosts.remove(target_id);
            self.ensure_authority_host(session_handle, target_id)?;
            return self.deliver_with_host_rebuild(session_handle, target_id, command, true);
        }

        let host = self.running_hosts.get(target_id).ok_or_else(|| {
            LifecycleError::Protocol("authority host cache lost entry".to_string())
        })?;
        match self
            .authority_backend
            .deliver_command(host, command.clone())?
        {
            AuthorityHostSignal::Ready => Ok(()),
            AuthorityHostSignal::Starting => Err(LifecycleError::Protocol(format!(
                "authority host for `{target_id}` did not become ready"
            ))),
            AuthorityHostSignal::Closed => {
                if rebuilt {
                    self.running_hosts.remove(target_id);
                    return Err(LifecycleError::Protocol(format!(
                        "authority host for `{target_id}` closed before accepting command"
                    )));
                }
                self.running_hosts.remove(target_id);
                self.ensure_authority_host(session_handle, target_id)?;
                self.deliver_with_host_rebuild(session_handle, target_id, command, true)
            }
        }
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
