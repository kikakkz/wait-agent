use crate::cli::RemoteNetworkConfig;
use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    ApplyPtyResize, NodeSessionEnvelope as GrpcNodeSessionEnvelope, RouteContext,
    TargetExited as GrpcTargetExited, TargetInputDelivery, TargetPublished as GrpcTargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, RemoteNodeSessionHandle,
    RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, TargetExitedPayload, TargetPublishedPayload,
    REMOTE_PROTOCOL_VERSION,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::{
    RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BRIDGE_REFRESH_INTERVAL: Duration = Duration::from_millis(250);

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
}

impl RemoteNodeIngressServerRuntime {
    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            publication_runtime: RemoteTargetPublicationRuntime::from_build_env_with_network(
                network.clone(),
            )?,
            network,
        })
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

    loop {
        match transport_rx.recv_timeout(BRIDGE_REFRESH_INTERVAL) {
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
                    if let Some(active) = sessions.get_mut(&node_id) {
                        refresh_authority_bridges(&node_id, active, internal_tx.clone());
                    }
                    let _ = route_transport_envelope(
                        &publication_runtime,
                        &node_id,
                        envelope,
                        sessions.get_mut(&node_id),
                    );
                }
                RemoteNodeTransportEvent::SessionClosed { node_id } => {
                    let _ = publication_runtime.mark_discovered_remote_node_offline(&node_id);
                    sessions.remove(&node_id);
                }
                RemoteNodeTransportEvent::TransportFailed { node_id, .. } => {
                    if let Some(node_id) = node_id {
                        let _ = publication_runtime.mark_discovered_remote_node_offline(&node_id);
                        sessions.remove(&node_id);
                    }
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                for (node_id, active) in &mut sessions {
                    refresh_authority_bridges(node_id, active, internal_tx.clone());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
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
            publication_runtime.apply_discovered_remote_session_envelope(node_id, mapped)
        }
        Some(Body::TargetExited(payload)) => {
            let mapped = map_target_exited_envelope(node_id, &envelope, payload);
            publication_runtime.apply_discovered_remote_session_envelope(node_id, mapped)
        }
        Some(Body::TargetOutput(payload)) => {
            let Some(session) = session else {
                return Ok(());
            };
            let bytes_base64 = encode_base64(&payload.output_bytes);
            let session_id = route_session_id(&envelope)
                .or_else(|| payload_session_id(&payload.session_id, &payload.target_id))
                .unwrap_or_else(|| payload.target_id.clone());
            let target_id = route_target_id(&envelope).unwrap_or_else(|| payload.target_id.clone());
            let target_component =
                sanitize_socket_component(&format!("remote-peer:{node_id}:{session_id}"));
            let mut stale = Vec::new();
            for (path, bridge) in &session.bridges {
                if bridge.target_component != target_component {
                    continue;
                }
                if let Err(error) = bridge.transport.send_target_output(
                    &session_id,
                    &target_id,
                    payload.output_seq,
                    known_output_stream(&payload.stream).map_err(remote_node_ingress_error)?,
                    bytes_base64.clone(),
                ) {
                    let _ = error;
                    stale.push(path.clone());
                }
            }
            for path in stale {
                session.bridges.remove(&path);
            }
            Ok(())
        }
        Some(Body::Heartbeat(_)) | Some(Body::ClientHello(_)) | Some(Body::ServerHello(_)) => {
            Ok(())
        }
        _ => Ok(()),
    }
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
        let Ok(transport) = RemoteAuthorityTransportRuntime::connect(&socket_path, node_id) else {
            continue;
        };
        let transport = Arc::new(transport);
        let reader = transport.clone();
        let transport_session = session.session.clone();
        let node_id_owned = node_id.to_string();
        let socket_path_owned = socket_path.clone();
        let internal_tx_owned = internal_tx.clone();
        thread::spawn(move || {
            while let Ok(command) = reader.recv_command() {
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
    let target_prefix = sanitize_socket_component(&format!("remote-peer:{authority_id}:"));
    let mut paths = Vec::new();
    for entry in fs::read_dir(std::env::temp_dir())? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with("waitagent-remote-") || !name.ends_with(".sock") {
            continue;
        }
        if !name.contains(&target_prefix) {
            continue;
        }
        paths.push(entry.path());
    }
    Ok(paths)
}

fn extract_target_component(file_name: &str, authority_id: &str) -> Option<String> {
    let prefix = sanitize_socket_component(&format!("remote-peer:{authority_id}:"));
    let start = file_name.find(&prefix)?;
    Some(file_name[start..].trim_end_matches(".sock").to_string())
}

fn sanitize_socket_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn map_authority_command_to_grpc(
    session: &RemoteNodeSessionHandle,
    command: RemoteAuthorityCommand,
) -> Result<GrpcNodeSessionEnvelope, io::Error> {
    let (route, body) = match command {
        RemoteAuthorityCommand::TargetInput(payload) => (
            Some(RouteContext {
                authority_node_id: Some(session.node_id().to_string()),
                target_id: Some(payload.target_id.clone()),
                attachment_id: Some(payload.attachment_id.clone()),
                console_id: Some(payload.console_id.clone()),
                console_host_id: Some(payload.console_host_id.clone()),
                session_id: Some(payload.session_id.clone()),
            }),
            Some(Body::TargetInputDelivery(TargetInputDelivery {
                attachment_id: payload.attachment_id,
                target_id: payload.target_id,
                console_id: payload.console_id,
                console_host_id: payload.console_host_id,
                input_seq: payload.input_seq,
                session_id: payload.session_id,
                input_bytes: decode_base64(&payload.bytes_base64).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?,
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
            session_role: None,
            workspace_key: None,
            command_name: payload.command_name.clone(),
            current_path: payload.current_path.clone(),
            attached_clients: payload.attached_count.unwrap_or(0) as usize,
            window_count: 0,
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
mod tests {
    use super::{discover_authority_socket_paths, extract_target_component};
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn extracts_target_component_for_authority_socket_file() {
        let component = extract_target_component(
            "waitagent-remote-wa-1-workspace-remote-peer_peer-a_target-1.sock",
            "peer-a",
        );

        assert_eq!(component.as_deref(), Some("remote-peer_peer-a_target-1"));
    }

    #[test]
    fn authority_socket_discovery_filters_to_authority() {
        let matching_a =
            temp_dir_path("waitagent-remote-wa-a-workspace-remote-peer_peer-a_target-1");
        let matching_b =
            temp_dir_path("waitagent-remote-wa-b-server-console-remote-peer_peer-a_target-2");
        let different_authority =
            temp_dir_path("waitagent-remote-wa-c-workspace-remote-peer_peer-b_target-1");
        fs::write(&matching_a, b"").expect("matching file should write");
        fs::write(&matching_b, b"").expect("second matching file should write");
        fs::write(&different_authority, b"").expect("other authority file should write");

        let mut paths = discover_authority_socket_paths("peer-a")
            .expect("authority socket discovery should succeed");
        paths.sort();

        assert_eq!(paths, vec![matching_a.clone(), matching_b.clone()]);

        let _ = fs::remove_file(matching_a);
        let _ = fs::remove_file(matching_b);
        let _ = fs::remove_file(different_authority);
    }

    fn temp_dir_path(file_name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{file_name}-{}-{unique}.sock", process::id()))
    }
}
