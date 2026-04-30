use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body as GrpcBody;
use crate::infra::remote_grpc_proto::v1::{
    ApplyPtyResize, NodeSessionEnvelope as GrpcNodeSessionEnvelope, RouteContext,
    TargetExited as GrpcTargetExited, TargetInputDelivery, TargetOutput as GrpcTargetOutput,
    TargetPublished as GrpcTargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, OutboundNodeSessionRequest,
    RemoteNodeSessionHandle, RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{
    ApplyResizePayload, ControlPlanePayload, NodeSessionChannel, NodeSessionEnvelope,
    ProtocolEnvelope, TargetExitedPayload, TargetInputPayload, TargetOutputPayload,
    TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, read_node_session_envelope, write_control_plane_envelope,
    write_node_session_envelope, write_registration_frame, RemoteTransportCodecError,
};
use crate::runtime::remote_authority_connection_runtime::QueuedAuthorityStreamSink;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_transport_runtime::{
    read_client_hello, read_server_hello, write_client_hello, write_server_hello,
};
use crate::runtime::remote_target_publication_runtime::PublicationSenderCommand;
use std::fmt;
use std::fs;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const NODE_SESSION_SERVER_ID: &str = "waitagent-remote-node-session";

pub trait RemoteNodePublicationSink: Send + Sync + 'static {
    fn publish(
        &self,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), RemoteNodeSessionError>;
}

pub struct RemoteNodeSessionRuntime {
    node_id: String,
    transport: RemoteNodeSessionTransport,
    next_message_id: AtomicU64,
}

enum RemoteNodeSessionTransport {
    Local(LocalRemoteNodeSessionTransport),
    Grpc(GrpcRemoteNodeSessionTransport),
}

struct LocalRemoteNodeSessionTransport {
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
}

struct GrpcRemoteNodeSessionTransport {
    session: RemoteNodeSessionHandle,
    authority_rx: Mutex<mpsc::Receiver<GrpcAuthorityEvent>>,
    guard: Mutex<Option<GrpcRemoteNodeTransportGuard>>,
}

enum GrpcAuthorityEvent {
    Command(RemoteAuthorityCommand),
    Failed(String),
    Closed,
}

pub struct RemoteNodeSessionListenerGuard {
    socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNodeSessionError {
    message: String,
}

impl RemoteNodeSessionRuntime {
    pub fn connect(
        socket_path: impl AsRef<Path>,
        node_id: impl Into<String>,
        server_endpoint: Option<&str>,
    ) -> Result<Self, RemoteNodeSessionError> {
        let node_id = node_id.into();
        let transport = match server_endpoint {
            Some(endpoint_uri) => {
                RemoteNodeSessionTransport::Grpc(connect_grpc_node_session(&node_id, endpoint_uri)?)
            }
            None => RemoteNodeSessionTransport::Local(connect_local_node_session(
                socket_path,
                &node_id,
            )?),
        };
        Ok(Self {
            node_id,
            transport,
            next_message_id: AtomicU64::new(0),
        })
    }

    pub fn recv_authority_command(&self) -> Result<RemoteAuthorityCommand, RemoteNodeSessionError> {
        match &self.transport {
            RemoteNodeSessionTransport::Local(transport) => recv_local_authority_command(transport),
            RemoteNodeSessionTransport::Grpc(transport) => {
                let event = transport
                    .authority_rx
                    .lock()
                    .expect("grpc authority receiver mutex should not be poisoned")
                    .recv()
                    .map_err(|_| {
                        RemoteNodeSessionError::new(
                            "grpc node session authority stream closed unexpectedly",
                        )
                    })?;
                match event {
                    GrpcAuthorityEvent::Command(command) => Ok(command),
                    GrpcAuthorityEvent::Failed(message) => {
                        Err(RemoteNodeSessionError::new(message))
                    }
                    GrpcAuthorityEvent::Closed => Err(RemoteNodeSessionError::new(
                        "grpc node session closed by remote peer",
                    )),
                }
            }
        }
    }

    pub fn send_target_output(
        &self,
        session_id: &str,
        target_id: &str,
        output_seq: u64,
        stream: &'static str,
        bytes_base64: impl Into<String>,
    ) -> Result<(), RemoteNodeSessionError> {
        self.send_payload(
            NodeSessionChannel::Authority,
            session_id,
            target_id,
            "authority-msg",
            ControlPlanePayload::TargetOutput(TargetOutputPayload {
                session_id: session_id.to_string(),
                target_id: target_id.to_string(),
                output_seq,
                stream,
                bytes_base64: bytes_base64.into(),
            }),
        )
    }

    pub fn send_target_published(
        &self,
        target: &ManagedSessionRecord,
        source_session_name: Option<&str>,
    ) -> Result<(), RemoteNodeSessionError> {
        let current_path = target
            .current_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        self.send_payload(
            NodeSessionChannel::Publication,
            target.address.session_id(),
            target.address.id().as_str(),
            "publication-msg",
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                transport_session_id: target.address.session_id().to_string(),
                source_session_name: source_session_name.map(str::to_string),
                selector: target.selector.clone(),
                availability: target.availability.as_str(),
                session_role: target
                    .session_role
                    .as_ref()
                    .map(WorkspaceSessionRole::as_str),
                workspace_key: target.workspace_key.clone(),
                command_name: target.command_name.clone(),
                current_path,
                attached_clients: target.attached_clients,
                window_count: target.window_count,
                task_state: target.task_state.as_str(),
            }),
        )
    }

    pub fn send_target_exited(
        &self,
        transport_session_id: &str,
        source_session_name: Option<&str>,
    ) -> Result<(), RemoteNodeSessionError> {
        let target_id = format!("remote-peer:{}:{transport_session_id}", self.node_id);
        self.send_payload(
            NodeSessionChannel::Publication,
            transport_session_id,
            &target_id,
            "publication-msg",
            ControlPlanePayload::TargetExited(TargetExitedPayload {
                transport_session_id: transport_session_id.to_string(),
                source_session_name: source_session_name.map(str::to_string),
            }),
        )
    }

    pub(crate) fn send_publication_sender_command(
        &self,
        command: &PublicationSenderCommand,
    ) -> Result<(), RemoteNodeSessionError> {
        match command {
            PublicationSenderCommand::PublishTarget {
                authority_id,
                transport_session_id,
                source_session_name,
                selector,
                availability,
                session_role,
                workspace_key,
                command_name,
                current_path,
                attached_clients,
                window_count,
                task_state,
            } => self.send_payload(
                NodeSessionChannel::Publication,
                transport_session_id,
                &format!("remote-peer:{authority_id}:{transport_session_id}"),
                "publication-msg",
                ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                    transport_session_id: transport_session_id.clone(),
                    source_session_name: source_session_name.clone(),
                    selector: selector.clone(),
                    availability,
                    session_role: *session_role,
                    workspace_key: workspace_key.clone(),
                    command_name: command_name.clone(),
                    current_path: current_path.clone(),
                    attached_clients: *attached_clients,
                    window_count: *window_count,
                    task_state,
                }),
            ),
            PublicationSenderCommand::ExitTarget {
                authority_id,
                transport_session_id,
                source_session_name,
            } => self.send_payload(
                NodeSessionChannel::Publication,
                transport_session_id,
                &format!("remote-peer:{authority_id}:{transport_session_id}"),
                "publication-msg",
                ControlPlanePayload::TargetExited(TargetExitedPayload {
                    transport_session_id: transport_session_id.clone(),
                    source_session_name: source_session_name.clone(),
                }),
            ),
            PublicationSenderCommand::RegisterLiveSession { .. }
            | PublicationSenderCommand::UnregisterLiveSession { .. } => {
                Err(RemoteNodeSessionError::new(
                    "live session registration commands cannot be sent over the node session",
                ))
            }
        }
    }

    pub fn shutdown(&self) {
        match &self.transport {
            RemoteNodeSessionTransport::Local(transport) => {
                let _ = transport
                    .reader
                    .lock()
                    .expect("node session reader mutex should not be poisoned")
                    .shutdown(Shutdown::Both);
                let _ = transport
                    .writer
                    .lock()
                    .expect("node session writer mutex should not be poisoned")
                    .shutdown(Shutdown::Both);
            }
            RemoteNodeSessionTransport::Grpc(transport) => {
                let _ = transport
                    .guard
                    .lock()
                    .expect("grpc node session guard mutex should not be poisoned")
                    .take();
            }
        }
    }

    #[cfg(test)]
    fn connect_grpc_for_tests(
        endpoint_uri: &str,
        node_id: impl Into<String>,
    ) -> Result<Self, RemoteNodeSessionError> {
        let node_id = node_id.into();
        Ok(Self {
            transport: RemoteNodeSessionTransport::Grpc(connect_grpc_node_session(
                &node_id,
                endpoint_uri,
            )?),
            node_id,
            next_message_id: AtomicU64::new(0),
        })
    }

    fn send_payload(
        &self,
        channel: NodeSessionChannel,
        session_id: &str,
        target_id: &str,
        message_scope: &str,
        payload: ControlPlanePayload,
    ) -> Result<(), RemoteNodeSessionError> {
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!(
                "{}-{}-{}",
                self.node_id,
                message_scope,
                self.next_message_id.fetch_add(1, Ordering::Relaxed) + 1
            ),
            message_type: payload.message_type(),
            timestamp: now_rfc3339_like(),
            sender_id: self.node_id.clone(),
            correlation_id: None,
            session_id: Some(session_id.to_string()),
            target_id: Some(target_id.to_string()),
            attachment_id: None,
            console_id: None,
            payload,
        };
        match &self.transport {
            RemoteNodeSessionTransport::Local(transport) => {
                let mut writer = transport
                    .writer
                    .lock()
                    .expect("node session writer mutex should not be poisoned");
                write_node_session_envelope(
                    &mut *writer,
                    &NodeSessionEnvelope { channel, envelope },
                )?;
                Ok(())
            }
            RemoteNodeSessionTransport::Grpc(transport) => transport
                .session
                .send(map_outbound_grpc_envelope(
                    &self.node_id,
                    channel,
                    &envelope,
                )?)
                .map_err(|error| RemoteNodeSessionError::new(error.to_string())),
        }
    }
}

fn connect_local_node_session(
    socket_path: impl AsRef<Path>,
    node_id: &str,
) -> Result<LocalRemoteNodeSessionTransport, RemoteNodeSessionError> {
    let mut stream = UnixStream::connect(socket_path)?;
    write_client_hello(&mut stream, node_id)?;
    let _server_hello = read_server_hello(&mut stream)?;
    let writer = stream.try_clone()?;
    Ok(LocalRemoteNodeSessionTransport {
        reader: Mutex::new(stream),
        writer: Mutex::new(writer),
    })
}

fn connect_grpc_node_session(
    node_id: &str,
    endpoint_uri: &str,
) -> Result<GrpcRemoteNodeSessionTransport, RemoteNodeSessionError> {
    let transport = GrpcRemoteNodeTransport::new();
    let (event_tx, event_rx) = mpsc::channel();
    let guard = transport
        .connect_outbound(
            OutboundNodeSessionRequest {
                node_id: node_id.to_string(),
                endpoint_uri: endpoint_uri.to_string(),
            },
            event_tx,
        )
        .map_err(|error| RemoteNodeSessionError::new(error.to_string()))?;

    let event_rx = event_rx;
    let session = loop {
        match event_rx.recv() {
            Ok(RemoteNodeTransportEvent::SessionOpened { session }) => break session,
            Ok(RemoteNodeTransportEvent::EnvelopeReceived { .. }) => continue,
            Ok(RemoteNodeTransportEvent::TransportFailed { message, .. }) => {
                return Err(RemoteNodeSessionError::new(message))
            }
            Ok(RemoteNodeTransportEvent::SessionClosed { .. }) => {
                return Err(RemoteNodeSessionError::new(
                    "grpc node session closed before startup completed",
                ))
            }
            Err(_) => {
                return Err(RemoteNodeSessionError::new(
                    "grpc node session worker exited before startup completed",
                ))
            }
        }
    };

    let (authority_tx, authority_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            match event {
                RemoteNodeTransportEvent::EnvelopeReceived { envelope, .. } => {
                    if let Some(result) = map_inbound_grpc_authority_event(envelope) {
                        let _ = authority_tx.send(result);
                    }
                }
                RemoteNodeTransportEvent::TransportFailed { message, .. } => {
                    let _ = authority_tx.send(GrpcAuthorityEvent::Failed(message));
                    return;
                }
                RemoteNodeTransportEvent::SessionClosed { .. } => {
                    let _ = authority_tx.send(GrpcAuthorityEvent::Closed);
                    return;
                }
                RemoteNodeTransportEvent::SessionOpened { .. } => continue,
            }
        }
        let _ = authority_tx.send(GrpcAuthorityEvent::Closed);
    });

    Ok(GrpcRemoteNodeSessionTransport {
        session,
        authority_rx: Mutex::new(authority_rx),
        guard: Mutex::new(Some(guard)),
    })
}

fn recv_local_authority_command(
    transport: &LocalRemoteNodeSessionTransport,
) -> Result<RemoteAuthorityCommand, RemoteNodeSessionError> {
    let mut reader = transport
        .reader
        .lock()
        .expect("node session reader mutex should not be poisoned");
    let session_envelope = read_node_session_envelope(&mut *reader)?;
    if session_envelope.channel != NodeSessionChannel::Authority {
        return Err(RemoteNodeSessionError::new(format!(
            "unexpected node session channel `{}` while waiting for authority command",
            session_envelope.channel.as_str()
        )));
    }
    match session_envelope.envelope.payload {
        ControlPlanePayload::TargetInput(payload) => {
            Ok(RemoteAuthorityCommand::TargetInput(payload))
        }
        ControlPlanePayload::ApplyResize(payload) => {
            Ok(RemoteAuthorityCommand::ApplyResize(payload))
        }
        other => Err(RemoteNodeSessionError::new(format!(
            "unexpected authority command `{}`",
            other.message_type()
        ))),
    }
}

fn map_inbound_grpc_authority_event(
    envelope: GrpcNodeSessionEnvelope,
) -> Option<GrpcAuthorityEvent> {
    let route_session_id = route_session_id(&envelope);
    match envelope.body {
        Some(GrpcBody::TargetInputDelivery(payload)) => Some(GrpcAuthorityEvent::Command(
            RemoteAuthorityCommand::TargetInput(TargetInputPayload {
                attachment_id: payload.attachment_id,
                session_id: grpc_session_id(
                    route_session_id,
                    &payload.session_id,
                    &payload.target_id,
                ),
                target_id: payload.target_id,
                console_id: payload.console_id,
                console_host_id: payload.console_host_id,
                input_seq: payload.input_seq,
                bytes_base64: encode_base64(&payload.input_bytes),
            }),
        )),
        Some(GrpcBody::ApplyPtyResize(payload)) => Some(GrpcAuthorityEvent::Command(
            RemoteAuthorityCommand::ApplyResize(ApplyResizePayload {
                session_id: grpc_session_id(
                    route_session_id,
                    &payload.session_id,
                    &payload.target_id,
                ),
                target_id: payload.target_id,
                resize_epoch: payload.resize_epoch,
                resize_authority_console_id: payload.resize_authority_console_id,
                cols: payload.cols as usize,
                rows: payload.rows as usize,
            }),
        )),
        Some(GrpcBody::ServerHello(_)) | Some(GrpcBody::Heartbeat(_)) => None,
        Some(other) => Some(GrpcAuthorityEvent::Failed(format!(
            "unexpected grpc authority envelope `{other:?}`",
        ))),
        None => None,
    }
}

fn map_outbound_grpc_envelope(
    node_id: &str,
    channel: NodeSessionChannel,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<GrpcNodeSessionEnvelope, RemoteNodeSessionError> {
    let route = Some(RouteContext {
        authority_node_id: Some(node_id.to_string()),
        target_id: envelope.target_id.clone(),
        attachment_id: envelope.attachment_id.clone(),
        console_id: envelope.console_id.clone(),
        console_host_id: match &envelope.payload {
            ControlPlanePayload::TargetInput(payload) => Some(payload.console_host_id.clone()),
            _ => None,
        },
        session_id: envelope.session_id.clone(),
    });
    let body = match (&channel, &envelope.payload) {
        (NodeSessionChannel::Authority, ControlPlanePayload::TargetOutput(payload)) => {
            Some(GrpcBody::TargetOutput(GrpcTargetOutput {
                target_id: payload.target_id.clone(),
                output_seq: payload.output_seq,
                stream: payload.stream.to_string(),
                session_id: payload.session_id.clone(),
                output_bytes: decode_base64(&payload.bytes_base64)
                    .map_err(|error| RemoteNodeSessionError::new(error.to_string()))?,
            }))
        }
        (NodeSessionChannel::Authority, ControlPlanePayload::TargetInput(payload)) => {
            Some(GrpcBody::TargetInputDelivery(TargetInputDelivery {
                attachment_id: payload.attachment_id.clone(),
                target_id: payload.target_id.clone(),
                console_id: payload.console_id.clone(),
                console_host_id: payload.console_host_id.clone(),
                input_seq: payload.input_seq,
                session_id: payload.session_id.clone(),
                input_bytes: decode_base64(&payload.bytes_base64)
                    .map_err(|error| RemoteNodeSessionError::new(error.to_string()))?,
            }))
        }
        (NodeSessionChannel::Authority, ControlPlanePayload::ApplyResize(payload)) => {
            Some(GrpcBody::ApplyPtyResize(ApplyPtyResize {
                target_id: payload.target_id.clone(),
                resize_epoch: payload.resize_epoch,
                resize_authority_console_id: payload.resize_authority_console_id.clone(),
                cols: payload.cols as u32,
                rows: payload.rows as u32,
                session_id: payload.session_id.clone(),
            }))
        }
        (NodeSessionChannel::Publication, ControlPlanePayload::TargetPublished(payload)) => {
            Some(GrpcBody::TargetPublished(GrpcTargetPublished {
                target_id: envelope.target_id.clone().unwrap_or_else(|| {
                    format!("remote-peer:{node_id}:{}", payload.transport_session_id)
                }),
                authority_node_id: node_id.to_string(),
                transport: "grpc".to_string(),
                transport_session_id: payload.transport_session_id.clone(),
                selector: payload.selector.clone(),
                availability: payload.availability.to_string(),
                command_name: payload.command_name.clone(),
                current_path: payload.current_path.clone(),
                attached_count: Some(payload.attached_clients as u64),
                session_role: payload.session_role.map(str::to_string),
                workspace_key: payload.workspace_key.clone(),
                window_count: Some(payload.window_count as u64),
                task_state: Some(payload.task_state.to_string()),
            }))
        }
        (NodeSessionChannel::Publication, ControlPlanePayload::TargetExited(payload)) => {
            Some(GrpcBody::TargetExited(GrpcTargetExited {
                target_id: envelope.target_id.clone().unwrap_or_else(|| {
                    format!("remote-peer:{node_id}:{}", payload.transport_session_id)
                }),
                transport_session_id: payload.transport_session_id.clone(),
            }))
        }
        _ => None,
    }
    .ok_or_else(|| {
        RemoteNodeSessionError::new(format!(
            "unsupported grpc outbound node-session payload `{}` on channel `{}`",
            envelope.payload.message_type(),
            channel.as_str()
        ))
    })?;

    Ok(GrpcNodeSessionEnvelope {
        message_id: envelope.message_id.clone(),
        sent_at: Some(timestamp_now()),
        session_instance_id: format!("client-session-{}", now_millis()),
        correlation_id: envelope.correlation_id.clone(),
        route,
        body: Some(body),
    })
}

fn route_session_id(envelope: &GrpcNodeSessionEnvelope) -> Option<String> {
    envelope
        .route
        .as_ref()
        .and_then(|route| route.session_id.clone())
}

fn grpc_session_id(
    route_session_id: Option<String>,
    payload_session_id: &str,
    target_id: &str,
) -> String {
    route_session_id
        .filter(|session_id| !session_id.is_empty())
        .or_else(|| {
            if payload_session_id.is_empty() {
                None
            } else {
                Some(payload_session_id.to_string())
            }
        })
        .or_else(|| derive_session_id_from_target_id(target_id))
        .unwrap_or_else(|| target_id.to_string())
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

pub fn spawn_remote_node_session_listener(
    socket_path: PathBuf,
    authority_sink: QueuedAuthorityStreamSink,
    publication_sink: Arc<dyn RemoteNodePublicationSink>,
) -> io::Result<RemoteNodeSessionListenerGuard> {
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path)?;
    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(stream) = accepted else {
                break;
            };
            let authority_sink = authority_sink.clone();
            let publication_sink = publication_sink.clone();
            thread::spawn(move || {
                let _ = bridge_remote_node_session(stream, authority_sink, publication_sink);
            });
        }
    });
    Ok(RemoteNodeSessionListenerGuard { socket_path })
}

impl RemoteNodeSessionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteNodeSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteNodeSessionError {}

impl From<io::Error> for RemoteNodeSessionError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<RemoteTransportCodecError> for RemoteNodeSessionError {
    fn from(value: RemoteTransportCodecError) -> Self {
        Self::new(value.to_string())
    }
}

impl Drop for RemoteNodeSessionListenerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn bridge_remote_node_session(
    mut transport_stream: UnixStream,
    authority_sink: QueuedAuthorityStreamSink,
    publication_sink: Arc<dyn RemoteNodePublicationSink>,
) -> Result<(), RemoteNodeSessionError> {
    let node_id = read_client_hello(&mut transport_stream)?;

    let (mut local_reader, pane_stream) = UnixStream::pair()?;
    authority_sink
        .submit(pane_stream)
        .map_err(|_| RemoteNodeSessionError::new("authority stream consumer is unavailable"))?;
    write_registration_frame(&mut local_reader, &node_id)?;

    let mut transport_writer = transport_stream.try_clone()?;
    write_server_hello(&mut transport_writer, NODE_SESSION_SERVER_ID)?;
    let local_writer = local_reader.try_clone()?;
    let authority_forward =
        thread::spawn(move || forward_authority_from_local(local_reader, transport_writer));
    let network_result = forward_network_session(local_writer, transport_stream, publication_sink);
    let _ = authority_forward.join();
    network_result
}

fn forward_authority_from_local(
    mut reader: UnixStream,
    mut writer: UnixStream,
) -> Result<(), RemoteNodeSessionError> {
    while let Ok(envelope) = read_control_plane_envelope(&mut reader) {
        write_node_session_envelope(
            &mut writer,
            &NodeSessionEnvelope {
                channel: NodeSessionChannel::Authority,
                envelope,
            },
        )?;
    }
    Ok(())
}

fn forward_network_session(
    mut authority_writer: UnixStream,
    mut transport_reader: UnixStream,
    publication_sink: Arc<dyn RemoteNodePublicationSink>,
) -> Result<(), RemoteNodeSessionError> {
    while let Ok(session_envelope) = read_node_session_envelope(&mut transport_reader) {
        match session_envelope.channel {
            NodeSessionChannel::Authority => {
                write_control_plane_envelope(&mut authority_writer, &session_envelope.envelope)?;
            }
            NodeSessionChannel::Publication => {
                publication_sink.publish(session_envelope.envelope)?;
            }
        }
    }
    Ok(())
}

fn now_rfc3339_like() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
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

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::{
        spawn_remote_node_session_listener, RemoteNodePublicationSink, RemoteNodeSessionRuntime,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::remote_protocol::{
        ControlPlanePayload, ProtocolEnvelope, TargetInputPayload, TargetPublishedPayload,
    };
    use crate::runtime::remote_authority_connection_runtime::{
        AuthorityConnectionRequest, AuthorityConnectionStarter, AuthorityTransportEvent,
        QueuedAuthorityStreamSource, QueuedAuthorityStreamStarter,
        RemoteAuthorityConnectionRuntime,
    };
    use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneSink;
    use crate::runtime::remote_node_ingress_runtime::{
        GrpcRemoteNodeIngressSource, RemoteNodeIngressSource,
    };
    use crate::runtime::remote_transport_runtime::{
        RegistryRemoteControlPlaneSink, RemoteConnectionRegistry,
    };
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    struct RecordingPublicationSink {
        tx: Mutex<mpsc::Sender<ProtocolEnvelope<ControlPlanePayload>>>,
    }

    impl RemoteNodePublicationSink for RecordingPublicationSink {
        fn publish(
            &self,
            envelope: ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<(), super::RemoteNodeSessionError> {
            self.tx
                .lock()
                .expect("publication sink mutex should not be poisoned")
                .send(envelope)
                .map_err(|_| super::RemoteNodeSessionError::new("publication test receiver closed"))
        }
    }

    #[test]
    fn single_outer_session_carries_authority_and_publication() {
        let socket_path = test_socket_path("mixed");
        let (authority_starter, authority_sink) = QueuedAuthorityStreamStarter::channel();
        let (tx, rx) = mpsc::channel();
        let publication_sink: Arc<dyn RemoteNodePublicationSink> =
            Arc::new(RecordingPublicationSink { tx: Mutex::new(tx) });
        let registry = RemoteConnectionRegistry::new();
        let (event_tx, event_rx) = mpsc::channel();
        let _guard = authority_starter
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: PathBuf::from("/tmp/unused.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                event_tx,
            )
            .expect("authority starter should start");
        let _listener = spawn_remote_node_session_listener(
            socket_path.clone(),
            authority_sink,
            publication_sink,
        )
        .expect("node session listener should bind");

        let session = Arc::new(
            RemoteNodeSessionRuntime::connect(&socket_path, "peer-a", None)
                .expect("node session should connect"),
        );

        let session_writer = session.clone();
        let authority_thread = thread::spawn(move || {
            while let Ok(event) = event_rx.recv_timeout(TEST_TIMEOUT) {
                if let AuthorityTransportEvent::Connected = event {
                    break;
                }
            }
            let connection = registry
                .connection_for("peer-a")
                .expect("authority connection should register");
            connection
                .send(&ProtocolEnvelope {
                    protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION
                        .to_string(),
                    message_id: "msg-in".to_string(),
                    message_type: "target_input",
                    timestamp: "1Z".to_string(),
                    sender_id: "observer".to_string(),
                    correlation_id: None,
                    session_id: Some("shell-1".to_string()),
                    target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                    attachment_id: Some("att-1".to_string()),
                    console_id: Some("console-1".to_string()),
                    payload: ControlPlanePayload::TargetInput(TargetInputPayload {
                        attachment_id: "att-1".to_string(),
                        session_id: "shell-1".to_string(),
                        target_id: "remote-peer:peer-a:shell-1".to_string(),
                        console_id: "console-1".to_string(),
                        console_host_id: "wa-local".to_string(),
                        input_seq: 1,
                        bytes_base64: "YQ==".to_string(),
                    }),
                })
                .expect("authority input should send");
            session_writer
                .send_target_published(&remote_target("peer-a", "shell-1"), Some("target-host-1"))
                .expect("publication should send");
        });

        match session
            .recv_authority_command()
            .expect("authority command should arrive")
        {
            crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand::TargetInput(
                payload,
            ) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
            }
            other => panic!("unexpected authority command: {other:?}"),
        }
        authority_thread
            .join()
            .expect("authority helper thread should join cleanly");
        let envelope = rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("publication envelope should arrive");
        match envelope.payload {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                transport_session_id,
                source_session_name,
                ..
            }) => {
                assert_eq!(transport_session_id, "shell-1");
                assert_eq!(source_session_name.as_deref(), Some("target-host-1"));
            }
            other => panic!("unexpected publication payload: {other:?}"),
        }
    }

    #[test]
    fn grpc_session_runtime_bridges_authority_and_publication_through_ingress_boundary() {
        let bind_addr = unused_local_addr();
        let source = GrpcRemoteNodeIngressSource::new(bind_addr);
        let (authority_source, authority_sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(authority_source);
        let registry = RemoteConnectionRegistry::new();
        let (event_tx, event_rx) = mpsc::channel();
        let _connection_guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-grpc-node-ingress.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                event_tx,
            )
            .expect("queued authority runtime should start");
        let (publication_tx, publication_rx) = mpsc::channel();
        let publication_sink: Arc<dyn RemoteNodePublicationSink> =
            Arc::new(RecordingPublicationSink {
                tx: Mutex::new(publication_tx),
            });
        let _guard = source
            .start(
                std::env::temp_dir().join("unused-grpc-node-session.sock"),
                authority_sink,
                publication_sink,
            )
            .expect("grpc ingress source should start");

        let session = Arc::new(
            RemoteNodeSessionRuntime::connect_grpc_for_tests(
                &format!("http://{bind_addr}"),
                "peer-a",
            )
            .expect("grpc node session should connect"),
        );

        while let Ok(event) = event_rx.recv_timeout(TEST_TIMEOUT) {
            if let AuthorityTransportEvent::Connected = event {
                break;
            }
        }

        session
            .send_target_output("shell-1", "remote-peer:peer-a:shell-1", 7, "pty", "YQ==")
            .expect("target output should send through grpc node session");
        let authority_event = event_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("authority output should arrive");
        match authority_event {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                    assert_eq!(payload.output_seq, 7);
                    assert_eq!(payload.bytes_base64, "YQ==");
                }
                other => panic!("unexpected authority envelope payload: {other:?}"),
            },
            other => panic!("unexpected authority transport event: {other:?}"),
        }

        RegistryRemoteControlPlaneSink::new(registry.clone())
            .send(&[
                crate::infra::remote_protocol::NodeBoundControlPlaneMessage {
                    node_id: "peer-a".to_string(),
                    envelope: ProtocolEnvelope {
                        protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION
                            .to_string(),
                        message_id: "target-input-1".to_string(),
                        message_type: "target_input",
                        timestamp: "1Z".to_string(),
                        sender_id: "server".to_string(),
                        correlation_id: None,
                        session_id: Some("shell-1".to_string()),
                        target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                        attachment_id: Some("attach-1".to_string()),
                        console_id: Some("console-1".to_string()),
                        payload: ControlPlanePayload::TargetInput(TargetInputPayload {
                            attachment_id: "attach-1".to_string(),
                            session_id: "shell-1".to_string(),
                            target_id: "remote-peer:peer-a:shell-1".to_string(),
                            console_id: "console-1".to_string(),
                            console_host_id: "host-1".to_string(),
                            input_seq: 3,
                            bytes_base64: "Yg==".to_string(),
                        }),
                    },
                },
            ])
            .expect("target input should route through grpc node session");
        match session
            .recv_authority_command()
            .expect("authority command should arrive over grpc")
        {
            crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand::TargetInput(
                payload,
            ) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.input_seq, 3);
                assert_eq!(payload.bytes_base64, "Yg==");
            }
            other => panic!("unexpected authority command: {other:?}"),
        }

        session
            .send_target_published(&remote_target("peer-a", "shell-1"), Some("target-host-1"))
            .expect("publication should send through grpc node session");
        let envelope = publication_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("publication envelope should arrive");
        match envelope.payload {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                transport_session_id,
                source_session_name,
                selector,
                availability,
                command_name,
                current_path,
                attached_clients,
                window_count,
                session_role,
                workspace_key,
                task_state,
            }) => {
                assert_eq!(transport_session_id, "shell-1");
                assert_eq!(source_session_name, None);
                assert_eq!(selector.as_deref(), Some("wk:shell"));
                assert_eq!(availability, "online");
                assert_eq!(command_name.as_deref(), Some("codex"));
                assert_eq!(current_path.as_deref(), Some("/tmp/demo"));
                assert_eq!(attached_clients, 2);
                assert_eq!(window_count, 1);
                assert_eq!(session_role, Some("target-host"));
                assert_eq!(workspace_key.as_deref(), Some("wk-1"));
                assert_eq!(task_state, "unknown");
            }
            other => panic!("unexpected publication payload: {other:?}"),
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some("wk:shell".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 2,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Unknown,
        }
    }

    fn test_socket_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-remote-node-session-test-{}-{}.sock",
            std::process::id(),
            label
        ))
    }

    fn unused_local_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral listener should bind");
        let addr = listener
            .local_addr()
            .expect("ephemeral listener should report local addr");
        drop(listener);
        addr
    }
}
