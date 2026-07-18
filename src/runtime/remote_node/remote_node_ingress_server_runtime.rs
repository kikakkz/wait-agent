use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    ApplyPtyResize, CloseMirrorRequest, CreateSessionRequest,
    NodeSessionEnvelope as GrpcNodeSessionEnvelope, OpenMirrorRequest, RawPtyInput,
    RouteContext, TargetExited as GrpcTargetExited,
    TargetPublicationAck as GrpcTargetPublicationAck,
    TargetPublicationAckStatus as GrpcTargetPublicationAckStatus,
    TargetPublished as GrpcTargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, RemoteNodeSessionHandle,
    RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{
    BootstrapMode, ControlPlanePayload, CreateSessionAcceptedPayload, CreateSessionRejectedPayload,
    ProtocolEnvelope, TargetExitedPayload, TargetPublicationAckPayload, TargetPublicationAckStatus,
    TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_node_session_envelope, write_node_session_envelope,
};
use crate::infra::tmux::EmbeddedTmuxBackend;
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_authority_transport_runtime::{
    authority_target_component, RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
use crate::runtime::remote_node_session_runtime::{
    map_inbound_grpc_authority_event, map_outbound_grpc_envelope,
};
use crate::runtime::remote_node_session_sync_runtime::SessionSyncAuthorityManager;
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::remote_workspace_socket_registry_runtime::{
    workspace_socket_registry_path, RemoteWorkspaceSocketRegistryRuntime,
};
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar_child;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Cursor, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BRIDGE_REFRESH_INTERVAL: Duration = Duration::from_millis(50);
const BRIDGE_DISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(25);
const BRIDGE_DISCOVERY_RETRY_ATTEMPTS: u8 = 20;
const REMOTE_NODE_INGRESS_OWNER_IDLE_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const OWNER_CONTROL_MAGIC: &[u8; 4] = b"waOC";
const OWNER_CONTROL_REPLY_OK: u8 = 0;
const OWNER_CONTROL_REPLY_PENDING: u8 = 1;
const OWNER_CONTROL_REPLY_ERROR: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AuthoritySocketReadyReply {
    status: AuthoritySocketReadyStatus,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AuthoritySocketReadyStatus {
    Registered,
    Pending,
    Error,
}

pub struct RemoteNodeIngressServerRuntime {
    publication_runtime: RemoteTargetPublicationRuntime,
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
    bridges: HashMap<PathBuf, ActiveAuthoritySocketBridge>,
    published_fingerprints: HashMap<String, String>,
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

pub(super) enum InternalEvent {
    BridgeClosed {
        node_id: String,
        socket_path: PathBuf,
    },
    AuthorityCommandReceived {
        node_id: String,
        session_instance_id: String,
        socket_path: PathBuf,
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
        socket_path: PathBuf,
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
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerLifecycleEvent {
    WorkspaceRegistered(String),
    WorkspaceUnregistered(String),
    WorkspaceRegistryChanged(BTreeSet<String>),
    ShutdownRequested,
}

impl RemoteNodeIngressServerRuntime {
    pub fn from_build_env_with_network_and_socket(
        network: RemoteNetworkConfig,
        _socket_name: impl Into<String>,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            publication_runtime: RemoteTargetPublicationRuntime::from_build_env_with_network(
                network.clone(),
            )?,
            network,
        })
    }

    pub fn run_owner(&self, ready_socket: Option<&str>) -> Result<(), LifecycleError> {
        let socket_path = remote_node_ingress_owner_socket_path(&self.network);
        let startup =
            (|| -> Result<(UnixListener, RemoteNodeIngressServerGuard), LifecycleError> {
                if socket_path.exists() {
                    let _ = fs::remove_file(&socket_path);
                }
                let listener =
                    UnixListener::bind(&socket_path).map_err(remote_node_ingress_error)?;
                let guard = self.start()?;
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
        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    pub fn ensure_owner_running(
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let socket_path = remote_node_ingress_owner_socket_path(network);
        if remote_node_ingress_owner_available(&socket_path) {
            register_workspace_socket_with_owner(network, socket_name)?;
            return Ok(());
        }
        let lock_path = owner_startup_lock_path(&socket_path);
        let Some(_startup_lock) = OwnerStartupLock::try_acquire(&lock_path)? else {
            let _startup_lock = OwnerStartupLock::acquire(&lock_path)?;
            if remote_node_ingress_owner_available(&socket_path) {
                register_workspace_socket_with_owner(network, socket_name)?;
                return Ok(());
            }
            return Err(LifecycleError::Protocol(format!(
                "remote node ingress owner for listener `{}` was not ready after startup lock {} released",
                network.listener_addr(),
                lock_path.display()
            )));
        };
        if remote_node_ingress_owner_available(&socket_path) {
            register_workspace_socket_with_owner(network, socket_name)?;
            return Ok(());
        }
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let current_executable = current_waitagent_executable()?;
        let ready_socket = owner_ready_socket_path(&socket_path);
        if ready_socket.exists() {
            let _ = fs::remove_file(&ready_socket);
        }
        let ready_listener =
            UnixListener::bind(&ready_socket).map_err(remote_node_ingress_error)?;
        let child = spawn_waitagent_sidecar_child(
            &current_executable,
            remote_node_ingress_owner_args(network, Some(&ready_socket)),
        )
        .map_err(remote_node_ingress_error)?;
        let ready = wait_for_owner_ready(ready_listener, &ready_socket, child);
        let _ = fs::remove_file(&ready_socket);
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
        let socket_path = remote_node_ingress_owner_socket_path(network);
        if !remote_node_ingress_owner_available(&socket_path) {
            return Ok(());
        }
        unregister_workspace_socket_with_owner(network, socket_name)
    }

    pub fn shutdown_owner(network: &RemoteNetworkConfig) -> Result<(), LifecycleError> {
        let socket_path = remote_node_ingress_owner_socket_path(network);
        if !remote_node_ingress_owner_available(&socket_path) {
            return Ok(());
        }
        shutdown_owner_with_control_socket(network)
    }

    pub fn start(&self) -> Result<RemoteNodeIngressServerGuard, LifecycleError> {
        let transport = GrpcRemoteNodeTransport::new();
        let (transport_tx, transport_rx) = mpsc::channel();
        let (internal_tx, internal_rx) = mpsc::channel();
        let transport_guard = transport
            .listen_inbound(self.network.listener_addr(), transport_tx)
            .map_err(remote_node_ingress_error)?;
        let publication_runtime = self.publication_runtime.clone();
        let network = self.network.clone();
        let shutdown_tx = internal_tx.clone();
        let worker = thread::spawn(move || {
            let _ = run_node_ingress_server_loop(
                publication_runtime,
                network,
                transport_rx,
                internal_rx,
                internal_tx,
                true,
            );
        });
        Ok(RemoteNodeIngressServerGuard {
            transport_guard: Some(transport_guard),
            worker: Some(worker),
            shutdown_tx: Some(shutdown_tx),
        })
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
            .map_err(remote_node_ingress_error)?;
        match flock_owner_startup_lock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(remote_node_ingress_error(error)),
        }
    }

    fn acquire(path: &Path) -> Result<Self, LifecycleError> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(remote_node_ingress_error)?;
        flock_owner_startup_lock(&file, libc::LOCK_EX).map_err(remote_node_ingress_error)?;
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

pub(crate) fn notify_authority_socket_ready(
    network: &RemoteNetworkConfig,
    node_id: &str,
    socket_path: &std::path::Path,
) -> io::Result<()> {
    RemoteNodeIngressServerRuntime::ensure_owner_running("__shared__", network)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
    let mut stream = UnixStream::connect(remote_node_ingress_owner_socket_path(network))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    write_owner_control_authority_socket_ready(&mut stream, node_id, socket_path)?;
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
    let mut stream = UnixStream::connect(remote_node_ingress_owner_socket_path(network))
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

fn unregister_workspace_socket_with_owner(
    network: &RemoteNetworkConfig,
    socket_name: &str,
) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(remote_node_ingress_owner_socket_path(network))
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

fn shutdown_owner_with_control_socket(network: &RemoteNetworkConfig) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(remote_node_ingress_owner_socket_path(network))
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

pub(crate) fn remote_node_ingress_owner_socket_path(network: &RemoteNetworkConfig) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-node-ingress-{}.sock",
        sanitize_socket_component(&network.listener_addr().to_string())
    ))
}

fn owner_ready_socket_path(owner_socket_path: &Path) -> PathBuf {
    let pid = std::process::id();
    owner_socket_path.with_extension(format!("ready-{pid}.sock"))
}

fn notify_owner_ready(ready_socket: Option<&str>, result: Result<(), String>) -> io::Result<()> {
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

fn wait_for_owner_ready(
    listener: UnixListener,
    ready_socket: &Path,
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

    loop {
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
                return Err(LifecycleError::Protocol(format!(
                    "remote node ingress owner sent invalid ready response `{response}`"
                )));
            }
            Ok(OwnerReadyEvent::Ready(Err(error))) => {
                return Err(remote_node_ingress_error(error));
            }
            Ok(OwnerReadyEvent::Exited(Ok(status))) => {
                return Err(LifecycleError::Protocol(format!(
                    "remote node ingress owner exited before reporting ready: {status}"
                )));
            }
            Ok(OwnerReadyEvent::Exited(Err(error))) => {
                return Err(remote_node_ingress_error(error))
            }
            Err(_) => {
                return Err(LifecycleError::Protocol(format!(
                    "remote node ingress owner ready socket `{}` closed before reporting ready",
                    ready_socket.display()
                )));
            }
        }
    }
}

fn remote_node_ingress_owner_args(
    network: &RemoteNetworkConfig,
    ready_socket: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "__remote-node-ingress-server".to_string(),
        "--socket-name".to_string(),
        "__shared__".to_string(),
    ];
    if let Some(ready_socket) = ready_socket {
        args.push("--ready-socket".to_string());
        args.push(ready_socket.display().to_string());
    }
    prepend_global_network_args(args, network)
}

fn remote_node_ingress_owner_available(socket_path: &std::path::Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn start_owner_control_acceptor(
    listener: UnixListener,
    owner_tx: &mpsc::Sender<InternalEvent>,
    lifecycle_tx: mpsc::Sender<OwnerLifecycleEvent>,
) -> thread::JoinHandle<()> {
    let owner_tx = owner_tx.clone();
    thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            handle_owner_stream(stream, owner_tx.clone(), lifecycle_tx.clone());
        }
    })
}

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

fn handle_owner_stream(
    mut stream: UnixStream,
    owner_tx: mpsc::Sender<InternalEvent>,
    lifecycle_tx: mpsc::Sender<OwnerLifecycleEvent>,
) {
    thread::spawn(move || {
        let mut prefix = [0_u8; 4];
        if stream.read_exact(&mut prefix).is_err() {
            return;
        }
        let request = if &prefix == OWNER_CONTROL_MAGIC {
            match read_owner_control_message(&mut stream) {
                Ok(OwnerControlMessage::AuthoritySocketReady {
                    node_id,
                    socket_path,
                }) => {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    if owner_tx
                        .send(InternalEvent::AuthoritySocketReady {
                            node_id,
                            socket_path,
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
        ERROR_LOG.log(format!("[diag-create] owner received local create-session request id={request_id} authority={authority_node_id}"));
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
                ERROR_LOG.log(format!(
                    "[diag-create] owner got create-session reply after {:?}",
                    wait_started.elapsed()
                ));
                if let Some(envelope) = map_local_reply_from_grpc(reply) {
                    let _ = write_node_session_envelope(&mut stream, &envelope);
                }
            }
            Err(_) => {
                ERROR_LOG.log(format!("[diag-create] owner timed out waiting create-session reply id={request_id} after {:?}", wait_started.elapsed()));
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
        socket_path: PathBuf,
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
    socket_path: &std::path::Path,
) -> io::Result<()> {
    writer.write_all(OWNER_CONTROL_MAGIC)?;
    writer.write_all(&[1])?;
    write_owner_control_string(writer, node_id)?;
    write_owner_control_string(writer, &socket_path.to_string_lossy())?;
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

fn write_owner_control_unregister_workspace_socket(
    writer: &mut impl Write,
    socket_name: &str,
) -> io::Result<()> {
    writer.write_all(OWNER_CONTROL_MAGIC)?;
    writer.write_all(&[3])?;
    write_owner_control_string(writer, socket_name)?;
    writer.flush()
}

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
            let socket_path = PathBuf::from(read_owner_control_string(reader)?);
            Ok(OwnerControlMessage::AuthoritySocketReady {
                node_id,
                socket_path,
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

fn live_workspace_sockets(
    network: &RemoteNetworkConfig,
) -> Result<BTreeSet<String>, LifecycleError> {
    let backend = EmbeddedTmuxBackend::from_build_env().map_err(remote_node_ingress_error)?;
    let registry = RemoteWorkspaceSocketRegistryRuntime::new(network.clone());
    registry.live_workspace_socket_names_retaining(|socket_name| {
        backend.socket_is_live(&crate::infra::tmux::TmuxSocketName::new(socket_name))
    })
}

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
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let c_path = std::ffi::CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid tmux socket dir"))?;
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            c_path.as_ptr(),
            (libc::IN_CREATE | libc::IN_CLOSE_WRITE | libc::IN_DELETE | libc::IN_MOVED_TO) as u32,
        )
    };
    if wd < 0 {
        let error = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(error);
    }

    Ok(thread::spawn(move || {
        let event_size = std::mem::size_of::<libc::inotify_event>();
        let mut buf = [0u8; 4096];

        loop {
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
                                unsafe { libc::close(fd) };
                                return;
                            }
                        }
                    }
                }
                off += event_size + name_len;
            }
        }

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
            thread::sleep(BRIDGE_REFRESH_INTERVAL);
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

/// Watches the temp directory for new authority socket files and sends
/// [`InternalEvent::SocketDirChanged`] through the channel when one appears.
///
/// Linux production uses a blocking inotify fd so bridge discovery is driven by
/// kernel filesystem events, not periodic refresh scans.
fn start_socket_watcher(
    internal_tx: mpsc::Sender<InternalEvent>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<thread::JoinHandle<()>> {
    #[cfg(target_os = "linux")]
    {
        start_inotify_watcher(internal_tx, shutdown)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (internal_tx, shutdown);
        Err(io::Error::other(
            "remote node ingress server requires Linux inotify for event-driven authority discovery",
        ))
    }
}

/// Linux inotify-based watcher.
#[cfg(target_os = "linux")]
fn start_inotify_watcher(
    internal_tx: mpsc::Sender<InternalEvent>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<thread::JoinHandle<()>> {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }

    let dir = std::env::temp_dir();
    let dir_path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::other("temp_dir contains interior null byte"))?;

    let wd = unsafe {
        libc::inotify_add_watch(fd, dir_path.as_ptr(), libc::IN_CREATE | libc::IN_MOVED_TO)
    };
    if wd == -1 {
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }

    Ok(thread::spawn(move || {
        let event_size = std::mem::size_of::<libc::inotify_event>();
        let mut buf = [0u8; 4096];

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    thread::sleep(BRIDGE_REFRESH_INTERVAL);
                    continue;
                }
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

        unsafe { libc::close(fd) };
    }))
}

impl RemoteNodeIngressServerGuard {
    fn owner_event_sender(&self) -> Option<mpsc::Sender<InternalEvent>> {
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

fn run_node_ingress_server_loop(
    publication_runtime: RemoteTargetPublicationRuntime,
    network: RemoteNetworkConfig,
    transport_rx: mpsc::Receiver<RemoteNodeTransportEvent>,
    internal_rx: mpsc::Receiver<InternalEvent>,
    internal_tx: mpsc::Sender<InternalEvent>,
    start_authority_socket_watcher: bool,
) {
    let mut sessions = HashMap::<String, ActiveNodeIngressSession>::new();
    let mut authority_manager =
        SessionSyncAuthorityManager::with_ingress_events(network, None, internal_tx.clone());
    let mut pending_create_sessions =
        HashMap::<String, mpsc::Sender<GrpcNodeSessionEnvelope>>::new();
    let mut registered_workspace_sockets = BTreeSet::<String>::new();
    let (event_tx, event_rx) = mpsc::channel::<IngressServerEvent>();

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
    drop(event_tx);
    let watcher_shutdown = Arc::new(AtomicBool::new(false));
    let watcher = if start_authority_socket_watcher {
        match start_socket_watcher(internal_tx.clone(), watcher_shutdown.clone()) {
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

    loop {
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
                &publication_runtime,
                &mut authority_manager,
                &mut sessions,
                &mut pending_create_sessions,
                &registered_workspace_sockets,
                &mut publication_revisions,
                internal_tx.clone(),
                &mut socket_discovery_retry_scheduled,
                &mut closed_session_instances,
                event,
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
            IngressServerEvent::Internal(event) => {
                handle_internal_event(
                    &mut sessions,
                    &mut registered_workspace_sockets,
                    internal_tx.clone(),
                    &mut socket_discovery_retry_scheduled,
                    event,
                );
            }
        }
    }
    watcher_shutdown.store(true, Ordering::Relaxed);
    if let Some(watcher) = watcher {
        let _ = watcher.join();
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

fn handle_transport_event(
    publication_runtime: &RemoteTargetPublicationRuntime,
    authority_manager: &mut SessionSyncAuthorityManager,
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    pending_create_sessions: &mut HashMap<String, mpsc::Sender<GrpcNodeSessionEnvelope>>,
    registered_workspace_sockets: &BTreeSet<String>,
    publication_revisions: &mut ReceiverPublicationRevisionTable,
    internal_tx: mpsc::Sender<InternalEvent>,
    socket_discovery_retry_scheduled: &mut bool,
    closed_session_instances: &mut HashSet<String>,
    event: RemoteNodeTransportEvent,
) {
    match event {
        RemoteNodeTransportEvent::SessionOpened { session } => {
            let node_id = session.node_id().to_string();
            let session_instance_id = session.session_instance_id().to_string();
            closed_session_instances.remove(&session_instance_id);
            let mut active = ActiveNodeIngressSession {
                session,
                bridges: HashMap::new(),
                published_fingerprints: HashMap::new(),
            };
            let outcome = refresh_authority_bridges(&node_id, &mut active, internal_tx.clone());
            if outcome.pending > 0 {
                schedule_socket_discovery_retry(
                    internal_tx,
                    BRIDGE_DISCOVERY_RETRY_ATTEMPTS,
                    socket_discovery_retry_scheduled,
                );
            }
            sessions.insert(session_instance_id, active);
        }
        RemoteNodeTransportEvent::EnvelopeReceived {
            node_id,
            session_instance_id,
            envelope,
        } => {
            if closed_session_instances.contains(&session_instance_id) {
                ERROR_LOG.log(format!(
                    "[diag-ingress] dropping event for closed session_instance_id={session_instance_id} node={node_id}"
                ));
                return;
            }
            if let Some(request_id) = grpc_create_session_reply_request_id(&envelope) {
                if let Some(reply_tx) = pending_create_sessions.remove(&request_id) {
                    let _ = reply_tx.send(envelope);
                    return;
                }
            }
            let is_command = matches!(
                envelope.body.as_ref(),
                Some(Body::OpenMirrorRequest(_))
                    | Some(Body::CloseMirrorRequest(_))
                    | Some(Body::ApplyPtyResize(_))
                    | Some(Body::RawPtyInput(_))
            );
            if is_command {
                if let Some(active) = sessions.get(&session_instance_id) {
                    if let Some(event) = map_inbound_grpc_authority_event(envelope) {
                        ERROR_LOG.log(format!(
                            "[diag-timing] ingress server: routing command via authority_manager node={node_id}"
                        ));
                        authority_manager.handle_event(&active.session, event);
                    }
                } else {
                    ERROR_LOG.log(format!(
                        "[diag-timing] ingress server: no session for command node={node_id}, dropping"
                    ));
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
            authority_manager.stop_hosts_bound_to_session(&session_instance_id);
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
                authority_manager.stop_hosts_bound_to_session(&session_instance_id);
            }
            if let Some(node_id) = node_id {
                mark_discovered_node_offline_if_last_ingress_session(
                    publication_runtime,
                    sessions,
                    node_id.as_str(),
                );
            }
        }
    }
}

fn has_active_ingress_session_for_node(
    sessions: &HashMap<String, ActiveNodeIngressSession>,
    node_id: &str,
) -> bool {
    sessions
        .values()
        .any(|active| active.session.node_id() == node_id)
}

fn mark_discovered_node_offline_if_last_ingress_session(
    publication_runtime: &RemoteTargetPublicationRuntime,
    sessions: &HashMap<String, ActiveNodeIngressSession>,
    node_id: &str,
) {
    if has_active_ingress_session_for_node(sessions, node_id) {
        return;
    }
    if let Err(error) = publication_runtime.mark_discovered_remote_node_offline(node_id) {
        ERROR_LOG.log(format!(
            "[diag] ingress server: failed to mark discovered node offline {node_id}: {error}"
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
        let _ = reply_tx.send(local_create_session_rejected_grpc_envelope(
            request_id,
            format!("remote authority `{authority_node_id}` is not connected"),
        ));
        return;
    };
    let request_id = grpc_create_session_request_id(&envelope).unwrap_or_default();
    ERROR_LOG.log(format!("[diag-create] routing create-session request id={request_id} authority={authority_node_id}"));
    pending_create_sessions.insert(request_id.clone(), reply_tx);
    let send_started = std::time::Instant::now();
    if active.session.send(envelope).is_err() {
        ERROR_LOG.log(format!(
            "[diag-create] failed sending create-session request id={request_id} after {:?}",
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
    stream: &mut UnixStream,
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

fn handle_internal_event(
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    registered_workspace_sockets: &mut BTreeSet<String>,
    internal_tx: mpsc::Sender<InternalEvent>,
    socket_discovery_retry_scheduled: &mut bool,
    event: InternalEvent,
) {
    match event {
        InternalEvent::BridgeClosed {
            node_id,
            socket_path,
        } => {
            for active in sessions.values_mut() {
                if active.session.node_id() == node_id {
                    active.bridges.remove(&socket_path);
                }
            }
        }
        InternalEvent::AuthorityCommandReceived {
            node_id,
            session_instance_id,
            socket_path,
            command,
        } => {
            let Some(active) = sessions.get(&session_instance_id) else {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] dropping authority command for node={node_id} session_instance_id={session_instance_id} socket={} because no active session is open",
                    socket_path.display()
                ));
                return;
            };
            let envelope = match map_authority_command_to_grpc(&active.session, command) {
                Ok(envelope) => envelope,
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[remote-node-ingress] failed to map authority command for node={node_id} session_instance_id={session_instance_id} socket={}: {error}",
                        socket_path.display()
                    ));
                    return;
                }
            };
            if let Err(error) = active.session.send(envelope) {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] failed to send authority command for node={node_id} session_instance_id={session_instance_id} socket={}: {error}",
                    socket_path.display()
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
            ERROR_LOG.log(format!(
                "[diag-timing] ingress authority output: forwarding envelope type={} to session_instance_id={}",
                envelope.payload.message_type(),
                session_instance_id
            ));
            if let Err(error) = active.session.send(grpc) {
                ERROR_LOG.log(format!(
                    "[remote-node-ingress] failed to send authority output for node={node_id} session_instance_id={session_instance_id} type={}: {error}",
                    envelope.payload.message_type()
                ));
            }
        }
        InternalEvent::AuthoritySocketReady {
            node_id,
            socket_path,
            reply_tx,
        } => {
            let outcome = refresh_authority_bridge_for_socket(
                sessions,
                internal_tx,
                &node_id,
                socket_path.clone(),
                socket_discovery_retry_scheduled,
            );
            let reply = authority_socket_ready_reply(&node_id, &socket_path, outcome);
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
        InternalEvent::Shutdown
        | InternalEvent::ShutdownOwner { .. }
        | InternalEvent::LocalCreateSession { .. }
        | InternalEvent::LocalCreateSessionTimedOut { .. } => {}
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
        ERROR_LOG.log(format!(
            "[diag-publication] failed to send publication ack node={node_id} target={target_id} revision={revision}: {error}"
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

fn route_transport_envelope(
    publication_runtime: &RemoteTargetPublicationRuntime,
    node_id: &str,
    envelope: GrpcNodeSessionEnvelope,
    mut session: Option<&mut ActiveNodeIngressSession>,
    registered_workspace_sockets: &BTreeSet<String>,
    publication_revisions: &mut ReceiverPublicationRevisionTable,
) -> Result<(), LifecycleError> {
    match envelope.body.as_ref() {
        Some(Body::TargetPublished(payload)) => {
            let target_id = route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone());
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
            let mapped = map_target_published_envelope(node_id, &envelope, payload)
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
        Some(Body::TargetPublicationAck(payload)) => {
            let mapped = map_target_publication_ack_envelope(node_id, &envelope, payload)
                .map_err(remote_node_ingress_error)?;
            publication_runtime.apply_discovered_remote_session_envelope(node_id, mapped)
        }
        Some(Body::TargetExited(payload)) => {
            let target_id = route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone());
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
            let t_exit = std::time::Instant::now();
            ERROR_LOG.log_exit_latency(format!(
                "[diag-bug] ingress_server route_transport_envelope: received TargetExited node={node_id} session={}",
                payload.transport_session_id
            ));
            let mapped = map_target_exited_envelope(node_id, &envelope, payload);
            let t_apply = std::time::Instant::now();
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
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] ingress_target_exited_apply node={} session={} elapsed={:?} total={:?} stage=ingress_server",
                node_id,
                payload.transport_session_id,
                t_apply.elapsed(),
                t_exit.elapsed()
            ));
            ERROR_LOG.log_exit_latency(format!(
                "[diag-bug] ingress_server: applied TargetExited to live workspaces node={node_id} session={}",
                payload.transport_session_id
            ));
            let Some(session) = session else {
                return Ok(());
            };
            let session_id = route_session_id(&envelope)
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
                            source_session_name: None,
                        }),
                    )
                },
            )
        }
        Some(Body::TargetOutput(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            let stream = known_output_stream(&payload.stream).map_err(remote_node_ingress_error)?;
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
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
        Some(Body::RawPtyOutput(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
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
        Some(Body::MirrorBootstrapChunk(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            let stream = known_output_stream(&payload.stream).map_err(remote_node_ingress_error)?;
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
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
        Some(Body::MirrorBootstrapComplete(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
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
        Some(Body::OpenMirrorRequest(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
                |transport, session_id, target_id| {
                    transport.send_open_mirror_request(
                        &session_id,
                        &target_id,
                        &payload.console_id,
                        payload.cols as usize,
                        payload.rows as usize,
                        payload.raw_pty_passthrough,
                        if payload.bootstrap_mode_visible_only {
                            BootstrapMode::VisibleOnly
                        } else {
                            BootstrapMode::Full
                        },
                    )
                },
            )
        }
        Some(Body::CloseMirrorRequest(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
                |transport, session_id, target_id| {
                    transport.send_close_mirror_request(&session_id, &target_id)
                },
            )
        }
        Some(Body::ApplyPtyResize(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
                |transport, session_id, target_id| {
                    transport.send_apply_resize(
                        &session_id,
                        &target_id,
                        payload.cols as usize,
                        payload.rows as usize,
                        payload.resize_epoch,
                        payload.resize_authority_console_id.clone(),
                    )
                },
            )
        }
        Some(Body::PtyResizeApplied(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
                |transport, session_id, target_id| {
                    transport.send_resize_applied(
                        &session_id,
                        &target_id,
                        payload.cols as usize,
                        payload.rows as usize,
                        payload.resize_epoch,
                        payload.resize_authority_console_id.clone(),
                    )
                },
            )
        }
        Some(Body::RawPtyInput(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            bridge_output_to_authority_transports(
                node_id,
                session,
                route_session_id(&envelope)
                    .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                    .unwrap_or_else(|| payload.target_id.clone()),
                route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone()),
                |transport, session_id, target_id| {
                    transport.send_raw_pty_input(
                        &session_id,
                        &target_id,
                        &payload.console_id,
                        &payload.attachment_id,
                        &payload.console_host_id,
                        payload.input_seq,
                        payload.input_bytes.clone(),
                    )
                },
            )
        }
        Some(Body::Heartbeat(_)) | Some(Body::ClientHello(_)) | Some(Body::ServerHello(_)) => {
            Ok(())
        }
        _ => Ok(()),
    }
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
        crate::runtime::remote_authority_transport_runtime::RemoteAuthorityTransportError,
    >,
{
    let target_component = authority_target_component(node_id, &session_id);
    let mut stale = Vec::new();
    for (path, bridge) in &session.bridges {
        if bridge.target_component != target_component {
            continue;
        }
        if let Err(error) = deliver(&bridge.transport, &session_id, &target_id) {
            let _ = error;
            stale.push(path.clone());
        }
    }
    for path in stale {
        session.bridges.remove(&path);
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
    socket_path: PathBuf,
    socket_discovery_retry_scheduled: &mut bool,
) -> BridgeRefreshOutcome {
    let mut total = BridgeRefreshOutcome::default();
    for active in sessions.values_mut() {
        if active.session.node_id() != node_id {
            continue;
        }
        let outcome =
            refresh_authority_bridge_path(node_id, active, &socket_path, internal_tx.clone());
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
    socket_path: &PathBuf,
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
                socket_path.display()
            ),
        };
    }
    AuthoritySocketReadyReply {
        status: AuthoritySocketReadyStatus::Error,
        message: format!(
            "authority socket bridge for node {node_id} was not registered: {}",
            socket_path.display()
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
    socket_path: &PathBuf,
    internal_tx: mpsc::Sender<InternalEvent>,
) -> BridgeRefreshOutcome {
    let mut outcome = BridgeRefreshOutcome::default();
    if session.bridges.contains_key(socket_path) {
        outcome.already_registered += 1;
        return outcome;
    }
    let Some(target_component) = socket_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .and_then(|name| extract_target_component(&name, node_id))
    else {
        outcome.invalid += 1;
        return outcome;
    };
    let transport = match RemoteAuthorityTransportRuntime::connect(socket_path, node_id) {
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
        socket_path.clone(),
        transport.clone(),
        internal_tx,
    );
    session.bridges.insert(
        socket_path.clone(),
        ActiveAuthoritySocketBridge {
            target_component,
            transport,
        },
    );
    outcome.connected += 1;
    ERROR_LOG.log(format!(
        "[remote-node-ingress] registered authority bridge for node={node_id}"
    ));
    outcome
}

fn refresh_authority_bridges(
    node_id: &str,
    session: &mut ActiveNodeIngressSession,
    internal_tx: mpsc::Sender<InternalEvent>,
) -> BridgeRefreshOutcome {
    let mut outcome = BridgeRefreshOutcome::default();
    let Ok(socket_paths) = discover_authority_socket_paths(node_id) else {
        return outcome;
    };
    for socket_path in socket_paths {
        if session.bridges.contains_key(&socket_path) {
            continue;
        }
        let Some(target_component) = socket_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .and_then(|name| extract_target_component(&name, node_id))
        else {
            continue;
        };
        let transport = match RemoteAuthorityTransportRuntime::connect(&socket_path, node_id) {
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
            socket_path.clone(),
            transport.clone(),
            internal_tx.clone(),
        );
        session.bridges.insert(
            socket_path,
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
    socket_path: PathBuf,
    reader: Arc<RemoteAuthorityTransportRuntime>,
    internal_tx: mpsc::Sender<InternalEvent>,
) {
    thread::spawn(move || {
        loop {
            let command = match reader.recv_command() {
                Ok(command) => command,
                Err(_) => break,
            };
            if internal_tx
                .send(InternalEvent::AuthorityCommandReceived {
                    node_id: node_id.clone(),
                    session_instance_id: session_instance_id.clone(),
                    socket_path: socket_path.clone(),
                    command,
                })
                .is_err()
            {
                return;
            }
        }
        let _ = internal_tx.send(InternalEvent::BridgeClosed {
            node_id,
            socket_path,
        });
    });
}

fn discover_authority_socket_paths(authority_id: &str) -> io::Result<Vec<PathBuf>> {
    let authority_hash = stable_socket_hash(&[authority_id]);
    let mut paths = Vec::new();
    for entry in fs::read_dir(std::env::temp_dir())? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with("waitagent-remote-") || !name.ends_with(".sock") {
            continue;
        }
        if !name.contains(&format!("-{authority_hash}-")) {
            continue;
        }
        paths.push(entry.path());
    }
    Ok(paths)
}

fn extract_target_component(file_name: &str, authority_id: &str) -> Option<String> {
    let trimmed = file_name.trim_end_matches(".sock");
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

fn sanitize_socket_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch,
            _ => '_',
        })
        .collect()
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
            source_session_name: None,
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
            source_session_name: None,
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

#[cfg(test)]
mod remote_node_ingress_server_runtime_test;
