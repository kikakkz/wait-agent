use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    ApplyPtyResize, CloseMirrorRequest, MirrorBootstrapChunk, MirrorBootstrapComplete,
    NodeSessionEnvelope as GrpcNodeSessionEnvelope, OpenMirrorAccepted, OpenMirrorRejected,
    OpenMirrorRequest, RawPtyInput, RawPtyOutput as GrpcRawPtyOutput, RouteContext,
    TargetExited as GrpcTargetExited, TargetOutput as GrpcTargetOutput,
    TargetPublished as GrpcTargetPublished,
};
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, RemoteNodeSessionHandle,
    RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{
    BootstrapMode, CloseMirrorRequestPayload, ControlPlanePayload, MirrorBootstrapChunkPayload,
    MirrorBootstrapCompletePayload, OpenMirrorAcceptedPayload, OpenMirrorRejectedPayload,
    OpenMirrorRequestPayload, ProtocolEnvelope, RawPtyOutputPayload, TargetExitedPayload,
    TargetOutputPayload, TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_authority_transport_frame, write_authority_transport_frame, write_control_plane_envelope,
    write_registration_frame, AuthorityTransportFrame,
};
use crate::runtime::remote_authority_connection_runtime::QueuedAuthorityStreamSink;
use crate::runtime::remote_authority_transport_runtime::spawn_authority_transport_listener;
use crate::runtime::remote_node_session_runtime::{
    spawn_remote_node_session_listener, RemoteNodePublicationSink, RemoteNodeSessionListenerGuard,
};
use std::collections::HashMap;
use std::io::{self, Write};
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
    #[cfg(test)]
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
    #[cfg(test)]
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

pub(crate) fn run_grpc_node_ingress_worker(
    event_rx: mpsc::Receiver<RemoteNodeTransportEvent>,
    authority_sink: QueuedAuthorityStreamSink,
    publication_sink: Arc<dyn RemoteNodePublicationSink>,
) {
    let t_worker = std::time::Instant::now();
    let mut sessions = HashMap::new();
    while let Ok(event) = event_rx.recv() {
        match event {
            RemoteNodeTransportEvent::SessionOpened { session } => {
                let t_session = std::time::Instant::now();
                if let Ok(bridge) =
                    ActiveGrpcNodeSession::new(session.clone(), authority_sink.clone())
                {
                    ERROR_LOG.log(format!(
                        "[diag-timing] ingress worker: SessionOpened -> ActiveGrpcNodeSession created for node {} (worker_elapsed={:?}, session_new={:?})",
                        session.node_id(),
                        t_worker.elapsed(),
                        t_session.elapsed()
                    ));
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
            RemoteNodeTransportEvent::SessionClosed { node_id, .. } => {
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

pub(crate) struct ActiveGrpcNodeSession {
    inbound_writer: UnixStream,
    outbound_shutdown: UnixStream,
    outbound_forwarder: Option<thread::JoinHandle<()>>,
}

impl ActiveGrpcNodeSession {
    pub(crate) fn new(
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

    pub(crate) fn write_authority_envelope(
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

pub(crate) fn route_grpc_envelope(
    node_id: &str,
    envelope: GrpcNodeSessionEnvelope,
    session: Option<&mut ActiveGrpcNodeSession>,
    publication_sink: &dyn RemoteNodePublicationSink,
) -> Result<(), io::Error> {
    match envelope.body.as_ref() {
        Some(Body::TargetPublished(payload)) => publication_sink
            .publish(map_target_published_envelope(node_id, &envelope, payload)?)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string())),
        Some(Body::TargetExited(payload)) => {
            // Also forward to the authority session so the pane runtime can
            // perform a clean shutdown instead of entering reconnection.
            if let Some(session) = session {
                let _ = session.write_authority_envelope(&ProtocolEnvelope {
                    protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                    message_id: envelope.message_id.clone(),
                    message_type: "target_exited",
                    timestamp: timestamp_string(&envelope),
                    sender_id: node_id.to_string(),
                    correlation_id: envelope.correlation_id.clone(),
                    session_id: None,
                    target_id: Some(payload.target_id.clone()),
                    attachment_id: None,
                    console_id: None,
                    payload: ControlPlanePayload::TargetExited(TargetExitedPayload {
                        transport_session_id: payload.transport_session_id.clone(),
                        source_session_name: None,
                    }),
                });
            }
            publication_sink
                .publish(map_target_exited_envelope(node_id, &envelope, payload))
                .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
        }
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
        Some(Body::RawPtyOutput(payload)) => {
            let session = session.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("grpc authority session `{node_id}` is not registered"),
                )
            })?;
            session.write_authority_envelope(&ProtocolEnvelope {
                protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                message_id: envelope.message_id.clone(),
                message_type: "raw_pty_output",
                timestamp: timestamp_string(&envelope),
                sender_id: node_id.to_string(),
                correlation_id: envelope.correlation_id.clone(),
                session_id: route_session_id(&envelope)
                    .or_else(|| grpc_payload_session_id(&payload.session_id, &payload.target_id)),
                target_id: route_target_id(&envelope).or_else(|| Some(payload.target_id.clone())),
                attachment_id: route_attachment_id(&envelope),
                console_id: route_console_id(&envelope),
                payload: ControlPlanePayload::RawPtyOutput(RawPtyOutputPayload {
                    session_id: grpc_payload_session_id(&payload.session_id, &payload.target_id)
                        .unwrap_or_else(|| payload.target_id.clone()),
                    target_id: payload.target_id.clone(),
                    output_seq: payload.output_seq,
                    output_bytes: payload.output_bytes.clone(),
                }),
            })
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
                    raw_pty_passthrough: payload.raw_pty_passthrough,
                    bootstrap_mode: if payload.bootstrap_mode_visible_only {
                        BootstrapMode::VisibleOnly
                    } else {
                        BootstrapMode::Full
                    },
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
            output_bytes: payload.output_bytes.clone(),
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
            output_bytes: payload.output_bytes.clone(),
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
            alternate_screen_active: payload.alternate_screen_active,
            application_cursor_keys: payload.application_cursor_keys,
            cursor_visible: payload.cursor_visible,
        }),
    })
}

fn forward_local_authority_outbound(
    mut local_stream: UnixStream,
    session: RemoteNodeSessionHandle,
) -> Result<(), io::Error> {
    loop {
        match read_authority_transport_frame(&mut local_stream)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?
        {
            AuthorityTransportFrame::Ping => {
                let mut buf = Vec::new();
                let _ = write_authority_transport_frame(&mut buf, &AuthorityTransportFrame::Pong);
                let _ = local_stream.write_all(&buf);
            }
            AuthorityTransportFrame::Pong => {
                // Consume silently.
            }
            AuthorityTransportFrame::ControlPlane(envelope) => {
                if let Some(grpc_envelope) =
                    map_outbound_control_plane_envelope(&session, envelope)?
                {
                    session.send(grpc_envelope).map_err(|error| {
                        io::Error::new(io::ErrorKind::BrokenPipe, error.to_string())
                    })?;
                }
            }
            AuthorityTransportFrame::RawPtyInput(payload) => {
                // Raw input from the stdin fast path — convert to a gRPC
                // RawPtyInput envelope and forward to the authority.
                ERROR_LOG.log(format!(
                    "[diag-timing] forwarder: forwarding RawPtyInput ({} bytes)",
                    payload.input_bytes.len()
                ));
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let envelope = ProtocolEnvelope {
                    protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                    message_id: format!("raw-input-{ts}"),
                    message_type: "raw_pty_input",
                    timestamp: format!("{ts}"),
                    sender_id: session.node_id().to_string(),
                    correlation_id: None,
                    session_id: Some(payload.session_id.clone()),
                    target_id: Some(payload.target_id.clone()),
                    attachment_id: Some(payload.attachment_id.clone()),
                    console_id: Some(payload.console_id.clone()),
                    payload: ControlPlanePayload::RawPtyInput(payload),
                };
                if let Some(grpc_envelope) =
                    map_outbound_control_plane_envelope(&session, envelope)?
                {
                    session.send(grpc_envelope).map_err(|error| {
                        io::Error::new(io::ErrorKind::BrokenPipe, error.to_string())
                    })?;
                }
            }
            _ => {
                // RawPtyOutput, SyncRequest, SyncResponse are
                // not expected on the outbound path — consume silently.
            }
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
            ControlPlanePayload::RawPtyInput(payload) => Some(payload.console_host_id.clone()),
            _ => None,
        },
        session_id: envelope.session_id.clone(),
    });
    let body = match envelope.payload {
        ControlPlanePayload::RawPtyInput(payload) => Some(Body::RawPtyInput(RawPtyInput {
            attachment_id: payload.attachment_id,
            target_id: payload.target_id,
            console_id: payload.console_id,
            console_host_id: payload.console_host_id,
            input_seq: payload.input_seq,
            session_id: payload.session_id,
            input_bytes: payload.input_bytes,
        })),
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
                raw_pty_passthrough: payload.raw_pty_passthrough,
                bootstrap_mode_visible_only: matches!(
                    payload.bootstrap_mode,
                    BootstrapMode::VisibleOnly
                ),
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
mod remote_node_ingress_runtime_test;
