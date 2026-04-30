use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
use crate::infra::remote_grpc_proto::v1::node_session_service_server::{
    NodeSessionService, NodeSessionServiceServer,
};
use crate::infra::remote_grpc_proto::v1::{
    ClientHello, NodeSessionEnvelope, ProtocolVersion, RecoveryPolicy, ServerHello,
};
use std::fmt;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::Builder;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio_stream::wrappers::{TcpListenerStream, UnboundedReceiverStream};
use tokio_stream::StreamExt;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status};

const SERVER_ID: &str = "waitagent-remote-ingress";
const HEARTBEAT_INTERVAL_SECONDS: i64 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundNodeSessionRequest {
    pub node_id: String,
    pub endpoint_uri: String,
}

#[derive(Debug, Clone)]
pub struct RemoteNodeSessionHandle {
    node_id: String,
    session_instance_id: String,
    outbound_tx: tokio_mpsc::UnboundedSender<NodeSessionEnvelope>,
}

#[derive(Debug, Clone)]
pub enum RemoteNodeTransportEvent {
    SessionOpened {
        session: RemoteNodeSessionHandle,
    },
    SessionClosed {
        node_id: String,
    },
    EnvelopeReceived {
        node_id: String,
        envelope: NodeSessionEnvelope,
    },
    TransportFailed {
        node_id: Option<String>,
        message: String,
    },
}

pub trait RemoteNodeTransport: Send + Sync {
    fn connect_outbound(
        &self,
        request: OutboundNodeSessionRequest,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<GrpcRemoteNodeTransportGuard, RemoteNodeTransportError>;

    fn listen_inbound(
        &self,
        bind_addr: SocketAddr,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<GrpcRemoteNodeTransportGuard, RemoteNodeTransportError>;
}

#[derive(Debug, Clone, Default)]
pub struct GrpcRemoteNodeTransport;

pub struct GrpcRemoteNodeTransportGuard {
    shutdown_tx: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
    local_addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNodeTransportError {
    message: String,
}

impl GrpcRemoteNodeTransport {
    pub fn new() -> Self {
        Self
    }

    pub fn endpoint(&self, endpoint_uri: &str) -> Result<Endpoint, RemoteNodeTransportError> {
        Endpoint::from_shared(endpoint_uri.to_string())
            .map_err(|error| RemoteNodeTransportError::new(error.to_string()))
    }

    pub fn client(&self, channel: Channel) -> NodeSessionServiceClient<Channel> {
        NodeSessionServiceClient::new(channel)
    }
}

impl RemoteNodeSessionHandle {
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn session_instance_id(&self) -> &str {
        &self.session_instance_id
    }

    pub fn send(&self, envelope: NodeSessionEnvelope) -> Result<(), RemoteNodeTransportError> {
        self.outbound_tx
            .send(envelope)
            .map_err(|_| RemoteNodeTransportError::new("remote node session is no longer open"))
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(
        node_id: impl Into<String>,
        session_instance_id: impl Into<String>,
    ) -> (Self, tokio_mpsc::UnboundedReceiver<NodeSessionEnvelope>) {
        let (outbound_tx, outbound_rx) = tokio_mpsc::unbounded_channel();
        (
            Self {
                node_id: node_id.into(),
                session_instance_id: session_instance_id.into(),
                outbound_tx,
            },
            outbound_rx,
        )
    }
}

impl GrpcRemoteNodeTransportGuard {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for GrpcRemoteNodeTransportGuard {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl RemoteNodeTransport for GrpcRemoteNodeTransport {
    fn connect_outbound(
        &self,
        request: OutboundNodeSessionRequest,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<GrpcRemoteNodeTransportGuard, RemoteNodeTransportError> {
        let endpoint = self.endpoint(&request.endpoint_uri)?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let runtime = Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("grpc outbound node transport runtime should build");
            runtime.block_on(async move {
                let session_instance_id = format!("client-session-{}", now_millis());
                let (outbound_tx, outbound_rx) = tokio_mpsc::unbounded_channel();
                let outbound_session = RemoteNodeSessionHandle {
                    node_id: request.node_id.clone(),
                    session_instance_id: session_instance_id.clone(),
                    outbound_tx,
                };
                if let Err(error) = outbound_session.send(client_hello_envelope(
                    &request.node_id,
                    &session_instance_id,
                )) {
                    let _ = started_tx.send(Err(error));
                    return;
                }

                let channel = match endpoint.connect().await {
                    Ok(channel) => channel,
                    Err(error) => {
                        let transport_error =
                            RemoteNodeTransportError::new(error.to_string());
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(request.node_id.clone()),
                            message: transport_error.to_string(),
                        });
                        let _ = started_tx.send(Err(transport_error));
                        return;
                    }
                };
                let mut client = NodeSessionServiceClient::new(channel);
                let response = client
                    .open_node_session(Request::new(UnboundedReceiverStream::new(outbound_rx)))
                    .await;
                let mut inbound = match response {
                    Ok(response) => response.into_inner(),
                    Err(error) => {
                        let transport_error =
                            RemoteNodeTransportError::new(error.to_string());
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(request.node_id.clone()),
                            message: transport_error.to_string(),
                        });
                        let _ = started_tx.send(Err(transport_error));
                        return;
                    }
                };
                let first_envelope = match inbound.message().await {
                    Ok(Some(envelope)) => envelope,
                    Ok(None) => {
                        let transport_error = RemoteNodeTransportError::new(
                            "grpc node session closed before server hello arrived",
                        );
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(request.node_id.clone()),
                            message: transport_error.to_string(),
                        });
                        let _ = started_tx.send(Err(transport_error));
                        return;
                    }
                    Err(error) => {
                        let transport_error =
                            RemoteNodeTransportError::new(error.to_string());
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(request.node_id.clone()),
                            message: transport_error.to_string(),
                        });
                        let _ = started_tx.send(Err(transport_error));
                        return;
                    }
                };
                let Some(Body::ServerHello(server_hello)) = first_envelope.body.as_ref() else {
                    let transport_error = RemoteNodeTransportError::new(
                        "grpc node session did not start with server_hello",
                    );
                    let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                        node_id: Some(request.node_id.clone()),
                        message: transport_error.to_string(),
                    });
                    let _ = started_tx.send(Err(transport_error));
                    return;
                };
                let session = RemoteNodeSessionHandle {
                    node_id: request.node_id.clone(),
                    session_instance_id: server_hello.session_instance_id.clone(),
                    outbound_tx: outbound_session.outbound_tx.clone(),
                };
                let _ = event_tx.send(RemoteNodeTransportEvent::SessionOpened {
                    session: session.clone(),
                });
                let _ = started_tx.send(Ok(()));

                let _ = event_tx.send(RemoteNodeTransportEvent::EnvelopeReceived {
                    node_id: request.node_id.clone(),
                    envelope: first_envelope,
                });

                tokio::pin!(shutdown_rx);
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            break;
                        }
                        result = inbound.message() => {
                            match result {
                                Ok(Some(envelope)) => {
                                    if event_tx.send(RemoteNodeTransportEvent::EnvelopeReceived {
                                        node_id: request.node_id.clone(),
                                        envelope,
                                    }).is_err() {
                                        break;
                                    }
                                }
                                Ok(None) => break,
                                Err(error) => {
                                    let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                        node_id: Some(request.node_id.clone()),
                                        message: error.to_string(),
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
                let _ = event_tx.send(RemoteNodeTransportEvent::SessionClosed {
                    node_id: request.node_id,
                });
            });
        });
        match started_rx.recv() {
            Ok(Ok(())) => Ok(GrpcRemoteNodeTransportGuard {
                shutdown_tx: Some(shutdown_tx),
                worker: Some(worker),
                local_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            }),
            Ok(Err(error)) => {
                let _ = shutdown_tx.send(());
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = shutdown_tx.send(());
                let _ = worker.join();
                Err(RemoteNodeTransportError::new(
                    "grpc outbound node-session worker failed before startup completed",
                ))
            }
        }
    }

    fn listen_inbound(
        &self,
        bind_addr: SocketAddr,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<GrpcRemoteNodeTransportGuard, RemoteNodeTransportError> {
        let listener = std::net::TcpListener::bind(bind_addr)
            .map_err(|error| RemoteNodeTransportError::new(error.to_string()))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| RemoteNodeTransportError::new(error.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|error| RemoteNodeTransportError::new(error.to_string()))?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let worker = thread::spawn(move || {
            let runtime = Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("grpc remote node transport runtime should build");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("std listener should convert into tokio listener");
                let incoming = TcpListenerStream::new(listener);
                let failure_tx = event_tx.clone();
                let service = TransportNodeSessionService { event_tx };
                let server = Server::builder()
                    .add_service(NodeSessionServiceServer::new(service))
                    .serve_with_incoming_shutdown(incoming, async move {
                        let _ = shutdown_rx.await;
                    });
                if let Err(error) = server.await {
                    let _ = failure_tx.send(RemoteNodeTransportEvent::TransportFailed {
                        node_id: None,
                        message: error.to_string(),
                    });
                }
            });
        });
        Ok(GrpcRemoteNodeTransportGuard {
            shutdown_tx: Some(shutdown_tx),
            worker: Some(worker),
            local_addr,
        })
    }
}

impl RemoteNodeTransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteNodeTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteNodeTransportError {}

struct TransportNodeSessionService {
    event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
}

type NodeSessionResponseStream =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<NodeSessionEnvelope, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl NodeSessionService for TransportNodeSessionService {
    type OpenNodeSessionStream = NodeSessionResponseStream;

    async fn open_node_session(
        &self,
        request: Request<tonic::Streaming<NodeSessionEnvelope>>,
    ) -> Result<Response<Self::OpenNodeSessionStream>, Status> {
        let mut inbound = request.into_inner();
        let Some(first_envelope) = inbound.message().await? else {
            return Err(Status::invalid_argument(
                "node session stream must start with client_hello",
            ));
        };
        let Some(Body::ClientHello(client_hello)) = first_envelope.body.as_ref() else {
            return Err(Status::invalid_argument(
                "node session stream must start with client_hello",
            ));
        };
        let node_id = client_hello.node_id.clone();
        if node_id.is_empty() {
            return Err(Status::invalid_argument(
                "client_hello.node_id must not be empty",
            ));
        }

        let session_instance_id = format!("server-session-{}", now_millis());
        let (outbound_tx, outbound_rx) = tokio_mpsc::unbounded_channel();
        let session = RemoteNodeSessionHandle {
            node_id: node_id.clone(),
            session_instance_id: session_instance_id.clone(),
            outbound_tx,
        };
        self.event_tx
            .send(RemoteNodeTransportEvent::SessionOpened {
                session: session.clone(),
            })
            .map_err(|_| Status::unavailable("remote node ingress worker is unavailable"))?;
        session
            .send(server_hello_envelope(&first_envelope, &session_instance_id))
            .map_err(|error| Status::unavailable(error.to_string()))?;

        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(envelope)) => {
                        if event_tx
                            .send(RemoteNodeTransportEvent::EnvelopeReceived {
                                node_id: node_id.clone(),
                                envelope,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(node_id.clone()),
                            message: error.to_string(),
                        });
                        break;
                    }
                }
            }
            let _ = event_tx.send(RemoteNodeTransportEvent::SessionClosed { node_id });
        });

        Ok(Response::new(Box::pin(
            UnboundedReceiverStream::new(outbound_rx).map(Ok),
        )))
    }
}

fn server_hello_envelope(
    client_hello: &NodeSessionEnvelope,
    session_instance_id: &str,
) -> NodeSessionEnvelope {
    NodeSessionEnvelope {
        message_id: format!("server-hello-{}", now_millis()),
        sent_at: Some(timestamp_now()),
        session_instance_id: session_instance_id.to_string(),
        correlation_id: Some(client_hello.message_id.clone()),
        route: None,
        body: Some(Body::ServerHello(ServerHello {
            server_id: SERVER_ID.to_string(),
            session_instance_id: session_instance_id.to_string(),
            negotiated_version: Some(ProtocolVersion { major: 1, minor: 0 }),
            heartbeat_interval: Some(prost_types::Duration {
                seconds: HEARTBEAT_INTERVAL_SECONDS,
                nanos: 0,
            }),
            recovery_policy: Some(RecoveryPolicy {
                authority_republish_required: true,
                observer_reopen_required: true,
                replay_supported: true,
            }),
        })),
    }
}

fn client_hello_envelope(node_id: &str, session_instance_id: &str) -> NodeSessionEnvelope {
    NodeSessionEnvelope {
        message_id: format!("client-hello-{}", now_millis()),
        sent_at: Some(timestamp_now()),
        session_instance_id: session_instance_id.to_string(),
        correlation_id: None,
        route: None,
        body: Some(Body::ClientHello(ClientHello {
            node_id: node_id.to_string(),
            node_instance_id: session_instance_id.to_string(),
            min_supported_version: Some(ProtocolVersion { major: 1, minor: 0 }),
            max_supported_version: Some(ProtocolVersion { major: 1, minor: 0 }),
            capabilities: None,
            resume: None,
        })),
    }
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
        Body, GrpcRemoteNodeTransport, NodeSessionEnvelope, ProtocolVersion, RemoteNodeTransport,
        RemoteNodeTransportEvent,
    };
    use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
    use crate::infra::remote_grpc_proto::v1::{ClientHello, Heartbeat};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::mpsc;
    use std::time::Duration;
    use tokio::runtime::Builder;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;

    #[test]
    fn inbound_listener_reports_session_events_and_forwards_outbound_envelopes() {
        let bind_addr = unused_local_addr();
        let transport = GrpcRemoteNodeTransport::new();
        let (event_tx, event_rx) = mpsc::channel();
        let _guard = transport
            .listen_inbound(bind_addr, event_tx)
            .expect("grpc listener should start");

        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");
        runtime.block_on(async {
            let mut client = NodeSessionServiceClient::connect(format!("http://{bind_addr}"))
                .await
                .expect("grpc client should connect");
            let (tx, rx) = tokio_mpsc::channel(8);
            tx.send(client_hello_envelope("peer-a"))
                .await
                .expect("client hello should send");
            let response = client
                .open_node_session(Request::new(ReceiverStream::new(rx)))
                .await
                .expect("node session should open");
            let mut inbound = response.into_inner();

            let opened = event_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("session opened event should arrive");
            let session = match opened {
                RemoteNodeTransportEvent::SessionOpened { session } => session,
                other => panic!("unexpected transport event: {other:?}"),
            };
            assert_eq!(session.node_id(), "peer-a");
            assert!(!session.session_instance_id().is_empty());

            let server_hello = inbound
                .message()
                .await
                .expect("server hello should decode")
                .expect("server hello should be present");
            assert!(matches!(server_hello.body, Some(Body::ServerHello(_))));

            tx.send(NodeSessionEnvelope {
                message_id: "heartbeat-1".to_string(),
                sent_at: None,
                session_instance_id: "client-session-1".to_string(),
                correlation_id: None,
                route: None,
                body: Some(Body::Heartbeat(Heartbeat {
                    runtime_id: "peer-a".to_string(),
                })),
            })
            .await
            .expect("heartbeat should send");

            let received = event_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("envelope received event should arrive");
            match received {
                RemoteNodeTransportEvent::EnvelopeReceived { node_id, envelope } => {
                    assert_eq!(node_id, "peer-a");
                    assert!(matches!(envelope.body, Some(Body::Heartbeat(_))));
                }
                other => panic!("unexpected transport event: {other:?}"),
            }

            session
                .send(NodeSessionEnvelope {
                    message_id: "server-heartbeat-1".to_string(),
                    sent_at: None,
                    session_instance_id: session.session_instance_id().to_string(),
                    correlation_id: None,
                    route: None,
                    body: Some(Body::Heartbeat(Heartbeat {
                        runtime_id: "server".to_string(),
                    })),
                })
                .expect("outbound envelope should queue");
            let outbound = inbound
                .message()
                .await
                .expect("outbound envelope should decode")
                .expect("outbound envelope should be present");
            assert!(matches!(outbound.body, Some(Body::Heartbeat(_))));
        });
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

    fn unused_local_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral listener should bind");
        let addr = listener
            .local_addr()
            .expect("ephemeral listener should report local addr");
        drop(listener);
        addr
    }
}
