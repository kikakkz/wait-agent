use crate::application::target_registry_service::merge_local_targets_by_identity;
use crate::cli::{
    prepend_global_network_args, RemoteAuthorityTargetHostCommand, RemoteNetworkConfig,
    RemoteSessionSyncOwnerCommand,
};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::published_target_store::PublishedTargetStore;
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, OutboundNodeSessionRequest,
    RemoteNodeSessionHandle, RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxChromeGateway, TmuxSocketName};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_authority_target_host_runtime::RemoteAuthorityPublicationGateway;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_session_owner_runtime::live_authority_session_socket_path;
use crate::runtime::remote_node_session_runtime::GrpcAuthorityEvent;
use crate::runtime::sidecar_process_runtime::{
    spawn_waitagent_sidecar, spawn_waitagent_sidecar_child,
};
use crate::runtime::target_host_runtime::TargetHostRuntime;
use std::collections::HashMap;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

mod sync_helpers;
pub(crate) use sync_helpers::*;

const SESSION_SYNC_RECONNECT_DELAY: Duration = Duration::from_millis(500);

pub(super) const SESSION_SYNC_AUTHORITY_ID: &str = "waitagent-session-sync-authority";
pub(super) const LIVE_AUTHORITY_SERVER_ID: &str = "waitagent-live-authority-owner";
pub(super) const WAITAGENT_ACTIVE_TARGET_OPTION: &str = "@waitagent_active_target";

pub trait LocalSessionCatalog: Send + 'static {
    type Error: ToString;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error>;

    fn local_target_socket_name(&self) -> Option<&str> {
        None
    }
}

#[derive(Clone)]
pub struct SocketScopedLocalSessionCatalog<G> {
    gateway: G,
    socket_name: TmuxSocketName,
    published_target_store: PublishedTargetStore,
}

impl<G> SocketScopedLocalSessionCatalog<G> {
    pub fn new(
        gateway: G,
        socket_name: TmuxSocketName,
        published_target_store: PublishedTargetStore,
    ) -> Self {
        Self {
            gateway,
            socket_name,
            published_target_store,
        }
    }
}

impl<G> LocalSessionCatalog for SocketScopedLocalSessionCatalog<G>
where
    G: TmuxChromeGateway + Send + 'static,
    G::Error: ToString,
{
    type Error = G::Error;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        let sessions = self.gateway.list_sessions_on_socket(&self.socket_name)?;
        let pane_backed = self
            .gateway
            .list_local_target_content_pane_sessions(&self.socket_name)?;
        let sessions = merge_local_targets_by_identity(sessions, pane_backed);
        let active_targets =
            active_workspace_targets_on_socket(&self.gateway, &self.socket_name, &sessions)?;
        Ok(exportable_local_sessions_for_socket(
            overlay_workspace_runtime_onto_active_local_target_hosts(
                sessions,
                self.socket_name.as_str(),
                &active_targets,
            ),
            self.socket_name.as_str(),
            &self.published_target_store,
        ))
    }

    fn local_target_socket_name(&self) -> Option<&str> {
        Some(self.socket_name.as_str())
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
    G,
    T = GrpcRemoteNodeTransport,
    O = SidecarLocalTargetExitObserver,
> {
    gateway: G,
    transport: T,
    local_target_exit_observer: O,
    network: RemoteNetworkConfig,
    reconnect_delay: Duration,
}

pub struct RemoteNodeSessionSyncGuard {
    stop_tx: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

pub(super) struct SessionSyncAuthorityManager {
    pub(super) running_hosts: HashMap<String, SessionSyncAuthorityHost>,
    network: RemoteNetworkConfig,
    local_target_socket_name: Option<String>,
    output_route: SessionSyncAuthorityOutputRoute,
}

pub(super) struct SessionSyncAuthorityHost {
    pub(super) writer: Arc<Mutex<Option<UnixStream>>>,
    pub(super) running: Arc<AtomicBool>,
    pub(super) writer_ready: Arc<Condvar>,
}

#[derive(Clone)]
pub(in crate::runtime::remote_node::remote_node_session_sync_runtime) enum SessionSyncAuthorityOutputRoute
{
    OwnerEvent(mpsc::Sender<SessionSyncEvent>),
    IngressEvent(
        mpsc::Sender<
            crate::runtime::remote_node::remote_node_ingress_server_runtime::InternalEvent,
        >,
    ),
}

#[derive(Clone)]
pub(super) struct SessionSyncAuthorityPublicationGateway {
    network: RemoteNetworkConfig,
}

impl SessionSyncAuthorityPublicationGateway {
    pub(super) fn new(network: RemoteNetworkConfig) -> Self {
        Self { network }
    }
}

impl RemoteNodeSessionSyncRuntime<SocketScopedLocalSessionCatalog<EmbeddedTmuxBackend>> {
    pub fn from_build_env_with_network_and_socket(
        network: RemoteNetworkConfig,
        socket_name: &str,
    ) -> Result<Self, LifecycleError> {
        Ok(Self::new_with_local_target_exit_observer(
            SocketScopedLocalSessionCatalog::new(
                EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?,
                TmuxSocketName::new(socket_name),
                PublishedTargetStore::new(std::path::PathBuf::from("/dev/null")),
            ),
            GrpcRemoteNodeTransport::new(),
            SidecarLocalTargetExitObserver::from_build_env(network.clone())?,
            network,
        ))
    }

    pub fn run_owner(
        command: RemoteSessionSyncOwnerCommand,
        network: RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        // Install a file-based panic hook so that panic messages are captured
        // even when stderr goes to a deleted pts (the sidecar's original stderr
        // fd may reference a dead pty). The hook chains to the original hook so
        // stderr output is preserved when it is available.
        let socket_name_for_hook = command.socket_name.clone();
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let diag_path = std::env::temp_dir().join(format!(
                "waitagent-sync-owner-panic-{}.log",
                socket_name_for_hook
                    .chars()
                    .map(|ch| match ch {
                        'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
                        _ => '_',
                    })
                    .collect::<String>()
            ));
            let _ = std::fs::write(
                &diag_path,
                format!("remote session sync owner panicked: {info}\n"),
            );
            original_hook(info);
        }));

        let socket_path = remote_session_sync_owner_socket_path(&command.socket_name);
        let startup = (|| -> Result<
            (
                mpsc::Receiver<()>,
                RemoteNodeSessionSyncGuard,
                thread::JoinHandle<()>,
            ),
            LifecycleError,
        > {
            if socket_path.exists() {
                let _ = fs::remove_file(&socket_path);
            }
            let listener = UnixListener::bind(&socket_path).map_err(remote_session_sync_error)?;
            let (local_catalog_tx, local_catalog_rx) = mpsc::channel();
            let (shutdown_tx, shutdown_rx) = mpsc::channel();
            let owner_socket = listener.try_clone().map_err(remote_session_sync_error)?;
            let owner_command_worker =
                serve_owner_commands(owner_socket, local_catalog_tx, shutdown_tx);
            let guard =
                Self::from_build_env_with_network_and_socket(network, &command.socket_name)?
                    .start_with_local_catalog_changes(local_catalog_rx)?;
            Ok((shutdown_rx, guard, owner_command_worker))
        })();
        let (shutdown_rx, _guard, _owner_command_worker) = match startup {
            Ok(startup) => startup,
            Err(error) => {
                let _ = notify_remote_session_sync_owner_ready(
                    command.ready_socket.as_deref(),
                    Err(error.to_string()),
                );
                return Err(error);
            }
        };
        if let Err(error) =
            notify_remote_session_sync_owner_ready(command.ready_socket.as_deref(), Ok(()))
        {
            ERROR_LOG.log(format!(
                "[diag-newhost] session_sync_owner ready notification failed: {error}"
            ));
        }
        loop {
            if shutdown_rx.recv_timeout(Duration::from_secs(1)).is_ok() {
                break;
            }
            if !backend_socket_still_exists(&command.socket_name) {
                break;
            }
        }
        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    pub fn ensure_owner_running(
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let t_owner = std::time::Instant::now();
        let socket_path = remote_session_sync_owner_socket_path(socket_name);
        if remote_session_sync_owner_available(&socket_path) {
            ERROR_LOG.log(format!(
                "[diag-newhost] ensure_session_sync_owner socket={} already_available elapsed={:?}",
                socket_name,
                t_owner.elapsed()
            ));
            return Ok(());
        }
        let lock_path = owner_startup_lock_path(&socket_path);
        let Some(_startup_lock) = OwnerStartupLock::try_acquire(&lock_path)? else {
            let _startup_lock = OwnerStartupLock::acquire(&lock_path)?;
            if remote_session_sync_owner_available(&socket_path) {
                ERROR_LOG.log(format!(
                    "[diag-newhost] ensure_session_sync_owner socket={} ready_by_peer elapsed={:?}",
                    socket_name,
                    t_owner.elapsed()
                ));
                return Ok(());
            }
            return Err(LifecycleError::Protocol(format!(
                "remote session sync owner for socket `{socket_name}` was not ready after startup lock {} released",
                lock_path.display()
            )));
        };
        if remote_session_sync_owner_available(&socket_path) {
            return Ok(());
        }
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let current_executable = current_waitagent_executable()?;
        let ready_socket = remote_session_sync_owner_ready_socket_path(&socket_path);
        if ready_socket.exists() {
            let _ = fs::remove_file(&ready_socket);
        }
        let ready_listener =
            UnixListener::bind(&ready_socket).map_err(remote_session_sync_error)?;
        let child = spawn_waitagent_sidecar_child(
            &current_executable,
            remote_session_sync_owner_args(socket_name, network, Some(&ready_socket)),
        )
        .map_err(remote_session_sync_error)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] ensure_session_sync_owner socket={} sidecar_spawned elapsed={:?}",
            socket_name,
            t_owner.elapsed()
        ));
        let ready = wait_for_remote_session_sync_owner_ready(ready_listener, &ready_socket, child);
        let _ = fs::remove_file(&ready_socket);
        ready?;
        ERROR_LOG.log(format!(
            "[diag-newhost] ensure_session_sync_owner socket={} ready elapsed={:?}",
            socket_name,
            t_owner.elapsed()
        ));
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
        let socket_path = remote_session_sync_owner_socket_path(socket_name);
        match notify_remote_session_sync_owner(&socket_path, reason.clone()) {
            Ok(()) => Ok(()),
            Err(first_error) => {
                ERROR_LOG.log(format!(
                    "[diag-exit] session_sync_notify retry socket={} reason={} first_error={}",
                    socket_name,
                    reason.as_str(),
                    first_error
                ));
                Self::ensure_owner_running(socket_name, network)?;
                notify_remote_session_sync_owner(&socket_path, reason)
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
        let socket_path = remote_session_sync_owner_socket_path(socket_name);
        match signal_remote_session_sync_owner(&socket_path, reason.clone()) {
            Ok(()) => Ok(()),
            Err(first_error) => {
                ERROR_LOG.log(format!(
                    "[diag-exit] session_sync_signal retry socket={} reason={} first_error={}",
                    socket_name,
                    reason.as_str(),
                    first_error
                ));
                Self::ensure_owner_running(socket_name, network)?;
                signal_remote_session_sync_owner(&socket_path, reason)
            }
        }
    }
}

struct OwnerStartupLock {
    _file: fs::File,
}

impl OwnerStartupLock {
    fn try_acquire(path: &Path) -> Result<Option<Self>, LifecycleError> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(remote_session_sync_error)?;
        match flock_owner_startup_lock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(remote_session_sync_error(error)),
        }
    }

    fn acquire(path: &Path) -> Result<Self, LifecycleError> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(remote_session_sync_error)?;
        flock_owner_startup_lock(&file, libc::LOCK_EX).map_err(remote_session_sync_error)?;
        Ok(Self { _file: file })
    }
}

fn owner_startup_lock_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("sock.lock")
}

fn flock_owner_startup_lock(file: &fs::File, operation: libc::c_int) -> io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn remote_session_sync_owner_ready_socket_path(owner_socket_path: &Path) -> PathBuf {
    let pid = std::process::id();
    owner_socket_path.with_extension(format!("ready-{pid}.sock"))
}

fn notify_remote_session_sync_owner_ready(
    ready_socket: Option<&str>,
    result: Result<(), String>,
) -> io::Result<()> {
    let Some(ready_socket) = ready_socket else {
        return Ok(());
    };
    let mut stream = UnixStream::connect(ready_socket)?;
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
    listener: UnixListener,
    ready_socket: &Path,
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

    loop {
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
                return Err(LifecycleError::Protocol(format!(
                    "remote session sync owner sent invalid ready response `{response}`"
                )));
            }
            Ok(SessionSyncOwnerReadyEvent::Ready(Err(error))) => {
                return Err(remote_session_sync_error(error));
            }
            Ok(SessionSyncOwnerReadyEvent::Exited(Ok(status))) => {
                return Err(LifecycleError::Protocol(format!(
                    "remote session sync owner exited before reporting ready: {status}"
                )));
            }
            Ok(SessionSyncOwnerReadyEvent::Exited(Err(error))) => {
                return Err(remote_session_sync_error(error));
            }
            Err(_) => {
                return Err(LifecycleError::Protocol(format!(
                    "remote session sync owner ready socket `{}` closed before reporting ready",
                    ready_socket.display()
                )));
            }
        }
    }
}

impl<G, T, O> RemoteNodeSessionSyncRuntime<G, T, O>
where
    G: LocalSessionCatalog,
    T: OutboundRemoteNodeTransport,
    O: LocalTargetExitObserver,
{
    pub fn new_with_local_target_exit_observer(
        gateway: G,
        transport: T,
        local_target_exit_observer: O,
        network: RemoteNetworkConfig,
    ) -> Self {
        Self {
            gateway,
            transport,
            local_target_exit_observer,
            network,
            reconnect_delay: SESSION_SYNC_RECONNECT_DELAY,
        }
    }

    #[cfg(test)]
    pub fn start(self) -> Result<RemoteNodeSessionSyncGuard, LifecycleError> {
        let (_local_catalog_tx, local_catalog_rx) = mpsc::channel();
        self.start_with_local_catalog_changes(local_catalog_rx)
    }

    fn start_with_local_catalog_changes(
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
                self.gateway,
                self.transport,
                self.network,
                self.local_target_exit_observer,
                node_id,
                endpoint_uri,
                local_catalog_rx,
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

struct CreatedSyncSessionTarget {
    session_id: String,
    target_id: String,
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

impl SessionSyncAuthorityManager {
    pub(super) fn with_ingress_events(
        network: RemoteNetworkConfig,
        local_target_socket_name: Option<String>,
        ingress_event_tx: mpsc::Sender<
            crate::runtime::remote_node::remote_node_ingress_server_runtime::InternalEvent,
        >,
    ) -> Self {
        Self {
            running_hosts: HashMap::new(),
            network,
            local_target_socket_name,
            output_route: SessionSyncAuthorityOutputRoute::IngressEvent(ingress_event_tx),
        }
    }

    fn with_session_events(
        network: RemoteNetworkConfig,
        local_target_socket_name: Option<String>,
        session_event_tx: mpsc::Sender<SessionSyncEvent>,
    ) -> Self {
        Self {
            running_hosts: HashMap::new(),
            network,
            local_target_socket_name,
            output_route: SessionSyncAuthorityOutputRoute::OwnerEvent(session_event_tx),
        }
    }

    pub(super) fn shutdown(&mut self) {
        for (_, host) in self.running_hosts.drain() {
            host.running.store(false, Ordering::Relaxed);
            let writer = match host.writer.lock() {
                Ok(mut guard) => guard.take(),
                Err(poisoned) => {
                    ERROR_LOG.log(
                        "[session-sync] authority writer mutex poisoned, recovering".to_string(),
                    );
                    poisoned.into_inner().take()
                }
            };
            if let Some(writer) = writer {
                let _ = writer.shutdown(Shutdown::Both);
            }
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
            "[diag-create] sync owner received create-session request id={} authority={}",
            payload.request_id, payload.authority_node_id
        ));
        let result = self.create_local_target_for_create_session(session_handle, &payload);
        ERROR_LOG.log(format!(
            "[diag-create] sync owner create-session target result id={} elapsed={:?}",
            payload.request_id,
            started.elapsed()
        ));
        match result {
            Ok(created) => session_handle
                .send(crate::runtime::remote_node_session_runtime::map_outbound_grpc_envelope(
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
                .send(crate::runtime::remote_node_session_runtime::map_outbound_grpc_envelope(
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
    ) -> Result<CreatedSyncSessionTarget, LifecycleError> {
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
        let runtime = TargetHostRuntime::from_build_env_with_network_and_executable(
            EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?,
            self.network.clone(),
            current_waitagent_executable()?,
        )?;
        let socket_name = self.local_target_socket_name.as_deref().ok_or_else(|| {
            LifecycleError::Protocol(
                "create-session requires a socket-scoped local session catalog".to_string(),
            )
        })?;
        let target_started = std::time::Instant::now();
        let workspace = runtime
            .ensure_target_host(WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                &cwd,
                socket_name,
                u16::try_from(payload.rows).ok().filter(|rows| *rows > 0),
                u16::try_from(payload.cols).ok().filter(|cols| *cols > 0),
            ))
            .map_err(remote_session_sync_error)?;
        let session_id = workspace.workspace_handle.session_name.as_str().to_string();
        ERROR_LOG.log(format!(
            "[diag-create] sync owner ensure_target_host id={} session={} elapsed={:?}",
            payload.request_id,
            session_id,
            target_started.elapsed()
        ));
        Ok(CreatedSyncSessionTarget {
            target_id: format!("remote-peer:{}:{session_id}", session_handle.node_id()),
            session_id,
        })
    }

    fn ensure_authority_host(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        target_id: &str,
    ) -> Result<(), LifecycleError> {
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
        spawn_live_authority_listener(
            authority_socket_path.clone(),
            session_handle.node_id().to_string(),
            self.output_route.clone(),
            running.clone(),
            writer.clone(),
            writer_ready.clone(),
        );
        spawn_in_process_authority_target_host(
            running.clone(),
            writer.clone(),
            writer_ready.clone(),
            self.network.clone(),
            RemoteAuthorityTargetHostCommand {
                socket_name: socket_name.clone(),
                target_session_name: session_name.clone(),
                transport_session_id: target_session_name_from_target_id(target_id)
                    .unwrap_or_else(|| target_id.to_string()),
                authority_id: session_handle.node_id().to_string(),
                target_id: target_id.to_string(),
                transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
            },
        )?;
        self.running_hosts.insert(
            target_id.to_string(),
            SessionSyncAuthorityHost {
                writer,
                running,
                writer_ready,
            },
        );
        Ok(())
    }

    fn ensure_and_send_command(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        command: RemoteAuthorityCommand,
    ) -> Result<(), LifecycleError> {
        let target_id = authority_command_target_id(&command).to_string();
        if !self.running_hosts.contains_key(&target_id) {
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
            .map(authority_host_signal)
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
        match deliver_command_to_ready_host(host, command.clone())? {
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

impl RemoteAuthorityPublicationGateway for SessionSyncAuthorityPublicationGateway {
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        _authority_id: &str,
        _target_id: &str,
        _transport_socket_path: &str,
    ) -> Result<PathBuf, LifecycleError> {
        let authority_socket_path =
            live_authority_session_socket_path(socket_name, target_session_name);
        wait_for_live_authority_socket(&authority_socket_path)?;
        Ok(authority_socket_path)
    }

    fn ensure_live_session_unregistered(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }

    fn signal_source_session_closed(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }

    fn signal_local_runtime_changed(&self, socket_name: &str) -> Result<(), LifecycleError> {
        RemoteNodeSessionSyncRuntime::signal_local_catalog_changed(
            socket_name,
            &self.network,
            LocalCatalogChangeReason::LocalRuntimeChanged,
        )
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

#[cfg(test)]
mod remote_node_session_sync_runtime_test;
