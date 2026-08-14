use crate::infra::operator_auth::{self, OperatorKeyStore};
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
use crate::infra::remote_grpc_proto::v1::node_session_service_server::{
    NodeSessionService, NodeSessionServiceServer,
};
use crate::infra::remote_grpc_proto::v1::{
    ClientHello, Heartbeat, NodeSessionEnvelope, ProtocolVersion, RecoveryPolicy, ServerHello,
};
use sha2::{Digest, Sha256};
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::infra::error_log::ERROR_LOG;
use tokio::runtime::Builder;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio_stream::wrappers::{TcpListenerStream, UnboundedReceiverStream};
use tokio_stream::StreamExt;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status};
use tower::Service;

const SERVER_ID: &str = "waitagent-remote-ingress";
const HEARTBEAT_INTERVAL_SECONDS: i64 = 15;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const OPERATOR_AUTH_CHALLENGE_SIZE: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundNodeSessionRequest {
    pub node_id: String,
    pub endpoint_uri: String,
    pub tls_pin_sha256: Option<String>,
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
        session_instance_id: String,
    },
    EnvelopeReceived {
        node_id: String,
        session_instance_id: String,
        envelope: Box<NodeSessionEnvelope>,
    },
    TransportFailed {
        node_id: Option<String>,
        session_instance_id: Option<String>,
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
pub struct GrpcRemoteNodeTransport {
    /// Optional TLS certificate path for inbound listeners.
    tls_cert_path: Option<PathBuf>,
    /// Optional TLS private key path for inbound listeners.
    tls_key_path: Option<PathBuf>,
}

pub struct GrpcRemoteNodeTransportGuard {
    shutdown_tx: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
    #[allow(dead_code)]
    local_addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNodeTransportError {
    message: String,
}

impl GrpcRemoteNodeTransport {
    pub fn new() -> Self {
        Self {
            tls_cert_path: None,
            tls_key_path: None,
        }
    }

    /// Configure the transport to serve inbound connections over TLS using the
    /// given certificate and private key.
    pub fn with_tls(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Self {
        Self {
            tls_cert_path: Some(cert_path.into()),
            tls_key_path: Some(key_path.into()),
        }
    }

    pub fn endpoint(&self, endpoint_uri: &str) -> Result<Endpoint, RemoteNodeTransportError> {
        Ok(Endpoint::from_shared(endpoint_uri.to_string())
            .map_err(|error| RemoteNodeTransportError::new(error.to_string()))?
            .tcp_nodelay(true)
            .tcp_keepalive(Some(TCP_KEEPALIVE_IDLE))
            .connect_timeout(CONNECT_TIMEOUT)
            .http2_keep_alive_interval(HTTP2_KEEPALIVE_INTERVAL)
            .keep_alive_timeout(HTTP2_KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true))
    }

    #[allow(dead_code)]
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
}

impl GrpcRemoteNodeTransportGuard {
    #[allow(dead_code)]
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
        let endpoint = self.endpoint(&tls_endpoint_uri(
            &request.endpoint_uri,
            &request.tls_pin_sha256,
        ))?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let t_start = Instant::now();

        let worker = thread::Builder::new()
            .spawn(move || {
                let runtime = match Builder::new_multi_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = started_tx.send(Err(RemoteNodeTransportError::new(format!(
                            "failed to build grpc outbound node transport runtime: {error}"
                        ))));
                        return;
                    }
                };

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

                let tcp_start = Instant::now();

                let channel = match connect_channel(&endpoint, &request.tls_pin_sha256).await {
                    Ok(channel) => {
                        let _t_tcp = tcp_start.elapsed();
                        channel
                    }
                    Err(error) => {
                        let _t_fail = tcp_start.elapsed();
                        let transport_error =
                            RemoteNodeTransportError::new(error.to_string());
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(request.node_id.clone()),
                            session_instance_id: None,
                            message: transport_error.to_string(),
                        });
                        let _ = started_tx.send(Err(transport_error));
                        return;
                    }
                };
                let mut client = NodeSessionServiceClient::new(channel);
                let grpc_start = Instant::now();
                let response = client
                    .open_node_session(Request::new(UnboundedReceiverStream::new(outbound_rx)))
                    .await;
                let mut inbound = match response {
                    Ok(response) => {
                        let _t_grpc = grpc_start.elapsed();
                        response.into_inner()
                    }
                    Err(error) => {
                        let _t_fail = grpc_start.elapsed();
                        let transport_error =
                            RemoteNodeTransportError::new(error.to_string());
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(request.node_id.clone()),
                            session_instance_id: None,
                            message: transport_error.to_string(),
                        });
                        let _ = started_tx.send(Err(transport_error));
                        return;
                    }
                };
                let server_hello_start = Instant::now();
                let first_envelope = match inbound.message().await {
                    Ok(Some(envelope)) => {
                        let _t_hello = server_hello_start.elapsed();
                        envelope
                    }
                    Ok(None) => {
                        let transport_error = RemoteNodeTransportError::new(
                            "grpc node session closed before server hello arrived",
                        );
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(request.node_id.clone()),
                            session_instance_id: None,
                            message: transport_error.to_string(),
                        });
                        let _ = started_tx.send(Err(transport_error));
                        return;
                    }
                    Err(error) => {
                        let t_fail = server_hello_start.elapsed();
                        ERROR_LOG.log_error(format!(
                            "connect_outbound ServerHello error after {:?}: {}",
                            t_fail, error
                        ));
                        let transport_error =
                            RemoteNodeTransportError::new(error.to_string());
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: Some(request.node_id.clone()),
                            session_instance_id: None,
                            message: transport_error.to_string(),
                        });
                        let _ = started_tx.send(Err(transport_error));
                        return;
                    }
                };
                let Some(Body::ServerHello(server_hello)) = first_envelope.body.as_ref() else {
                    let _t_fail = server_hello_start.elapsed();
                    let transport_error = RemoteNodeTransportError::new(
                        "grpc node session did not start with server_hello",
                    );
                    let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                        node_id: Some(request.node_id.clone()),
                        session_instance_id: None,
                        message: transport_error.to_string(),
                    });
                    let _ = started_tx.send(Err(transport_error));
                    return;
                };

                if !server_hello.operator_challenge.is_empty() {
                    let keystore = operator_auth::KeyringOperatorKeyStore;
                    let challenge = server_hello.operator_challenge.clone();
                    match keystore.sign_challenge(&challenge) {
                        Ok((auth_scheme, challenge_response)) => {
                            if let Err(error) = outbound_session.send(auth_response_envelope(
                                &request.node_id,
                                &session_instance_id,
                                &auth_scheme,
                                &challenge_response,
                            )) {
                                let _ = started_tx.send(Err(error));
                                return;
                            }
                        }
                        Err(error) => {
                            let transport_error = RemoteNodeTransportError::new(error.to_string());
                            let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                node_id: Some(request.node_id.clone()),
                                session_instance_id: None,
                                message: transport_error.to_string(),
                            });
                            let _ = started_tx.send(Err(transport_error));
                            return;
                        }
                    }
                }

                let session = RemoteNodeSessionHandle {
                    node_id: request.node_id.clone(),
                    session_instance_id: server_hello.session_instance_id.clone(),
                    outbound_tx: outbound_session.outbound_tx.clone(),
                };
                let session_instance_id = session.session_instance_id().to_string();
                let _t_done = t_start.elapsed();

                let _ = event_tx.send(RemoteNodeTransportEvent::SessionOpened {
                    session: session.clone(),
                });
                let _ = started_tx.send(Ok(()));

                let _ = event_tx.send(RemoteNodeTransportEvent::EnvelopeReceived {
                    node_id: request.node_id.clone(),
                    session_instance_id: session_instance_id.clone(),
                    envelope: Box::new(first_envelope),
                });

                tokio::pin!(shutdown_rx);
                let mut heartbeat = tokio::time::interval_at(
                    tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
                    HEARTBEAT_INTERVAL,
                );
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            break;
                        }
                        _ = heartbeat.tick() => {
                            if session.send(heartbeat_envelope(
                                &request.node_id,
                                &session_instance_id,
                            )).is_err() {
                                break;
                            }
                        }
                        result = tokio::time::timeout(HEARTBEAT_TIMEOUT, inbound.message()) => {
                            match result {
                                Ok(Ok(Some(envelope))) => {
                                    if event_tx.send(RemoteNodeTransportEvent::EnvelopeReceived {
                                        node_id: request.node_id.clone(),
                                        session_instance_id: session_instance_id.clone(),
                                        envelope: Box::new(envelope),
                                    }).is_err() {
                                        ERROR_LOG.log_error(format!(
                                            "client reader: event_tx.send failed for node {}",
                                            request.node_id
                                        ));
                                        break;
                                    }
                                }
                                Ok(Ok(None)) => {
                                    break;
                                }
                                Ok(Err(error)) => {
                                    let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                        node_id: Some(request.node_id.clone()),
                                        session_instance_id: Some(session_instance_id.clone()),
                                        message: error.to_string(),
                                    });
                                    break;
                                }
                                Err(_) => {
                                    ERROR_LOG.log_error(format!(
                                        "client reader: heartbeat timeout for node {} session {}",
                                        request.node_id, session_instance_id
                                    ));
                                    let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                        node_id: Some(request.node_id.clone()),
                                        session_instance_id: Some(session_instance_id.clone()),
                                        message: "heartbeat timeout".to_string(),
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
                let _ = event_tx.send(RemoteNodeTransportEvent::SessionClosed {
                    node_id: request.node_id,
                    session_instance_id,
                });
            });
        }).map_err(|error| RemoteNodeTransportError::new(
            format!("failed to spawn grpc outbound node-session thread: {error}")
        ))?;
        match started_rx.recv() {
            Ok(Ok(())) => {
                let _t_total = t_start.elapsed();
                Ok(GrpcRemoteNodeTransportGuard {
                    shutdown_tx: Some(shutdown_tx),
                    worker: Some(worker),
                    local_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
                })
            }
            Ok(Err(error)) => {
                let t_fail = t_start.elapsed();
                ERROR_LOG.log_error(format!(
                    "connect_outbound FAILED (started err) after {:?}: {}",
                    t_fail, error
                ));
                let _ = shutdown_tx.send(());
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let t_fail = t_start.elapsed();
                ERROR_LOG.log_error(format!(
                    "connect_outbound FAILED (channel closed) after {:?}",
                    t_fail
                ));
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
        let tls_cert_path = self.tls_cert_path.clone();
        let tls_key_path = self.tls_key_path.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let worker = thread::Builder::new()
            .spawn(move || {
                let runtime = match Builder::new_multi_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: None,
                            session_instance_id: None,
                            message: format!(
                                "failed to build grpc remote node transport runtime: {error}"
                            ),
                        });
                        return;
                    }
                };
                runtime.block_on(async move {
                    let failure_tx = event_tx.clone();
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = failure_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                node_id: None,
                                session_instance_id: None,
                                message: format!(
                                    "failed to convert std tcp listener into tokio listener: {error}"
                                ),
                            });
                            return;
                        }
                    };
                    let incoming = TcpListenerStream::new(listener);
                    let session_shutdowns = Arc::new(Mutex::new(Vec::new()));
                    let shutdown_registry = session_shutdowns.clone();
                    let service = TransportNodeSessionService {
                        event_tx,
                        session_shutdowns,
                        authorized_operators_dir: Some(operator_auth::default_authorized_operators_dir()),
                    };
                    let mut server_builder = Server::builder()
                        .tcp_nodelay(true)
                        .tcp_keepalive(Some(TCP_KEEPALIVE_IDLE))
                        .http2_keepalive_interval(Some(HTTP2_KEEPALIVE_INTERVAL))
                        .http2_keepalive_timeout(Some(HTTP2_KEEPALIVE_TIMEOUT));
                    if let (Some(cert_path), Some(key_path)) = (&tls_cert_path, &tls_key_path) {
                        let (cert, key) = match (std::fs::read(cert_path), std::fs::read(key_path)) {
                            (Ok(cert), Ok(key)) => (cert, key),
                            (Err(error), _) | (_, Err(error)) => {
                                let _ = failure_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                    node_id: None,
                                    session_instance_id: None,
                                    message: format!("failed to read TLS certificate/key: {error}"),
                                });
                                return;
                            }
                        };
                        let identity = tonic::transport::Identity::from_pem(cert, key);
                        match server_builder
                            .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))
                        {
                            Ok(builder) => server_builder = builder,
                            Err(error) => {
                                let _ = failure_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                    node_id: None,
                                    session_instance_id: None,
                                    message: format!("failed to configure TLS: {error}"),
                                });
                                return;
                            }
                        }
                    }
                    let server = server_builder
                        .add_service(NodeSessionServiceServer::new(service))
                        .serve_with_incoming_shutdown(incoming, async move {
                            let _ = shutdown_rx.await;
                            let mut guard = shutdown_registry
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            for shutdown in guard.drain(..) {
                                let _ = shutdown.send(());
                            }
                        });
                    if let Err(error) = server.await {
                        let _ = failure_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: None,
                            session_instance_id: None,
                            message: error.to_string(),
                        });
                    }
                });
            })
            .map_err(|error| {
                RemoteNodeTransportError::new(format!(
                    "failed to spawn grpc listen_inbound thread: {error}"
                ))
            })?;
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
    session_shutdowns: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
    authorized_operators_dir: Option<PathBuf>,
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

        let operators = self.load_authorized_operators().await?;
        let require_auth = !operators.is_empty();

        let session_instance_id = format!("server-session-{}", now_millis());
        let (outbound_tx, outbound_rx) = tokio_mpsc::unbounded_channel();
        let (session_shutdown_tx, session_shutdown_rx) = oneshot::channel();
        self.session_shutdowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(session_shutdown_tx);
        let session = RemoteNodeSessionHandle {
            node_id: node_id.clone(),
            session_instance_id: session_instance_id.clone(),
            outbound_tx,
        };

        let challenge = if require_auth {
            let mut challenge = vec![0_u8; OPERATOR_AUTH_CHALLENGE_SIZE];
            getrandom::fill(&mut challenge).map_err(|error| {
                Status::internal(format!("failed to generate operator challenge: {error}"))
            })?;
            Some(challenge)
        } else {
            if operators.is_empty() {
                ERROR_LOG.log_error(format!(
                    "server: accepting node session for {} without operator authentication (no authorized keys)",
                    node_id
                ));
            }
            None
        };

        session
            .send(server_hello_envelope(
                &first_envelope,
                &session_instance_id,
                challenge.clone(),
            ))
            .map_err(|error| Status::unavailable(error.to_string()))?;

        if let Some(challenge) = challenge {
            let auth_envelope = match inbound.message().await? {
                Some(envelope) => envelope,
                None => {
                    return Err(Status::unauthenticated(
                        "node session closed before operator authentication response",
                    ));
                }
            };
            let Some(Body::ClientHello(auth_hello)) = auth_envelope.body.as_ref() else {
                return Err(Status::unauthenticated(
                    "operator authentication response must be a client_hello",
                ));
            };
            if auth_hello.challenge_response.is_empty() {
                return Err(Status::unauthenticated(
                    "operator authentication response is empty",
                ));
            }
            let mut verified = false;
            for (_fingerprint, public_key) in &operators {
                if operator_auth::verify_challenge(
                    &challenge,
                    &auth_hello.auth_scheme,
                    &auth_hello.challenge_response,
                    public_key,
                )
                .is_ok()
                {
                    verified = true;
                    break;
                }
            }
            if !verified {
                return Err(Status::unauthenticated(
                    "operator challenge signature invalid",
                ));
            }
        }

        self.event_tx
            .send(RemoteNodeTransportEvent::SessionOpened {
                session: session.clone(),
            })
            .map_err(|_| Status::unavailable("remote node ingress worker is unavailable"))?;

        let event_tx = self.event_tx.clone();
        let writer_node_id = node_id.clone();
        tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval_at(
                tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
                HEARTBEAT_INTERVAL,
            );
            loop {
                tokio::select! {
                    _ = heartbeat.tick() => {
                        if session.send(heartbeat_envelope(&node_id, &session_instance_id))
                            .is_err()
                        {
                            break;
                        }
                    }
                    result = tokio::time::timeout(HEARTBEAT_TIMEOUT, inbound.message()) => {
                        match result {
                            Ok(Ok(Some(envelope))) => {
                                if event_tx
                                    .send(RemoteNodeTransportEvent::EnvelopeReceived {
                                        node_id: node_id.clone(),
                                        session_instance_id: session_instance_id.clone(),
                                        envelope: Box::new(envelope),
                                    })
                                    .is_err()
                                {
                                    ERROR_LOG.log_error(format!(
                                        "server reader: event_tx.send failed for node {}",
                                        node_id
                                    ));
                                    break;
                                }
                            }
                            Ok(Ok(None)) => {
                                break;
                            }
                            Ok(Err(error)) => {
                                ERROR_LOG.log_error(format!(
                                    "server reader: inbound error for node {}: {}",
                                    node_id, error
                                ));
                                let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                    node_id: Some(node_id.clone()),
                                    session_instance_id: Some(session_instance_id.clone()),
                                    message: error.to_string(),
                                });
                                break;
                            }
                            Err(_) => {
                                ERROR_LOG.log_error(format!(
                                    "server reader: heartbeat timeout for node {} session {}",
                                    node_id, session_instance_id
                                ));
                                let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                                    node_id: Some(node_id.clone()),
                                    session_instance_id: Some(session_instance_id.clone()),
                                    message: "heartbeat timeout".to_string(),
                                });
                                break;
                            }
                        }
                    }
                }
            }
            let _ = event_tx.send(RemoteNodeTransportEvent::SessionClosed {
                node_id,
                session_instance_id,
            });
        });

        let (response_tx, response_rx) = tokio_mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut outbound_rx = outbound_rx;
            tokio::pin!(session_shutdown_rx);
            loop {
                tokio::select! {
                    _ = &mut session_shutdown_rx => {
                        break;
                    }
                    maybe_envelope = outbound_rx.recv() => {
                        let Some(envelope) = maybe_envelope else {
                            break;
                        };
                        if response_tx.send(envelope).is_err() {
                            ERROR_LOG.log_error(format!(
                                "server writer: response_tx.send failed for node {}",
                                writer_node_id
                            ));
                            break;
                        }
                    }
                }
            }
        });
        let outbound_stream = UnboundedReceiverStream::new(response_rx).map(Ok);

        Ok(Response::new(Box::pin(outbound_stream)))
    }
}

impl TransportNodeSessionService {
    async fn load_authorized_operators(&self) -> Result<Vec<(String, ssh_key::PublicKey)>, Status> {
        let Some(dir) = &self.authorized_operators_dir else {
            return Ok(Vec::new());
        };
        match operator_auth::list_authorized_operators(dir) {
            Ok(operators) => Ok(operators),
            Err(error) => Err(Status::internal(format!(
                "failed to load authorized operators: {error}"
            ))),
        }
    }
}

fn server_hello_envelope(
    client_hello: &NodeSessionEnvelope,
    session_instance_id: &str,
    operator_challenge: Option<Vec<u8>>,
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
            operator_challenge: operator_challenge.unwrap_or_default(),
        })),
    }
}

fn heartbeat_envelope(node_id: &str, session_instance_id: &str) -> NodeSessionEnvelope {
    NodeSessionEnvelope {
        message_id: format!("heartbeat-{}", now_millis()),
        sent_at: Some(timestamp_now()),
        session_instance_id: session_instance_id.to_string(),
        correlation_id: None,
        route: None,
        body: Some(Body::Heartbeat(Heartbeat {
            runtime_id: node_id.to_string(),
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
            auth_scheme: "none".to_string(),
            challenge_response: vec![],
        })),
    }
}

fn auth_response_envelope(
    node_id: &str,
    session_instance_id: &str,
    auth_scheme: &str,
    challenge_response: &[u8],
) -> NodeSessionEnvelope {
    NodeSessionEnvelope {
        message_id: format!("client-auth-response-{}", now_millis()),
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
            auth_scheme: auth_scheme.to_string(),
            challenge_response: challenge_response.to_vec(),
        })),
    }
}

fn tls_endpoint_uri(endpoint_uri: &str, tls_pin_sha256: &Option<String>) -> String {
    let bare = endpoint_uri
        .strip_prefix("http://")
        .or_else(|| endpoint_uri.strip_prefix("https://"))
        .or_else(|| endpoint_uri.strip_prefix("tls://"))
        .unwrap_or(endpoint_uri);
    match tls_pin_sha256 {
        // When a TLS pin is configured we use a custom TLS connector that
        // performs the handshake itself. Tonic's `connect_with_connector`
        // requires a plain `http://` URI in that case; using `https://` makes
        // it reject the connection with `HttpsUriWithoutTlsSupport`.
        Some(_) => format!("http://{bare}"),
        None => {
            if endpoint_uri.starts_with("https://") || endpoint_uri.starts_with("tls://") {
                format!("https://{bare}")
            } else {
                format!("http://{bare}")
            }
        }
    }
}

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = format!("{error:#?}");
    let mut source = error.source();
    while let Some(err) = source {
        message.push_str(&format!("\n  caused by: {err:#?}"));
        source = err.source();
    }
    message
}

async fn connect_channel(
    endpoint: &Endpoint,
    tls_pin_sha256: &Option<String>,
) -> Result<Channel, RemoteNodeTransportError> {
    match tls_pin_sha256 {
        Some(pin) => {
            let connector = TlsPinConnector::new(pin.clone())?;
            endpoint
                .connect_with_connector(connector)
                .await
                .map_err(|error| {
                    ERROR_LOG.log_error(format!(
                        "connect_channel (tls pin) failed; pin={pin}; error chain:\n{}",
                        format_error_chain(&error)
                    ));
                    RemoteNodeTransportError::new(error.to_string())
                })
        }
        None => endpoint.connect().await.map_err(|error| {
            ERROR_LOG.log_error(format!(
                "connect_channel (no pin) failed; error chain:\n{}",
                format_error_chain(&error)
            ));
            RemoteNodeTransportError::new(error.to_string())
        }),
    }
}

#[derive(Clone)]
struct TlsPinConnector {
    tls_connector: tokio_rustls::TlsConnector,
    server_name: rustls::pki_types::ServerName<'static>,
}

impl TlsPinConnector {
    fn new(pin: String) -> Result<Self, RemoteNodeTransportError> {
        let verifier = Arc::new(PinnedCertVerifier { pin });
        let mut config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let server_name = rustls::pki_types::ServerName::try_from("waitagent")
            .map_err(|error| RemoteNodeTransportError::new(error.to_string()))?;
        Ok(Self {
            tls_connector: tokio_rustls::TlsConnector::from(Arc::new(config)),
            server_name,
        })
    }
}

impl Service<tonic::transport::Uri> for TlsPinConnector {
    type Response =
        hyper_util::rt::tokio::TokioIo<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: tonic::transport::Uri) -> Self::Future {
        let connector = self.tls_connector.clone();
        let server_name = self.server_name.clone();
        Box::pin(async move {
            let authority = uri
                .authority()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing URI authority")
                })?
                .as_str();
            let (host, port) = authority
                .rsplit_once(':')
                .map(|(host, port)| {
                    let port = port.parse::<u16>().unwrap_or(443);
                    (host.to_string(), port)
                })
                .unwrap_or_else(|| (authority.to_string(), 443));
            let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
            let tls_stream = connector.connect(server_name, stream).await?;
            Ok(hyper_util::rt::tokio::TokioIo::new(tls_stream))
        })
    }
}

#[derive(Debug)]
struct PinnedCertVerifier {
    pin: String,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let spki = crate::infra::node_credentials::extract_spki_from_cert_der(end_entity.as_ref())
            .map_err(|_| {
                rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
            })?;
        let fingerprint = Sha256::digest(&spki);
        let fingerprint = hex_encode(&fingerprint);
        if fingerprint.eq_ignore_ascii_case(&self.pin) {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
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
                RemoteNodeTransportEvent::EnvelopeReceived {
                    node_id, envelope, ..
                } => {
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
                auth_scheme: "none".to_string(),
                challenge_response: vec![],
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
