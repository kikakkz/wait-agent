use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
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
    ControlPlanePayload, ProtocolEnvelope, TargetExitedPayload, TargetPublishedPayload,
    REMOTE_PROTOCOL_VERSION,
};
use crate::infra::tmux::EmbeddedTmuxBackend;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::{
    authority_target_component, RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
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
const WATCHER_SLEEP_ON_EMPTY: Duration = Duration::from_millis(200);
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
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
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
/// On Linux this uses inotify for zero-wakeup event-driven discovery. On other
/// platforms (macOS, BSD) it falls back to a low-frequency polling loop since
/// those are development-only targets; production deployment is Linux-only.
fn start_socket_watcher(
    internal_tx: mpsc::Sender<InternalEvent>,
) -> io::Result<thread::JoinHandle<()>> {
    #[cfg(target_os = "linux")]
    return start_inotify_watcher(internal_tx);

    #[cfg(not(target_os = "linux"))]
    Ok(start_periodic_watcher(internal_tx))
}

/// Linux inotify-based watcher — zero wakeups when the directory is idle.
#[cfg(target_os = "linux")]
fn start_inotify_watcher(
    internal_tx: mpsc::Sender<InternalEvent>,
) -> io::Result<thread::JoinHandle<()>> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK) };
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
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    thread::sleep(WATCHER_SLEEP_ON_EMPTY);
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

/// Fallback periodic watcher for non-Linux platforms (macOS, BSD). Polls the
/// temp directory at a low frequency since these are dev-only targets.
#[cfg(not(target_os = "linux"))]
fn start_periodic_watcher(internal_tx: mpsc::Sender<InternalEvent>) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        let _ = internal_tx.send(InternalEvent::SocketDirChanged);
        thread::sleep(Duration::from_secs(1));
    })
}

impl Drop for RemoteNodeIngressServerGuard {
    fn drop(&mut self) {
        let _ = self.transport_guard.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_node_ingress_server_loop(
    publication_runtime: RemoteTargetPublicationRuntime,
    transport_rx: mpsc::Receiver<RemoteNodeTransportEvent>,
    internal_rx: mpsc::Receiver<InternalEvent>,
    internal_tx: mpsc::Sender<InternalEvent>,
) {
    let mut sessions = HashMap::<String, ActiveNodeIngressSession>::new();

    // Start the inotify watcher to detect new authority socket files without polling.
    let _watcher = start_socket_watcher(internal_tx.clone()).ok();

    loop {
        match transport_rx.recv() {
            Ok(event) => match event {
                RemoteNodeTransportEvent::SessionOpened { session } => {
                    let node_id = session.node_id().to_string();
                    let mut active = ActiveNodeIngressSession {
                        session,
                        bridges: HashMap::new(),
                    };
                    refresh_authority_bridges(&node_id, &mut active, internal_tx.clone());
                    sessions.insert(node_id, active);
                }
                RemoteNodeTransportEvent::EnvelopeReceived { node_id, envelope } => {
                    if !matches!(envelope.body.as_ref(), Some(Body::RawPtyInput(_))) {
                        if let Some(active) = sessions.get_mut(&node_id) {
                            refresh_authority_bridges(&node_id, active, internal_tx.clone());
                        }
                    }
                    let _ = route_transport_envelope(
                        &publication_runtime,
                        &node_id,
                        envelope,
                        sessions.get_mut(&node_id),
                    );
                }
                RemoteNodeTransportEvent::SessionClosed { node_id } => {
                    let _ = publication_runtime
                        .remove_discovered_remote_node_on_live_workspaces(&node_id);
                    sessions.remove(&node_id);
                }
                RemoteNodeTransportEvent::TransportFailed { node_id, .. } => {
                    if let Some(node_id) = node_id {
                        let _ = publication_runtime
                            .remove_discovered_remote_node_on_live_workspaces(&node_id);
                        sessions.remove(&node_id);
                    }
                }
            },
            Err(mpsc::RecvError) => return,
        }

        while let Ok(event) = internal_rx.try_recv() {
            match event {
                InternalEvent::BridgeClosed {
                    node_id,
                    socket_path,
                } => {
                    if let Some(active) = sessions.get_mut(&node_id) {
                        active.bridges.remove(&socket_path);
                    }
                }
                InternalEvent::SocketDirChanged => {
                    for (node_id, active) in &mut sessions {
                        refresh_authority_bridges(node_id, active, internal_tx.clone());
                    }
                }
            }
        }
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
            let mapped = map_target_exited_envelope(node_id, &envelope, payload);
            publication_runtime
                .apply_discovered_remote_session_envelope_on_live_workspaces(node_id, mapped)
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
    let mut parts = target_id.splitn(3, ':');
    let _transport = parts.next()?;
    let _authority = parts.next()?;
    let session_id = parts.next()?;
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
