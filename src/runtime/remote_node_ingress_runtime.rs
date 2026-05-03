use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    ApplyPtyResize, CloseMirrorRequest, MirrorBootstrapChunk, MirrorBootstrapComplete,
    NodeSessionEnvelope as GrpcNodeSessionEnvelope, OpenMirrorAccepted, OpenMirrorRejected,
    OpenMirrorRequest, RouteContext, TargetExited as GrpcTargetExited, TargetInputDelivery,
    TargetOutput as GrpcTargetOutput, TargetPublished as GrpcTargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, RemoteNodeSessionHandle,
    RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{
    CloseMirrorRequestPayload, ControlPlanePayload, MirrorBootstrapChunkPayload,
    MirrorBootstrapCompletePayload, OpenMirrorAcceptedPayload, OpenMirrorRejectedPayload,
    OpenMirrorRequestPayload, ProtocolEnvelope, TargetExitedPayload, TargetOutputPayload,
    TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, write_control_plane_envelope, write_registration_frame,
};
use crate::runtime::remote_authority_connection_runtime::QueuedAuthorityStreamSink;
use crate::runtime::remote_authority_transport_runtime::spawn_authority_transport_listener;
use crate::runtime::remote_node_session_runtime::{
    spawn_remote_node_session_listener, RemoteNodePublicationSink, RemoteNodeSessionListenerGuard,
};
use std::collections::HashMap;
use std::io;
use std::net::{Shutdown, SocketAddr};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

pub trait RemoteNodeIngressGuard: Send {}

impl<T> RemoteNodeIngressGuard for T where T: Send {}

pub trait RemoteNodeIngressSource {
    type Guard;

    fn start(
        &self,
        socket_path: PathBuf,
        authority_sink: QueuedAuthorityStreamSink,
        publication_sink: Arc<dyn RemoteNodePublicationSink>,
    ) -> io::Result<Self::Guard>;
}

pub trait RemoteNodeIngressStarter: Send + Sync {
    fn start_ingress(
        &self,
        socket_path: PathBuf,
        authority_sink: QueuedAuthorityStreamSink,
        publication_sink: Arc<dyn RemoteNodePublicationSink>,
    ) -> io::Result<Box<dyn RemoteNodeIngressGuard>>;
}

#[derive(Clone, Default)]
pub struct LocalSocketRemoteNodeIngressSource;

#[derive(Clone, Default)]
pub struct AuthoritySocketRemoteNodeIngressSource;

#[derive(Clone)]
pub struct GrpcRemoteNodeIngressSource {
    bind_addr: SocketAddr,
    transport: GrpcRemoteNodeTransport,
}

pub struct RemoteNodeIngressRuntime<S = LocalSocketRemoteNodeIngressSource> {
    source: S,
}

impl<S> RemoteNodeIngressRuntime<S>
where
    S: RemoteNodeIngressSource,
{
    pub fn new(source: S) -> Self {
        Self { source }
    }

    pub fn start_with_source(
        &self,
        socket_path: PathBuf,
        authority_sink: QueuedAuthorityStreamSink,
        publication_sink: Arc<dyn RemoteNodePublicationSink>,
    ) -> io::Result<S::Guard> {
        self.source
            .start(socket_path, authority_sink, publication_sink)
    }
}

impl RemoteNodeIngressRuntime<GrpcRemoteNodeIngressSource> {
    pub fn with_grpc_source(bind_addr: SocketAddr) -> Self {
        Self::new(GrpcRemoteNodeIngressSource::new(bind_addr))
    }
}

impl<S> RemoteNodeIngressStarter for RemoteNodeIngressRuntime<S>
where
    S: RemoteNodeIngressSource + Send + Sync,
    S::Guard: RemoteNodeIngressGuard + 'static,
{
    fn start_ingress(
        &self,
        socket_path: PathBuf,
        authority_sink: QueuedAuthorityStreamSink,
        publication_sink: Arc<dyn RemoteNodePublicationSink>,
    ) -> io::Result<Box<dyn RemoteNodeIngressGuard>> {
        Ok(Box::new(self.start_with_source(
            socket_path,
            authority_sink,
            publication_sink,
        )?))
    }
}

impl RemoteNodeIngressSource for LocalSocketRemoteNodeIngressSource {
    type Guard = RemoteNodeSessionListenerGuard;

    fn start(
        &self,
        socket_path: PathBuf,
        authority_sink: QueuedAuthorityStreamSink,
        publication_sink: Arc<dyn RemoteNodePublicationSink>,
    ) -> io::Result<Self::Guard> {
        spawn_remote_node_session_listener(socket_path, authority_sink, publication_sink)
    }
}

impl RemoteNodeIngressSource for AuthoritySocketRemoteNodeIngressSource {
    type Guard =
        crate::runtime::remote_authority_transport_runtime::AuthorityTransportListenerGuard;

    fn start(
        &self,
        socket_path: PathBuf,
        authority_sink: QueuedAuthorityStreamSink,
        _publication_sink: Arc<dyn RemoteNodePublicationSink>,
    ) -> io::Result<Self::Guard> {
        spawn_authority_transport_listener(socket_path, authority_sink)
    }
}

impl GrpcRemoteNodeIngressSource {
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            transport: GrpcRemoteNodeTransport::new(),
        }
    }
}

pub struct GrpcRemoteNodeIngressGuard {
    transport_guard: Option<GrpcRemoteNodeTransportGuard>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for GrpcRemoteNodeIngressGuard {
    fn drop(&mut self) {
        let _ = self.transport_guard.take();
        let _ = self.worker.take().map(|worker| worker.join());
    }
}

impl RemoteNodeIngressSource for GrpcRemoteNodeIngressSource {
    type Guard = GrpcRemoteNodeIngressGuard;

    fn start(
        &self,
        _socket_path: PathBuf,
        authority_sink: QueuedAuthorityStreamSink,
        publication_sink: Arc<dyn RemoteNodePublicationSink>,
    ) -> io::Result<Self::Guard> {
        let (event_tx, event_rx) = mpsc::channel();
        let transport_guard = self
            .transport
            .listen_inbound(self.bind_addr, event_tx)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
        let worker = thread::spawn(move || {
            run_grpc_node_ingress_worker(event_rx, authority_sink, publication_sink);
        });
        Ok(GrpcRemoteNodeIngressGuard {
            transport_guard: Some(transport_guard),
            worker: Some(worker),
        })
    }
}

fn run_grpc_node_ingress_worker(
    event_rx: mpsc::Receiver<RemoteNodeTransportEvent>,
    authority_sink: QueuedAuthorityStreamSink,
    publication_sink: Arc<dyn RemoteNodePublicationSink>,
) {
    let mut sessions = HashMap::new();
    while let Ok(event) = event_rx.recv() {
        match event {
            RemoteNodeTransportEvent::SessionOpened { session } => {
                if let Ok(bridge) =
                    ActiveGrpcNodeSession::new(session.clone(), authority_sink.clone())
                {
                    sessions.insert(session.node_id().to_string(), bridge);
                }
            }
            RemoteNodeTransportEvent::EnvelopeReceived { node_id, envelope } => {
                let _ = route_grpc_envelope(
                    &node_id,
                    envelope,
                    sessions.get_mut(&node_id),
                    publication_sink.as_ref(),
                );
            }
            RemoteNodeTransportEvent::SessionClosed { node_id } => {
                if let Some(session) = sessions.remove(&node_id) {
                    session.shutdown();
                }
            }
            RemoteNodeTransportEvent::TransportFailed { node_id, .. } => {
                if let Some(node_id) = node_id {
                    if let Some(session) = sessions.remove(&node_id) {
                        session.shutdown();
                    }
                }
            }
        }
    }
    for (_, session) in sessions {
        session.shutdown();
    }
}

struct ActiveGrpcNodeSession {
    inbound_writer: UnixStream,
    outbound_shutdown: UnixStream,
    outbound_forwarder: Option<thread::JoinHandle<()>>,
}

impl ActiveGrpcNodeSession {
    fn new(
        session: RemoteNodeSessionHandle,
        authority_sink: QueuedAuthorityStreamSink,
    ) -> io::Result<Self> {
        let (mut local_stream, pane_stream) = UnixStream::pair()?;
        authority_sink.submit(pane_stream).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "authority stream consumer is unavailable for grpc ingress",
            )
        })?;
        write_registration_frame(&mut local_stream, session.node_id())
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
        let inbound_writer = local_stream.try_clone()?;
        let outbound_shutdown = local_stream.try_clone()?;
        let outbound_forwarder = thread::spawn(move || {
            let _ = forward_local_authority_outbound(local_stream, session);
        });
        Ok(Self {
            inbound_writer,
            outbound_shutdown,
            outbound_forwarder: Some(outbound_forwarder),
        })
    }

    fn write_authority_envelope(
        &mut self,
        envelope: &ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), io::Error> {
        write_control_plane_envelope(&mut self.inbound_writer, envelope)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
    }

    fn shutdown(mut self) {
        let _ = self.inbound_writer.shutdown(Shutdown::Both);
        let _ = self.outbound_shutdown.shutdown(Shutdown::Both);
        if let Some(forwarder) = self.outbound_forwarder.take() {
            let _ = forwarder.join();
        }
    }
}

fn route_grpc_envelope(
    node_id: &str,
    envelope: GrpcNodeSessionEnvelope,
    session: Option<&mut ActiveGrpcNodeSession>,
    publication_sink: &dyn RemoteNodePublicationSink,
) -> Result<(), io::Error> {
    match envelope.body.as_ref() {
        Some(Body::TargetPublished(payload)) => publication_sink
            .publish(map_target_published_envelope(node_id, &envelope, payload)?)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string())),
        Some(Body::TargetExited(payload)) => publication_sink
            .publish(map_target_exited_envelope(node_id, &envelope, payload))
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string())),
        Some(Body::TargetOutput(payload)) => {
            let session = session.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("grpc authority session `{node_id}` is not registered"),
                )
            })?;
            session
                .write_authority_envelope(&map_target_output_envelope(node_id, &envelope, payload)?)
        }
        Some(Body::MirrorBootstrapChunk(payload)) => {
            let session = session.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("grpc authority session `{node_id}` is not registered"),
                )
            })?;
            session.write_authority_envelope(&map_mirror_bootstrap_chunk_envelope(
                node_id, &envelope, payload,
            )?)
        }
        Some(Body::MirrorBootstrapComplete(payload)) => {
            let session = session.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("grpc authority session `{node_id}` is not registered"),
                )
            })?;
            session.write_authority_envelope(&map_mirror_bootstrap_complete_envelope(
                node_id, &envelope, payload,
            )?)
        }
        Some(Body::OpenMirrorRequest(payload)) => {
            let session = session.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("grpc authority session `{node_id}` is not registered"),
                )
            })?;
            session.write_authority_envelope(&ProtocolEnvelope {
                protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                message_id: envelope.message_id.clone(),
                message_type: "open_mirror_request",
                timestamp: timestamp_string(&envelope),
                sender_id: node_id.to_string(),
                correlation_id: envelope.correlation_id.clone(),
                session_id: route_session_id(&envelope)
                    .or_else(|| grpc_payload_session_id(&payload.session_id, &payload.target_id)),
                target_id: route_target_id(&envelope).or_else(|| Some(payload.target_id.clone())),
                attachment_id: route_attachment_id(&envelope),
                console_id: route_console_id(&envelope)
                    .or_else(|| Some(payload.console_id.clone())),
                payload: ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
                    session_id: grpc_payload_session_id(&payload.session_id, &payload.target_id)
                        .unwrap_or_else(|| payload.target_id.clone()),
                    target_id: payload.target_id.clone(),
                    console_id: payload.console_id.clone(),
                    cols: payload.cols as usize,
                    rows: payload.rows as usize,
                }),
            })
        }
        Some(Body::CloseMirrorRequest(payload)) => {
            let session = session.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("grpc authority session `{node_id}` is not registered"),
                )
            })?;
            session.write_authority_envelope(&ProtocolEnvelope {
                protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                message_id: envelope.message_id.clone(),
                message_type: "close_mirror_request",
                timestamp: timestamp_string(&envelope),
                sender_id: node_id.to_string(),
                correlation_id: envelope.correlation_id.clone(),
                session_id: route_session_id(&envelope)
                    .or_else(|| grpc_payload_session_id(&payload.session_id, &payload.target_id)),
                target_id: route_target_id(&envelope).or_else(|| Some(payload.target_id.clone())),
                attachment_id: route_attachment_id(&envelope),
                console_id: route_console_id(&envelope),
                payload: ControlPlanePayload::CloseMirrorRequest(CloseMirrorRequestPayload {
                    session_id: grpc_payload_session_id(&payload.session_id, &payload.target_id)
                        .unwrap_or_else(|| payload.target_id.clone()),
                    target_id: payload.target_id.clone(),
                }),
            })
        }
        Some(Body::OpenMirrorAccepted(payload)) => {
            let session = session.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("grpc authority session `{node_id}` is not registered"),
                )
            })?;
            session.write_authority_envelope(&ProtocolEnvelope {
                protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                message_id: envelope.message_id.clone(),
                message_type: "open_mirror_accepted",
                timestamp: timestamp_string(&envelope),
                sender_id: node_id.to_string(),
                correlation_id: envelope.correlation_id.clone(),
                session_id: route_session_id(&envelope)
                    .or_else(|| grpc_payload_session_id(&payload.session_id, &payload.target_id)),
                target_id: route_target_id(&envelope).or_else(|| Some(payload.target_id.clone())),
                attachment_id: route_attachment_id(&envelope),
                console_id: route_console_id(&envelope),
                payload: ControlPlanePayload::OpenMirrorAccepted(OpenMirrorAcceptedPayload {
                    session_id: grpc_payload_session_id(&payload.session_id, &payload.target_id)
                        .unwrap_or_else(|| payload.target_id.clone()),
                    target_id: payload.target_id.clone(),
                    availability: known_availability(&payload.availability)?,
                }),
            })
        }
        Some(Body::OpenMirrorRejected(payload)) => {
            let session = session.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("grpc authority session `{node_id}` is not registered"),
                )
            })?;
            session.write_authority_envelope(&ProtocolEnvelope {
                protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                message_id: envelope.message_id.clone(),
                message_type: "open_mirror_rejected",
                timestamp: timestamp_string(&envelope),
                sender_id: node_id.to_string(),
                correlation_id: envelope.correlation_id.clone(),
                session_id: route_session_id(&envelope)
                    .or_else(|| grpc_payload_session_id(&payload.session_id, &payload.target_id)),
                target_id: route_target_id(&envelope).or_else(|| Some(payload.target_id.clone())),
                attachment_id: route_attachment_id(&envelope),
                console_id: route_console_id(&envelope),
                payload: ControlPlanePayload::OpenMirrorRejected(OpenMirrorRejectedPayload {
                    session_id: grpc_payload_session_id(&payload.session_id, &payload.target_id)
                        .unwrap_or_else(|| payload.target_id.clone()),
                    target_id: payload.target_id.clone(),
                    code: "mirror_not_available",
                    message: payload.reason.clone(),
                }),
            })
        }
        Some(Body::Heartbeat(_)) | Some(Body::ClientHello(_)) => Ok(()),
        _ => Ok(()),
    }
}

fn map_target_published_envelope(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &GrpcTargetPublished,
) -> Result<ProtocolEnvelope<ControlPlanePayload>, io::Error> {
    ensure_matching_authority_id(node_id, Some(payload.authority_node_id.as_str()))?;
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

fn map_target_output_envelope(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &GrpcTargetOutput,
) -> Result<ProtocolEnvelope<ControlPlanePayload>, io::Error> {
    Ok(ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: envelope.message_id.clone(),
        message_type: "target_output",
        timestamp: timestamp_string(envelope),
        sender_id: node_id.to_string(),
        correlation_id: envelope.correlation_id.clone(),
        session_id: route_session_id(envelope)
            .or_else(|| grpc_payload_session_id(&payload.session_id, &payload.target_id)),
        target_id: route_target_id(envelope).or_else(|| Some(payload.target_id.clone())),
        attachment_id: route_attachment_id(envelope),
        console_id: route_console_id(envelope),
        payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
            session_id: grpc_payload_session_id(&payload.session_id, &payload.target_id)
                .unwrap_or_else(|| payload.target_id.clone()),
            target_id: payload.target_id.clone(),
            output_seq: payload.output_seq,
            stream: known_output_stream(&payload.stream)?,
            bytes_base64: encode_base64(&payload.output_bytes),
        }),
    })
}

fn map_mirror_bootstrap_chunk_envelope(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &MirrorBootstrapChunk,
) -> Result<ProtocolEnvelope<ControlPlanePayload>, io::Error> {
    Ok(ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: envelope.message_id.clone(),
        message_type: "mirror_bootstrap_chunk",
        timestamp: timestamp_string(envelope),
        sender_id: node_id.to_string(),
        correlation_id: envelope.correlation_id.clone(),
        session_id: route_session_id(envelope)
            .or_else(|| grpc_payload_session_id(&payload.session_id, &payload.target_id)),
        target_id: route_target_id(envelope).or_else(|| Some(payload.target_id.clone())),
        attachment_id: route_attachment_id(envelope),
        console_id: route_console_id(envelope),
        payload: ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
            session_id: grpc_payload_session_id(&payload.session_id, &payload.target_id)
                .unwrap_or_else(|| payload.target_id.clone()),
            target_id: payload.target_id.clone(),
            chunk_seq: payload.chunk_seq,
            stream: known_output_stream(&payload.stream)?,
            bytes_base64: encode_base64(&payload.output_bytes),
        }),
    })
}

fn map_mirror_bootstrap_complete_envelope(
    node_id: &str,
    envelope: &GrpcNodeSessionEnvelope,
    payload: &MirrorBootstrapComplete,
) -> Result<ProtocolEnvelope<ControlPlanePayload>, io::Error> {
    Ok(ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: envelope.message_id.clone(),
        message_type: "mirror_bootstrap_complete",
        timestamp: timestamp_string(envelope),
        sender_id: node_id.to_string(),
        correlation_id: envelope.correlation_id.clone(),
        session_id: route_session_id(envelope)
            .or_else(|| grpc_payload_session_id(&payload.session_id, &payload.target_id)),
        target_id: route_target_id(envelope).or_else(|| Some(payload.target_id.clone())),
        attachment_id: route_attachment_id(envelope),
        console_id: route_console_id(envelope),
        payload: ControlPlanePayload::MirrorBootstrapComplete(MirrorBootstrapCompletePayload {
            session_id: grpc_payload_session_id(&payload.session_id, &payload.target_id)
                .unwrap_or_else(|| payload.target_id.clone()),
            target_id: payload.target_id.clone(),
            last_chunk_seq: payload.last_chunk_seq,
        }),
    })
}

fn forward_local_authority_outbound(
    mut local_stream: UnixStream,
    session: RemoteNodeSessionHandle,
) -> Result<(), io::Error> {
    loop {
        let envelope = read_control_plane_envelope(&mut local_stream)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
        if let Some(envelope) = map_outbound_control_plane_envelope(&session, envelope)? {
            session
                .send(envelope)
                .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
        }
    }
}

fn map_outbound_control_plane_envelope(
    session: &RemoteNodeSessionHandle,
    envelope: ProtocolEnvelope<ControlPlanePayload>,
) -> Result<Option<GrpcNodeSessionEnvelope>, io::Error> {
    let route = Some(RouteContext {
        authority_node_id: Some(session.node_id().to_string()),
        target_id: envelope.target_id.clone(),
        attachment_id: envelope.attachment_id.clone(),
        console_id: envelope.console_id.clone(),
        console_host_id: match &envelope.payload {
            ControlPlanePayload::TargetInput(payload) => Some(payload.console_host_id.clone()),
            _ => None,
        },
        session_id: envelope.session_id.clone(),
    });
    let body = match envelope.payload {
        ControlPlanePayload::TargetInput(payload) => {
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
            }))
        }
        ControlPlanePayload::ApplyResize(payload) => Some(Body::ApplyPtyResize(ApplyPtyResize {
            target_id: payload.target_id,
            resize_epoch: payload.resize_epoch,
            resize_authority_console_id: payload.resize_authority_console_id,
            cols: payload.cols as u32,
            rows: payload.rows as u32,
            session_id: payload.session_id,
        })),
        ControlPlanePayload::OpenMirrorRequest(payload) => {
            Some(Body::OpenMirrorRequest(OpenMirrorRequest {
                target_id: payload.target_id,
                session_id: payload.session_id,
                console_id: payload.console_id,
                cols: payload.cols as u32,
                rows: payload.rows as u32,
            }))
        }
        ControlPlanePayload::OpenMirrorAccepted(payload) => {
            Some(Body::OpenMirrorAccepted(OpenMirrorAccepted {
                target_id: payload.target_id,
                session_id: payload.session_id,
                availability: payload.availability.to_string(),
            }))
        }
        ControlPlanePayload::OpenMirrorRejected(payload) => {
            Some(Body::OpenMirrorRejected(OpenMirrorRejected {
                target_id: payload.target_id,
                session_id: payload.session_id,
                reason: payload.message,
                status: None,
            }))
        }
        ControlPlanePayload::CloseMirrorRequest(payload) => {
            Some(Body::CloseMirrorRequest(CloseMirrorRequest {
                target_id: payload.target_id,
                session_id: payload.session_id,
            }))
        }
        _ => None,
    };
    Ok(body.map(|body| GrpcNodeSessionEnvelope {
        message_id: envelope.message_id,
        sent_at: None,
        session_instance_id: session.session_instance_id().to_string(),
        correlation_id: envelope.correlation_id,
        route,
        body: Some(body),
    }))
}

fn ensure_matching_authority_id(
    node_id: &str,
    authority_id: Option<&str>,
) -> Result<(), io::Error> {
    if let Some(authority_id) = authority_id {
        if !authority_id.is_empty() && authority_id != node_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "grpc published authority id `{authority_id}` does not match session node `{node_id}`"
                ),
            ));
        }
    }
    Ok(())
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

fn grpc_payload_session_id(payload_session_id: &str, target_id: &str) -> Option<String> {
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
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
}

#[cfg(test)]
mod tests {
    use super::{
        AuthoritySocketRemoteNodeIngressSource, GrpcRemoteNodeIngressSource,
        RemoteNodeIngressSource, RouteContext,
    };
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
    use crate::infra::remote_grpc_proto::v1::{
        ClientHello, MirrorBootstrapChunk, MirrorBootstrapComplete, NodeSessionEnvelope,
        ProtocolVersion, TargetOutput,
    };
    use crate::infra::remote_protocol::{
        ControlPlanePayload, MirrorBootstrapChunkPayload, MirrorBootstrapCompletePayload,
        OpenMirrorRequestPayload, ProtocolEnvelope, TargetInputPayload, REMOTE_PROTOCOL_VERSION,
    };
    use crate::runtime::remote_authority_connection_runtime::{
        AuthorityConnectionRequest, AuthorityTransportEvent, QueuedAuthorityStreamSource,
        RemoteAuthorityConnectionRuntime,
    };
    use crate::runtime::remote_authority_transport_runtime::{
        RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
    };
    use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneSink;
    use crate::runtime::remote_node_session_runtime::RemoteNodePublicationSink;
    use crate::runtime::remote_transport_runtime::{
        RegistryRemoteControlPlaneSink, RemoteConnectionRegistry,
    };
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;
    use tokio::runtime::Builder;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;

    #[test]
    fn grpc_source_bridges_authority_output_and_publication_into_existing_runtime_boundaries() {
        let bind_addr = unused_local_addr();
        let source = GrpcRemoteNodeIngressSource::new(bind_addr);
        let (authority_source, authority_sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(authority_source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _connection_guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-grpc-node-ingress.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued authority runtime should start");
        let publication_sink = Arc::new(RecordingPublicationSink::default());
        let _guard = source
            .start(
                std::env::temp_dir().join("unused-grpc-node-session.sock"),
                authority_sink,
                publication_sink.clone(),
            )
            .expect("grpc ingress source should start");

        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");
        runtime.block_on(async {
            let mut client = NodeSessionServiceClient::connect(format!("http://{bind_addr}"))
                .await
                .expect("grpc client should connect");
            let (tx, rx_stream) = tokio_mpsc::channel(8);
            tx.send(client_hello_envelope("peer-a"))
                .await
                .expect("client hello should send");
            let response = client
                .open_node_session(Request::new(ReceiverStream::new(rx_stream)))
                .await
                .expect("node session should open");
            let mut inbound = response.into_inner();
            let _server_hello = inbound
                .message()
                .await
                .expect("server hello should decode")
                .expect("server hello should exist");

            assert_eq!(
                rx.recv_timeout(Duration::from_secs(1))
                    .expect("connected event should arrive"),
                AuthorityTransportEvent::Connected
            );
            assert!(registry.has_connection("peer-a"));

            tx.send(target_output_envelope())
                .await
                .expect("target output should send");
            let inbound_event = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("authority envelope event should arrive");
            match inbound_event {
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

            tx.send(mirror_bootstrap_chunk_envelope())
                .await
                .expect("mirror bootstrap chunk should send");
            let inbound_event = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("mirror bootstrap chunk should arrive");
            match inbound_event {
                AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                    ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
                        target_id,
                        chunk_seq,
                        bytes_base64,
                        ..
                    }) => {
                        assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                        assert_eq!(chunk_seq, 1);
                        assert_eq!(bytes_base64, "Ym9vdHN0cmFw");
                    }
                    other => panic!("unexpected authority envelope payload: {other:?}"),
                },
                other => panic!("unexpected authority transport event: {other:?}"),
            }

            tx.send(mirror_bootstrap_complete_envelope())
                .await
                .expect("mirror bootstrap complete should send");
            let inbound_event = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("mirror bootstrap complete should arrive");
            match inbound_event {
                AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                    ControlPlanePayload::MirrorBootstrapComplete(
                        MirrorBootstrapCompletePayload {
                            target_id,
                            last_chunk_seq,
                            ..
                        },
                    ) => {
                        assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                        assert_eq!(last_chunk_seq, 1);
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
                            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
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
                .expect("target input should route through bridged grpc authority session");
            let outbound = inbound
                .message()
                .await
                .expect("outbound authority envelope should decode")
                .expect("outbound authority envelope should exist");
            match outbound.body.expect("body should exist") {
                Body::TargetInputDelivery(payload) => {
                    assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                    assert_eq!(payload.input_seq, 3);
                    assert_eq!(payload.input_bytes, b"b".to_vec());
                }
                other => panic!("unexpected grpc outbound body: {other:?}"),
            }
        });

        publication_sink.assert_empty();
    }

    #[test]
    fn authority_socket_source_accepts_authority_transport_clients() {
        let socket_path = std::env::temp_dir().join(format!(
            "waitagent-authority-source-test-{}-{}.sock",
            std::process::id(),
            unused_local_addr().port()
        ));
        let source = AuthoritySocketRemoteNodeIngressSource;
        let (authority_source, authority_sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(authority_source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _connection_guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-authority-source.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued authority runtime should start");
        let publication_sink = Arc::new(RecordingPublicationSink::default());
        let _guard = source
            .start(socket_path.clone(), authority_sink, publication_sink)
            .expect("authority socket ingress source should start");

        let transport = RemoteAuthorityTransportRuntime::connect(&socket_path, "peer-a")
            .expect("authority transport should connect");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        RegistryRemoteControlPlaneSink::new(registry.clone())
            .send(&[
                crate::infra::remote_protocol::NodeBoundControlPlaneMessage {
                    node_id: "peer-a".to_string(),
                    envelope: ProtocolEnvelope {
                        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                        message_id: "open-mirror-1".to_string(),
                        message_type: "open_mirror_request",
                        timestamp: "1Z".to_string(),
                        sender_id: "server".to_string(),
                        correlation_id: None,
                        session_id: Some("shell-1".to_string()),
                        target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                        attachment_id: None,
                        console_id: Some("console-1".to_string()),
                        payload: ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
                            session_id: "shell-1".to_string(),
                            target_id: "remote-peer:peer-a:shell-1".to_string(),
                            console_id: "console-1".to_string(),
                            cols: 120,
                            rows: 40,
                        }),
                    },
                },
            ])
            .expect("open mirror request should route through authority transport");

        match transport
            .recv_command()
            .expect("authority command should arrive")
        {
            RemoteAuthorityCommand::OpenMirror(payload) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.console_id, "console-1");
                assert_eq!(payload.cols, 120);
                assert_eq!(payload.rows, 40);
            }
            other => panic!("unexpected authority command: {other:?}"),
        }

        std::fs::remove_file(socket_path).ok();
    }

    #[derive(Default)]
    struct RecordingPublicationSink {
        envelopes: Mutex<Vec<ProtocolEnvelope<ControlPlanePayload>>>,
    }

    impl RecordingPublicationSink {
        fn assert_empty(&self) {
            assert!(self
                .envelopes
                .lock()
                .expect("publication sink mutex should not be poisoned")
                .is_empty());
        }
    }

    impl RemoteNodePublicationSink for RecordingPublicationSink {
        fn publish(
            &self,
            envelope: ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<(), crate::runtime::remote_node_session_runtime::RemoteNodeSessionError>
        {
            self.envelopes
                .lock()
                .expect("publication sink mutex should not be poisoned")
                .push(envelope);
            Ok(())
        }
    }

    fn client_hello_envelope(node_id: &str) -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: "client-hello-1".to_string(),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: None,
            body: Some(Body::ClientHello(ClientHello {
                node_id: node_id.to_string(),
                node_instance_id: "instance-a".to_string(),
                min_supported_version: Some(ProtocolVersion { major: 1, minor: 0 }),
                max_supported_version: Some(ProtocolVersion { major: 1, minor: 0 }),
                capabilities: None,
                resume: None,
            })),
        }
    }

    fn target_output_envelope() -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: "target-output-1".to_string(),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some("shell-1".to_string()),
            }),
            body: Some(Body::TargetOutput(TargetOutput {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq: 7,
                stream: "pty".to_string(),
                session_id: "shell-1".to_string(),
                output_bytes: b"a".to_vec(),
            })),
        }
    }

    fn mirror_bootstrap_chunk_envelope() -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: "mirror-bootstrap-chunk-1".to_string(),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some("shell-1".to_string()),
            }),
            body: Some(Body::MirrorBootstrapChunk(MirrorBootstrapChunk {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                session_id: "shell-1".to_string(),
                chunk_seq: 1,
                stream: "pty".to_string(),
                output_bytes: b"bootstrap".to_vec(),
            })),
        }
    }

    fn mirror_bootstrap_complete_envelope() -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: "mirror-bootstrap-complete-1".to_string(),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some("shell-1".to_string()),
            }),
            body: Some(Body::MirrorBootstrapComplete(MirrorBootstrapComplete {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                session_id: "shell-1".to_string(),
                last_chunk_seq: 1,
            })),
        }
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
