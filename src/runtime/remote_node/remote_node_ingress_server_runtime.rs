use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    ApplyPtyResize, CloseMirrorRequest, NodeSessionEnvelope as GrpcNodeSessionEnvelope,
    OpenMirrorRequest, RawPtyInput, RouteContext, TargetExited as GrpcTargetExited,
    TargetPublished as GrpcTargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, RemoteNodeSessionHandle,
    RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{
    BootstrapMode, ControlPlanePayload, ProtocolEnvelope, TargetExitedPayload,
    TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::tmux::EmbeddedTmuxBackend;
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_authority_transport_runtime::{
    authority_target_component, RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
use crate::runtime::remote_node_session_runtime::map_inbound_grpc_authority_event;
use crate::runtime::remote_node_session_sync_runtime::SessionSyncAuthorityManager;
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BRIDGE_REFRESH_INTERVAL: Duration = Duration::from_millis(50);
const REMOTE_NODE_INGRESS_OWNER_READY_RETRIES: usize = 20;
const REMOTE_NODE_INGRESS_OWNER_READY_SLEEP: Duration = Duration::from_millis(25);

pub struct RemoteNodeIngressServerRuntime {
    publication_runtime: RemoteTargetPublicationRuntime,
    network: RemoteNetworkConfig,
}

pub struct RemoteNodeIngressServerGuard {
    transport_guard: Option<GrpcRemoteNodeTransportGuard>,
    worker: Option<thread::JoinHandle<()>>,
}

struct ActiveAuthoritySocketBridge {
    target_component: String,
    transport: Arc<RemoteAuthorityTransportRuntime>,
}

struct ActiveNodeIngressSession {
    session: RemoteNodeSessionHandle,
    bridges: HashMap<PathBuf, ActiveAuthoritySocketBridge>,
}

enum InternalEvent {
    BridgeClosed {
        node_id: String,
        socket_path: PathBuf,
    },
    SocketDirChanged,
    #[cfg(test)]
    Shutdown,
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

    pub fn run_owner(&self) -> Result<(), LifecycleError> {
        let socket_path = remote_node_ingress_owner_socket_path(&self.network);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .map_err(remote_node_ingress_error)?;
        listener
            .set_nonblocking(true)
            .map_err(remote_node_ingress_error)?;
        let _guard = self.start()?;
        while any_live_workspace_exists()? {
            let _ = drain_owner_ping(&listener);
            thread::sleep(BRIDGE_REFRESH_INTERVAL);
        }
        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    pub fn ensure_owner_running(
        _socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let socket_path = remote_node_ingress_owner_socket_path(network);
        if remote_node_ingress_owner_available(&socket_path) {
            return Ok(());
        }
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let current_executable = current_waitagent_executable()?;
        spawn_waitagent_sidecar(&current_executable, remote_node_ingress_owner_args(network))
            .map_err(remote_node_ingress_error)?;
        for _ in 0..REMOTE_NODE_INGRESS_OWNER_READY_RETRIES {
            if remote_node_ingress_owner_available(&socket_path) {
                return Ok(());
            }
            thread::sleep(REMOTE_NODE_INGRESS_OWNER_READY_SLEEP);
        }
        Err(LifecycleError::Protocol(format!(
            "remote node ingress owner for listener `{}` did not become ready",
            network.listener_addr()
        )))
    }

    pub fn start(&self) -> Result<RemoteNodeIngressServerGuard, LifecycleError> {
        let transport = GrpcRemoteNodeTransport::new();
        let (transport_tx, transport_rx) = mpsc::channel();
        let (internal_tx, internal_rx) = mpsc::channel();
        let transport_guard = transport
            .listen_inbound(self.network.listener_addr(), transport_tx)
            .map_err(remote_node_ingress_error)?;
        let publication_runtime = self.publication_runtime.clone();
        let worker = thread::spawn(move || {
            let _ = run_node_ingress_server_loop(
                publication_runtime,
                transport_rx,
                internal_rx,
                internal_tx,
                true,
            );
        });
        Ok(RemoteNodeIngressServerGuard {
            transport_guard: Some(transport_guard),
            worker: Some(worker),
        })
    }
}

pub(crate) fn remote_node_ingress_owner_socket_path(network: &RemoteNetworkConfig) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-node-ingress-{}.sock",
        sanitize_socket_component(&network.listener_addr().to_string())
    ))
}

fn remote_node_ingress_owner_args(network: &RemoteNetworkConfig) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-node-ingress-server".to_string(),
            "--socket-name".to_string(),
            "__shared__".to_string(),
        ],
        network,
    )
}

fn remote_node_ingress_owner_available(socket_path: &std::path::Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn drain_owner_ping(listener: &std::os::unix::net::UnixListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok((_stream, _)) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn any_live_workspace_exists() -> Result<bool, LifecycleError> {
    let backend = EmbeddedTmuxBackend::from_build_env().map_err(remote_node_ingress_error)?;
    let sockets = backend
        .discover_waitagent_sockets()
        .map_err(remote_node_ingress_error)?;
    Ok(sockets
        .iter()
        .any(|socket_name| backend.socket_is_live(socket_name)))
}

/// Watches the temp directory for new authority socket files and sends
/// [`InternalEvent::SocketDirChanged`] through the channel when one appears.
///
/// Linux production uses a blocking inotify fd so bridge discovery is driven by
/// kernel filesystem events, not periodic refresh scans.
fn start_socket_watcher(
    internal_tx: mpsc::Sender<InternalEvent>,
) -> io::Result<thread::JoinHandle<()>> {
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
) -> io::Result<thread::JoinHandle<()>> {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
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
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
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

        unsafe { libc::close(fd) };
    }))
}

impl Drop for RemoteNodeIngressServerGuard {
    fn drop(&mut self) {
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

fn run_node_ingress_server_loop(
    publication_runtime: RemoteTargetPublicationRuntime,
    transport_rx: mpsc::Receiver<RemoteNodeTransportEvent>,
    internal_rx: mpsc::Receiver<InternalEvent>,
    internal_tx: mpsc::Sender<InternalEvent>,
    start_authority_socket_watcher: bool,
) {
    let mut sessions = HashMap::<String, ActiveNodeIngressSession>::new();
    let mut authority_manager = SessionSyncAuthorityManager::new();
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
    let _watcher = if start_authority_socket_watcher {
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

    while let Ok(event) = event_rx.recv() {
        match event {
            IngressServerEvent::Transport(event) => handle_transport_event(
                &publication_runtime,
                &mut authority_manager,
                &mut sessions,
                internal_tx.clone(),
                event,
            ),
            #[cfg(test)]
            IngressServerEvent::Internal(InternalEvent::Shutdown) => break,
            IngressServerEvent::Internal(event) => {
                handle_internal_event(&mut sessions, internal_tx.clone(), event);
            }
        }
    }
}

fn handle_transport_event(
    publication_runtime: &RemoteTargetPublicationRuntime,
    authority_manager: &mut SessionSyncAuthorityManager,
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    internal_tx: mpsc::Sender<InternalEvent>,
    event: RemoteNodeTransportEvent,
) {
    match event {
        RemoteNodeTransportEvent::SessionOpened { session } => {
            let node_id = session.node_id().to_string();
            let session_instance_id = session.session_instance_id().to_string();
            let mut active = ActiveNodeIngressSession {
                session,
                bridges: HashMap::new(),
            };
            refresh_authority_bridges(&node_id, &mut active, internal_tx);
            sessions.insert(session_instance_id, active);
        }
        RemoteNodeTransportEvent::EnvelopeReceived {
            node_id,
            session_instance_id,
            envelope,
        } => {
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
                let _ =
                    route_transport_envelope(publication_runtime, &node_id, envelope, Some(active));
            } else {
                let _ = route_transport_envelope(publication_runtime, &node_id, envelope, None);
            }
        }
        RemoteNodeTransportEvent::SessionClosed {
            node_id,
            session_instance_id,
        } => {
            sessions.remove(&session_instance_id);
            if !sessions
                .values()
                .any(|active| active.session.node_id() == node_id)
            {
                let _ =
                    publication_runtime.remove_discovered_remote_node_on_live_workspaces(&node_id);
            }
        }
        RemoteNodeTransportEvent::TransportFailed {
            node_id,
            session_instance_id,
            ..
        } => {
            if let Some(session_instance_id) = session_instance_id {
                sessions.remove(&session_instance_id);
            }
            if let Some(node_id) = node_id {
                if !sessions
                    .values()
                    .any(|active| active.session.node_id() == node_id)
                {
                    let _ = publication_runtime
                        .remove_discovered_remote_node_on_live_workspaces(&node_id);
                }
            }
        }
    }
}

fn handle_internal_event(
    sessions: &mut HashMap<String, ActiveNodeIngressSession>,
    internal_tx: mpsc::Sender<InternalEvent>,
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
        InternalEvent::SocketDirChanged => {
            for active in sessions.values_mut() {
                let node_id = active.session.node_id().to_string();
                refresh_authority_bridges(&node_id, active, internal_tx.clone());
            }
        }
        #[cfg(test)]
        InternalEvent::Shutdown => {}
    }
}

fn route_transport_envelope(
    publication_runtime: &RemoteTargetPublicationRuntime,
    node_id: &str,
    envelope: GrpcNodeSessionEnvelope,
    session: Option<&mut ActiveNodeIngressSession>,
) -> Result<(), LifecycleError> {
    match envelope.body.as_ref() {
        Some(Body::TargetPublished(payload)) => {
            let mapped = map_target_published_envelope(node_id, &envelope, payload)
                .map_err(remote_node_ingress_error)?;
            publication_runtime
                .apply_discovered_remote_session_envelope_on_live_workspaces(node_id, mapped)
        }
        Some(Body::TargetExited(payload)) => {
            ERROR_LOG.log(format!(
                "[diag-bug] ingress_server route_transport_envelope: received TargetExited node={node_id} session={}",
                payload.transport_session_id
            ));
            let mapped = map_target_exited_envelope(node_id, &envelope, payload);
            publication_runtime
                .apply_discovered_remote_session_envelope_on_live_workspaces(node_id, mapped)?;
            ERROR_LOG.log(format!(
                "[diag-bug] ingress_server: applied TargetExited to live workspaces node={node_id} session={}",
                payload.transport_session_id
            ));
            let Some(session) = session else {
                return Ok(());
            };
            let session_id = route_session_id(&envelope)
                .or_else(|| payload_session_id(&payload.transport_session_id, &payload.target_id))
                .unwrap_or_else(|| payload.transport_session_id.clone());
            let target_id = route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone());
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

fn refresh_authority_bridges(
    node_id: &str,
    session: &mut ActiveNodeIngressSession,
    internal_tx: mpsc::Sender<InternalEvent>,
) {
    let Ok(socket_paths) = discover_authority_socket_paths(node_id) else {
        return;
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
            Err(_) => continue,
        };
        let transport = Arc::new(transport);
        let reader = transport.clone();
        let transport_session = session.session.clone();
        let node_id_owned = node_id.to_string();
        let socket_path_owned = socket_path.clone();
        let internal_tx_owned = internal_tx.clone();
        thread::spawn(move || {
            loop {
                let command = match reader.recv_command() {
                    Ok(command) => command,
                    Err(_) => break,
                };
                let Ok(envelope) = map_authority_command_to_grpc(&transport_session, command)
                else {
                    break;
                };
                if transport_session.send(envelope).is_err() {
                    break;
                }
            }
            let _ = internal_tx_owned.send(InternalEvent::BridgeClosed {
                node_id: node_id_owned,
                socket_path: socket_path_owned,
            });
        });
        session.bridges.insert(
            socket_path,
            ActiveAuthoritySocketBridge {
                target_component,
                transport,
            },
        );
    }
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
            source_session_name: None,
        }),
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
