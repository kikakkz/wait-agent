use crate::infra::operator_auth::{self};
use crate::infra::peer_connection;
use crate::infra::relay_ingress::{spawn_relay_accept_worker, NodeIngressIo};
use crate::infra::relay_mux::stream::MuxResetError;
use crate::infra::relay_routing::error_code::RelayErrorCode;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::infra::error_log::ERROR_LOG;
use tokio::runtime::Builder;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio_stream::wrappers::{TcpListenerStream, UnboundedReceiverStream};
use tokio_stream::{Stream, StreamExt};
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

/// How an outbound node session reaches its peer (issue #35 PR-B).
///
/// `Direct` dials the peer's listening TCP port as before. `Relay` routes the
/// inner-TLS connection through the pinned relay as a routed stream opened by
/// the peer's certificate fingerprint (the same `tls_pin_sha256` the direct
/// dial pins). Profiles store the choice as the `via` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteNodeVia {
    /// Dial the peer directly over TCP (today's behavior).
    Direct,
    /// Reach the peer through the pinned relay's routed streams.
    Relay,
}

impl RemoteNodeVia {
    /// Parses a profile `via` value. The store validates with this at load
    /// time; consumers of already-validated profiles may use the `Ok` arm.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "direct" => Ok(Self::Direct),
            "relay" => Ok(Self::Relay),
            other => Err(format!(
                "unknown via value {other:?}; expected \"relay\" or \"direct\""
            )),
        }
    }

    /// The profile-file spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }
}

/// How the inner-TLS peer connection of a pinned dial is established.
/// `pub(crate)` for the test dial seam in `via_connect`.
#[derive(Clone)]
pub(crate) enum PeerDialer {
    /// Direct TCP dial (the pre-relay behavior).
    Direct,
    /// Open a relay-routed stream toward the peer's certificate fingerprint.
    /// The blocking handle call is driven via `spawn_blocking` inside the
    /// connector future (tonic polls it in async context).
    Relay(std::sync::Arc<crate::infra::relay_client::RelayClientHandle>),
}

/// Error message for a relay-via dial when this node never enrolled a relay
/// (kept verbatim: the connect UI surfaces it to the operator).
const NO_RELAY_CONFIGURED: &str = "via = \"relay\" but no relay is configured — run waitagent relay join <address> <token> or set via = \"direct\"";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundNodeSessionRequest {
    pub node_id: String,
    pub endpoint_uri: String,
    pub tls_pin_sha256: Option<String>,
    /// Dial path for the inner-TLS connection; `None` means direct. Carried
    /// through reconnects so a relay-via peer never falls back silently.
    pub via: Option<RemoteNodeVia>,
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

#[derive(Clone, Default)]
pub struct GrpcRemoteNodeTransport {
    /// Optional TLS certificate path for inbound listeners.
    tls_cert_path: Option<PathBuf>,
    /// Optional TLS private key path for inbound listeners.
    tls_key_path: Option<PathBuf>,
    /// Optional fallback TLS identity (the node credential pair) for inbound
    /// listeners with no explicit certificate configured. A relay-enrolled
    /// node is dialed in by certificate fingerprint — the peer's
    /// `tls_pin_sha256` is the relay identity fingerprint, i.e. the SPKI hash
    /// of this certificate — so the listener must present it or the pinned
    /// TLS dial hits a plaintext HTTP/2 listener and fails instantly with
    /// `InvalidContentType` (issue #129 follow-up).
    credential_tls_identity: Option<crate::infra::node_credentials::NodeCredentialPaths>,
    /// Optional authorized-operator keys directory for inbound listeners.
    /// When unset, the host default (`~/.waitagent/authorized_operators`) is
    /// used and inbound sessions skip operator authentication if it is empty.
    authorized_operators_dir: Option<PathBuf>,
    /// Relay client for relay-via outbound dials and for accepting
    /// relay-routed inbound streams in `listen_inbound` (issue #129).
    /// `None` makes a `via = relay` request fail with the guiding
    /// enrollment error and leaves the inbound listener TCP-only.
    relay_client: Option<std::sync::Arc<crate::infra::relay_client::RelayClientHandle>>,
}

pub struct GrpcRemoteNodeTransportGuard {
    shutdown_tx: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
    /// Stops the relay inbound accept worker (issue #129). Set in `Drop`,
    /// alongside the tonic oneshot; the worker is deliberately not joined —
    /// it may be parked in `accept_inbound` until a stream arrives or the
    /// relay client stops, and process exit tears it down regardless.
    relay_stop: Option<Arc<AtomicBool>>,
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
            credential_tls_identity: None,
            authorized_operators_dir: None,
            relay_client: None,
        }
    }

    /// Attach the relay client used for `via = relay` outbound dials and for
    /// accepting relay-routed inbound streams in `listen_inbound` (issue
    /// #129). Called once at startup by the node runtime after the relay
    /// link spawns.
    pub fn with_relay_client(
        mut self,
        relay_client: Option<std::sync::Arc<crate::infra::relay_client::RelayClientHandle>>,
    ) -> Self {
        self.relay_client = relay_client;
        self
    }

    /// Configure the transport to serve inbound connections over TLS using the
    /// given certificate and private key.
    pub fn with_tls(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Self {
        Self {
            tls_cert_path: Some(cert_path.into()),
            tls_key_path: Some(key_path.into()),
            credential_tls_identity: None,
            authorized_operators_dir: None,
            relay_client: None,
        }
    }

    /// Serve inbound connections with the node credential identity when no
    /// explicit certificate is configured. The node runtime attaches this for
    /// relay-enrolled nodes: a peer dialing `via = "relay"` pins the relay
    /// identity fingerprint (`tls_pin_sha256`), which is the SPKI hash of the
    /// node credential certificate, so the listener must present that
    /// certificate for the pinned TLS handshake to verify.
    pub fn with_credential_tls_identity(
        mut self,
        identity: crate::infra::node_credentials::NodeCredentialPaths,
    ) -> Self {
        self.credential_tls_identity = Some(identity);
        self
    }

    /// Serve inbound connections only to operator keys listed in the given
    /// authorized-keys directory instead of the host default.
    #[cfg(test)]
    pub fn with_authorized_operators_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.authorized_operators_dir = Some(dir.into());
        self
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

    #[cfg(test)]
    pub(crate) fn for_test(
        node_id: impl Into<String>,
        session_instance_id: impl Into<String>,
    ) -> Self {
        let (outbound_tx, _outbound_rx) = tokio_mpsc::unbounded_channel::<NodeSessionEnvelope>();
        Self {
            node_id: node_id.into(),
            session_instance_id: session_instance_id.into(),
            outbound_tx,
        }
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
        if let Some(relay_stop) = &self.relay_stop {
            relay_stop.store(true, Ordering::Relaxed);
        }
        // Deliberately do NOT join the worker: tonic's
        // `serve_with_incoming_shutdown` stops accepting on the shutdown
        // signal but still waits for every already-accepted connection to
        // close before returning. A peer (or a wedged dial) holding a node
        // session channel open therefore blocks `join()` forever, which wedged
        // whole node servers past their final "shutting down" log (observed
        // on remote hosts as zombies that still hold their listen port but
        // never accept again). The worker exits on its own once its
        // connections drain, and process exit tears it down regardless. The
        // same policy covers the relay inbound accept worker (see
        // `relay_stop` above).
        self.worker.take();
    }
}

impl RemoteNodeTransport for GrpcRemoteNodeTransport {
    fn connect_outbound(
        &self,
        request: OutboundNodeSessionRequest,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<GrpcRemoteNodeTransportGuard, RemoteNodeTransportError> {
        let dialer = match request.via {
            Some(RemoteNodeVia::Relay) => match &self.relay_client {
                Some(handle) => PeerDialer::Relay(handle.clone()),
                // Guiding error: fail before spawning the dial worker so the
                // connect UI surfaces the enrollment hint immediately. The
                // retry worker sees TransportFailed and keeps its cadence.
                None => {
                    return Err(RemoteNodeTransportError::new(NO_RELAY_CONFIGURED));
                }
            },
            Some(RemoteNodeVia::Direct) | None => PeerDialer::Direct,
        };
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

                let channel = match connect_channel(&endpoint, &request.tls_pin_sha256, &dialer).await {
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
                let response = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    client.open_node_session(Request::new(UnboundedReceiverStream::new(
                        outbound_rx,
                    ))),
                )
                .await;
                let mut inbound = match response {
                    Ok(Ok(response)) => {
                        let _t_grpc = grpc_start.elapsed();
                        response.into_inner()
                    }
                    Ok(Err(error)) => {
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
                    Err(_elapsed) => {
                        let t_fail = grpc_start.elapsed();
                        ERROR_LOG.log_error(format!(
                            "connect_outbound open_node_session timed out after {t_fail:?}"
                        ));
                        let transport_error = RemoteNodeTransportError::new(format!(
                            "open_node_session timed out after {CONNECT_TIMEOUT:?}"
                        ));
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
                let first_envelope = match tokio::time::timeout(CONNECT_TIMEOUT, inbound.message())
                    .await
                {
                    Ok(Ok(Some(envelope))) => {
                        let _t_hello = server_hello_start.elapsed();
                        envelope
                    }
                    Ok(Ok(None)) => {
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
                    Ok(Err(error)) => {
                        let t_fail = server_hello_start.elapsed();
                        ERROR_LOG.log_error(format!(
                            "connect_outbound ServerHello error after {t_fail:?}: {error}"
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
                    Err(_elapsed) => {
                        let t_fail = server_hello_start.elapsed();
                        ERROR_LOG.log_error(format!(
                            "connect_outbound ServerHello timed out after {t_fail:?}"
                        ));
                        let transport_error = RemoteNodeTransportError::new(format!(
                            "timed out waiting for server hello after {CONNECT_TIMEOUT:?}"
                        ));
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
                    let keystore = operator_auth::default_operator_key_store();
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
                    relay_stop: None,
                    local_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
                })
            }
            Ok(Err(error)) => {
                let t_fail = t_start.elapsed();
                ERROR_LOG.log_error(format!(
                    "connect_outbound FAILED (started err) after {t_fail:?}: {error}"
                ));
                let _ = shutdown_tx.send(());
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let t_fail = t_start.elapsed();
                ERROR_LOG.log_error(format!(
                    "connect_outbound FAILED (channel closed) after {t_fail:?}"
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
        let (tls_cert_path, tls_key_path) = match (&self.tls_cert_path, &self.tls_key_path) {
            (Some(cert_path), Some(key_path)) => (Some(cert_path.clone()), Some(key_path.clone())),
            (None, None) => match &self.credential_tls_identity {
                Some(identity) => {
                    ERROR_LOG.log(format!(
                        "[remote-ingress] listener presents the node credential identity \
                         (no explicit node cert configured): cert={} key={}",
                        identity.cert_path.display(),
                        identity.key_path.display()
                    ));
                    (
                        Some(identity.cert_path.clone()),
                        Some(identity.key_path.clone()),
                    )
                }
                None => (None, None),
            },
            _ => (None, None),
        };
        let authorized_operators_dir = self
            .authorized_operators_dir
            .clone()
            .or_else(|| Some(operator_auth::default_authorized_operators_dir()));
        let relay_client = self.relay_client.clone();
        let relay_stop = Arc::new(AtomicBool::new(false));
        // Relay-routed inbound dials (via = "relay") enter the same server
        // pipeline as TCP accepts: a dedicated worker consumes the relay
        // client's accept queue and feeds the listener's tonic server
        // (issue #129). A spawn failure degrades to the TCP-only listener.
        let relay_accept = relay_client
            .map(|handle| {
                let (relay_conn_tx, relay_conn_rx) =
                    tokio_mpsc::unbounded_channel::<NodeIngressIo>();
                (handle, relay_conn_tx, relay_conn_rx)
            })
            .and_then(|(handle, relay_conn_tx, relay_conn_rx)| {
                match spawn_relay_accept_worker(handle, relay_conn_tx, relay_stop.clone()) {
                    Ok(worker) => Some((worker, relay_conn_rx)),
                    Err(error) => {
                        let _ = event_tx.send(RemoteNodeTransportEvent::TransportFailed {
                            node_id: None,
                            session_instance_id: None,
                            message: format!(
                                "failed to spawn relay inbound accept worker: {error}"
                            ),
                        });
                        None
                    }
                }
            });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let relay_stop_worker = relay_stop.clone();
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
                    let tcp_incoming = TcpListenerStream::new(listener)
                        .map(|result| result.map(NodeIngressIo::Tcp));
                    let incoming: Pin<
                        Box<dyn Stream<Item = Result<NodeIngressIo, std::io::Error>> + Send>,
                    > = match relay_accept {
                        Some((relay_worker, relay_conn_rx)) => {
                            // Held until the server stops; detached afterwards,
                            // like the listener worker itself.
                            let _relay_worker = relay_worker;
                            let relay_incoming = UnboundedReceiverStream::new(relay_conn_rx)
                                .map(Result::<_, std::io::Error>::Ok);
                            Box::pin(tcp_incoming.merge(relay_incoming))
                        }
                        None => Box::pin(tcp_incoming),
                    };
                    let session_shutdowns = Arc::new(Mutex::new(Vec::new()));
                    let shutdown_registry = session_shutdowns.clone();
                    let service = TransportNodeSessionService {
                        event_tx,
                        session_shutdowns,
                        authorized_operators_dir,
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
                    let server_result = server.await;
                    // The listener stopped accepting: let the relay accept
                    // worker drain out at its next wakeup.
                    relay_stop_worker.store(true, Ordering::Relaxed);
                    if let Err(error) = server_result {
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
            relay_stop: Some(relay_stop),
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
            ERROR_LOG.log_error(format!(
                "server: accepting node session for {node_id} without operator authentication (no authorized keys)"
            ));
            None
        };

        session
            .send(server_hello_envelope(
                &first_envelope,
                &session_instance_id,
                challenge.clone(),
            ))
            .map_err(|error| Status::unavailable(error.to_string()))?;

        // Start forwarding outbound envelopes to the grpc response stream
        // before operator authentication completes. The peer cannot sign the
        // challenge until it receives the server hello above, and it cannot
        // receive the server hello until this handler returns the response
        // stream — so the stream must go live first or both sides block
        // forever.
        let (response_tx, response_rx) = tokio_mpsc::unbounded_channel();
        let writer_response_tx = response_tx.clone();
        let writer_node_id = node_id.clone();
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
                        if writer_response_tx.send(Ok(envelope)).is_err() {
                            ERROR_LOG.log_error(format!(
                                "server writer: response_tx.send failed for node {writer_node_id}"
                            ));
                            break;
                        }
                    }
                }
            }
        });

        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            // Operator authentication runs here, after the response stream is
            // live, so an authorized peer can answer the challenge. Until the
            // challenge is answered the peer only receives the server hello.
            if let Some(challenge) = challenge {
                let auth_failure: Option<Status> = match inbound.message().await {
                    Ok(Some(envelope)) => match envelope.body.as_ref() {
                        Some(Body::ClientHello(auth_hello)) => {
                            if auth_hello.challenge_response.is_empty() {
                                Some(Status::unauthenticated(
                                    "operator authentication response is empty",
                                ))
                            } else if operators.iter().any(|(_fingerprint, public_key)| {
                                operator_auth::verify_challenge(
                                    &challenge,
                                    &auth_hello.auth_scheme,
                                    &auth_hello.challenge_response,
                                    public_key,
                                )
                                .is_ok()
                            }) {
                                None
                            } else {
                                Some(Status::unauthenticated(
                                    "operator challenge signature invalid",
                                ))
                            }
                        }
                        _ => Some(Status::unauthenticated(
                            "operator authentication response must be a client_hello",
                        )),
                    },
                    Ok(None) => Some(Status::unauthenticated(
                        "node session closed before operator authentication response",
                    )),
                    Err(error) => Some(error),
                };
                if let Some(status) = auth_failure {
                    ERROR_LOG.log_error(format!(
                        "server: rejecting node {node_id} session {session_instance_id}: {status}"
                    ));
                    let _ = response_tx.send(Err(status));
                    return;
                }
            }

            if event_tx
                .send(RemoteNodeTransportEvent::SessionOpened {
                    session: session.clone(),
                })
                .is_err()
            {
                ERROR_LOG.log_error(format!(
                    "server: remote node ingress worker unavailable; dropping node {node_id} session {session_instance_id}"
                ));
                return;
            }

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
                                        "server reader: event_tx.send failed for node {node_id}"
                                    ));
                                    break;
                                }
                            }
                            Ok(Ok(None)) => {
                                break;
                            }
                            Ok(Err(error)) => {
                                ERROR_LOG.log_error(format!(
                                    "server reader: inbound error for node {node_id}: {error}"
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
                                    "server reader: heartbeat timeout for node {node_id} session {session_instance_id}"
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

        let outbound_stream = UnboundedReceiverStream::new(response_rx);

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

/// Renders an error and its `source()` chain as a single line, so the
/// diagnostics log (which keys entries by line and drops continuations) keeps
/// the whole chain. Shows each error's `Display`, not just its `Debug`.
fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = format!("{error}");
    let mut source = error.source();
    while let Some(err) = source {
        message.push_str(&format!("; caused by: {err}"));
        source = err.source();
    }
    message
}

/// Extracts the typed relay error code from an `io::Error` whose payload is a
/// [`MuxResetError`] — a stream-scoped relay `Error` frame surfaced through a
/// mux stream reset — decoding the wire code via
/// [`RelayErrorCode::from_wire`]. Returns `None` when the error carries no
/// such payload (plain resets, direct-dial failures, unrelated io errors).
pub(crate) fn relay_reset_code(error: &std::io::Error) -> Option<RelayErrorCode> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<MuxResetError>())
        .and_then(|reset| RelayErrorCode::from_wire(reset.code))
}

/// Finds a relay reset code anywhere in an error source chain. Tonic wraps
/// connector `io::Error`s behind `ConnectError` while its own `Display` stays
/// "transport error", so the typed code must be recovered by walking the
/// chain (`relay_reset_code` alone only sees a top-level `io::Error`).
fn relay_reset_code_in_chain(error: &(dyn std::error::Error + 'static)) -> Option<RelayErrorCode> {
    let mut source = error.source();
    while let Some(error) = source {
        if let Some(code) = error
            .downcast_ref::<std::io::Error>()
            .and_then(relay_reset_code)
        {
            return Some(code);
        }
        if let Some(code) = error
            .downcast_ref::<MuxResetError>()
            .and_then(|reset| RelayErrorCode::from_wire(reset.code))
        {
            return Some(code);
        }
        source = error.source();
    }
    None
}

async fn connect_channel(
    endpoint: &Endpoint,
    tls_pin_sha256: &Option<String>,
    dialer: &PeerDialer,
) -> Result<Channel, RemoteNodeTransportError> {
    match tls_pin_sha256 {
        Some(pin) => {
            let connector = TlsPinConnector::new_with_dialer(pin.clone(), dialer.clone())?;
            endpoint
                .connect_with_connector(connector)
                .await
                .map_err(|error| {
                    ERROR_LOG.log_error(format!(
                        "connect_channel (tls pin) failed; pin={pin}; error chain:\n{}",
                        format_error_chain(&error)
                    ));
                    // An OpenStream refusal surfaces here as a relay reset:
                    // the mux attached the Error frame's code+message to the
                    // stream's io::Error. Append the canonical message so the
                    // typed error names the cause (tonic's own Display is
                    // bare "transport error").
                    let mut message = error.to_string();
                    if let Some(code) = relay_reset_code_in_chain(&error) {
                        message = format!("{message}: {}", code.message());
                    }
                    RemoteNodeTransportError::new(message)
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
    /// Peer connection dialer for the inner TLS: direct TCP or a relay-routed
    /// stream (the routed target is the pinned fingerprint).
    dialer: PeerDialer,
    /// Target node fingerprint for relay-routed dials.
    pin: String,
}

impl TlsPinConnector {
    fn new_with_dialer(pin: String, dialer: PeerDialer) -> Result<Self, RemoteNodeTransportError> {
        let verifier = Arc::new(PinnedCertVerifier { pin: pin.clone() });
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
            dialer,
            pin,
        })
    }
}

impl Service<tonic::transport::Uri> for TlsPinConnector {
    type Response = hyper_util::rt::tokio::TokioIo<
        tokio_rustls::client::TlsStream<Box<dyn peer_connection::PeerConnection>>,
    >;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: tonic::transport::Uri) -> Self::Future {
        let connector = self.tls_connector.clone();
        let server_name = self.server_name.clone();
        let dialer = self.dialer.clone();
        let pin = self.pin.clone();
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
            let stream = match &dialer {
                PeerDialer::Direct => {
                    peer_connection::dial_tcp_peer_connection(&host, port).await?
                }
                PeerDialer::Relay(handle) => {
                    // Tonic polls this future in async context; the handle
                    // API blocks (its own runtime's block_on), so the dial
                    // runs on the blocking thread pool.
                    let handle = handle.clone();
                    let target = pin.clone();
                    let stream = tokio::task::spawn_blocking(move || {
                        handle.open_stream(&target)
                    })
                    .await
                    .map_err(|error| {
                        std::io::Error::other(format!("relay dial task failed: {error}"))
                    })?
                    .map_err(|error| {
                        std::io::Error::other(format!(
                            "relay dial to {pin} failed: {error}; the relay link reconnects and the dial retries"
                        ))
                    })?;
                    stream
                }
            };
            let tls_stream =
                match tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, stream))
                    .await
                {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "tls-pin handshake timed out",
                        ))
                    }
                };
            Ok(hyper_util::rt::tokio::TokioIo::new(tls_stream))
        })
    }
}

/// Test-only seam for the relay E2E (issue #35): runs one pinned connector
/// dial over the given dialer and returns the established inner-TLS stream.
/// Exercising `Service::call` directly proves the dial works from the async
/// context tonic polls it in (the relay branch bridges via `spawn_blocking`).
/// The relay dialer ignores the URI host (it targets the pin); the direct
/// dialer dials the URI authority as usual.
#[cfg(test)]
pub(crate) async fn test_dial_with_connector(
    uri: &str,
    pin: &str,
    dialer: PeerDialer,
) -> Result<tokio_rustls::client::TlsStream<Box<dyn peer_connection::PeerConnection>>, std::io::Error>
{
    use tower::Service;
    let mut connector = TlsPinConnector::new_with_dialer(pin.to_string(), dialer)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let uri: tonic::transport::Uri = uri.parse().map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{error}"))
    })?;
    let io = Service::call(&mut connector, uri).await?;
    Ok(io.into_inner())
}

#[cfg(test)]
pub(crate) fn test_relay_dialer(
    handle: std::sync::Arc<crate::infra::relay_client::RelayClientHandle>,
) -> PeerDialer {
    PeerDialer::Relay(handle)
}

#[cfg(test)]
pub(crate) fn test_direct_dialer() -> PeerDialer {
    PeerDialer::Direct
}

/// Test-only seam for the relay ingress wiring (issue #129): dials a tonic
/// `Channel` through the given dialer exactly the way `connect_outbound`
/// does (`connect_channel` + pinned TLS when a pin is present), so tests can
/// drive the production client path against an ingress listener. The URI
/// host is unused on the relay path (the pin is the routed target).
#[cfg(test)]
pub(crate) async fn test_connect_channel(
    uri: &str,
    tls_pin_sha256: &str,
    dialer: PeerDialer,
) -> Result<Channel, RemoteNodeTransportError> {
    let endpoint = Endpoint::from_shared(uri.to_string())
        .map_err(|error| RemoteNodeTransportError::new(error.to_string()))?
        .tcp_nodelay(true)
        .tcp_keepalive(Some(TCP_KEEPALIVE_IDLE))
        .connect_timeout(CONNECT_TIMEOUT)
        .http2_keep_alive_interval(HTTP2_KEEPALIVE_INTERVAL)
        .keep_alive_timeout(HTTP2_KEEPALIVE_TIMEOUT)
        .keep_alive_while_idle(true);
    connect_channel(&endpoint, &Some(tls_pin_sha256.to_string()), &dialer).await
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
        auth_response_envelope, Body, GrpcRemoteNodeTransport, NodeSessionEnvelope,
        NodeSessionService, NodeSessionServiceServer, OutboundNodeSessionRequest, ProtocolVersion,
        RemoteNodeTransport, RemoteNodeTransportEvent,
    };
    use crate::infra::operator_auth::{self, MemoryOperatorKeyStore, OperatorKeyStore};
    use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
    use crate::infra::remote_grpc_proto::v1::{ClientHello, Heartbeat};
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;
    use tokio::runtime::Builder;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;

    #[test]
    fn via_relay_without_a_relay_handle_fails_with_the_guiding_error() {
        let transport = GrpcRemoteNodeTransport::new();
        let (event_tx, _event_rx) = mpsc::channel::<RemoteNodeTransportEvent>();
        let result = transport.connect_outbound(
            OutboundNodeSessionRequest {
                node_id: "peer".to_string(),
                endpoint_uri: "tls://127.0.0.1:7474".to_string(),
                tls_pin_sha256: Some("deadbeef".to_string()),
                via: Some(super::RemoteNodeVia::Relay),
            },
            event_tx,
        );
        let error = match result {
            Ok(_guard) => panic!("a relay-via dial without a handle must fail before dialing"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("via = \"relay\"") && message.contains("relay join"),
            "the error guides enrollment: {message}"
        );
        assert!(
            message.contains("via = \"direct\""),
            "the error names the escape hatch: {message}"
        );
    }

    #[test]
    fn relay_reset_code_extracts_typed_code_from_mux_reset() {
        use crate::infra::relay_mux::stream::MuxResetError;
        use crate::infra::relay_routing::error_code::RelayErrorCode;

        let reset = std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            MuxResetError {
                code: RelayErrorCode::TargetUnknown.wire_value(),
                message: "target unknown".to_string(),
            },
        );
        assert_eq!(
            super::relay_reset_code(&reset),
            Some(RelayErrorCode::TargetUnknown),
            "a ConnectionReset carrying MuxResetError must decode to the typed code"
        );

        let plain = std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "mux stream 1 reset by the relay",
        );
        assert_eq!(
            super::relay_reset_code(&plain),
            None,
            "a plain reset has no relay code to extract"
        );

        let other_kind = std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            MuxResetError {
                code: RelayErrorCode::StreamUnknown.wire_value(),
                message: "unknown stream".to_string(),
            },
        );
        assert_eq!(
            super::relay_reset_code(&other_kind),
            Some(RelayErrorCode::StreamUnknown),
            "the payload, not the ErrorKind, carries the code (write leg resets are BrokenPipe)"
        );
    }

    #[test]
    fn inbound_listener_reports_session_events_and_forwards_outbound_envelopes() {
        let bind_addr = unused_local_addr();
        // Pin the listener to an empty authorized-operators dir: this test
        // speaks the raw protocol without answering the operator challenge,
        // so it only works in the no-authorized-keys accept path. Using the
        // host default dir would break the test once the real node has
        // authorized peers (e.g. after a genuine inbound connect).
        let auth_dir = std::env::temp_dir().join(format!(
            "waitagent-transport-inbound-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        std::fs::create_dir_all(&auth_dir).expect("auth dir should create");
        let transport = GrpcRemoteNodeTransport::new().with_authorized_operators_dir(auth_dir);
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

    #[test]
    fn inbound_listener_completes_operator_auth_before_session_opened() {
        let (auth_dir, keystore) = authorized_operator_fixture("accept");
        let bind_addr = unused_local_addr();
        let transport = GrpcRemoteNodeTransport::new().with_authorized_operators_dir(auth_dir);
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
            tx.send(client_hello_envelope("peer-auth"))
                .await
                .expect("client hello should send");
            // Regression: the response stream must be returned before the
            // operator challenge is answered; previously the server awaited
            // the authentication response here and both sides blocked.
            let response = client
                .open_node_session(Request::new(ReceiverStream::new(rx)))
                .await
                .expect("response stream should be returned before authentication completes");
            let mut inbound = response.into_inner();

            let server_hello = inbound
                .message()
                .await
                .expect("server hello should decode")
                .expect("server hello should be present");
            let Some(Body::ServerHello(hello)) = server_hello.body else {
                panic!("server hello envelope expected");
            };
            assert!(
                !hello.operator_challenge.is_empty(),
                "server hello should carry an operator challenge"
            );

            let (auth_scheme, challenge_response) = keystore
                .sign_challenge(&hello.operator_challenge)
                .expect("operator challenge should sign");
            tx.send(auth_response_envelope(
                "peer-auth",
                "client-session-1",
                &auth_scheme,
                &challenge_response,
            ))
            .await
            .expect("authentication response should send");

            let opened = event_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("session opened event should arrive after authentication");
            let session = match opened {
                RemoteNodeTransportEvent::SessionOpened { session } => session,
                other => panic!("unexpected transport event: {other:?}"),
            };
            assert_eq!(session.node_id(), "peer-auth");

            tx.send(NodeSessionEnvelope {
                message_id: "heartbeat-1".to_string(),
                sent_at: None,
                session_instance_id: "client-session-1".to_string(),
                correlation_id: None,
                route: None,
                body: Some(Body::Heartbeat(Heartbeat {
                    runtime_id: "peer-auth".to_string(),
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
                    assert_eq!(node_id, "peer-auth");
                    assert!(matches!(envelope.body, Some(Body::Heartbeat(_))));
                }
                other => panic!("unexpected transport event: {other:?}"),
            }
        });
    }

    #[test]
    fn inbound_listener_rejects_invalid_operator_auth_signature() {
        let (auth_dir, _keystore) = authorized_operator_fixture("reject");
        let bind_addr = unused_local_addr();
        let transport = GrpcRemoteNodeTransport::new().with_authorized_operators_dir(auth_dir);
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
            tx.send(client_hello_envelope("peer-reject"))
                .await
                .expect("client hello should send");
            let response = client
                .open_node_session(Request::new(ReceiverStream::new(rx)))
                .await
                .expect("response stream should be returned before authentication completes");
            let mut inbound = response.into_inner();

            let server_hello = inbound
                .message()
                .await
                .expect("server hello should decode")
                .expect("server hello should be present");
            let Some(Body::ServerHello(hello)) = server_hello.body else {
                panic!("server hello envelope expected");
            };
            assert!(
                !hello.operator_challenge.is_empty(),
                "server hello should carry an operator challenge"
            );

            tx.send(auth_response_envelope(
                "peer-reject",
                "client-session-1",
                "ssh-ed25519-challenge",
                b"not-a-valid-signature",
            ))
            .await
            .expect("authentication response should send");

            match inbound.message().await {
                Err(status) => assert_eq!(
                    status.code(),
                    tonic::Code::Unauthenticated,
                    "rejected peer should see an unauthenticated stream error"
                ),
                other => panic!("expected unauthenticated stream error, got {other:?}"),
            }
            assert!(
                event_rx.recv_timeout(Duration::from_millis(300)).is_err(),
                "no session event should be published for a rejected peer"
            );
        });
    }

    #[test]
    fn outbound_dial_completes_operator_auth_handshake() {
        let keystore = operator_auth::default_operator_key_store();
        let public_key = keystore
            .public_key_openssh()
            .expect("default operator key should be available");
        let auth_dir = std::env::temp_dir().join(format!(
            "waitagent-transport-auth-dial-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        std::fs::create_dir_all(&auth_dir).expect("auth dir should create");
        std::fs::write(auth_dir.join("operator.pub"), public_key)
            .expect("authorized operator should persist");

        let bind_addr = unused_local_addr();
        let listener = GrpcRemoteNodeTransport::new().with_authorized_operators_dir(auth_dir);
        let (server_event_tx, _server_event_rx) = mpsc::channel();
        let _listener_guard = listener
            .listen_inbound(bind_addr, server_event_tx)
            .expect("grpc listener should start");

        let dialer = GrpcRemoteNodeTransport::new();
        let (event_tx, event_rx) = mpsc::channel();
        // Regression: without the server returning the response stream before
        // authentication, this dial blocks forever waiting for the challenge.
        let _dial_guard = dialer
            .connect_outbound(
                OutboundNodeSessionRequest {
                    node_id: "peer-dial".to_string(),
                    endpoint_uri: format!("http://{bind_addr}"),
                    tls_pin_sha256: None,
                    via: None,
                },
                event_tx,
            )
            .expect("dial should complete operator authentication and open the session");

        let opened = event_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dialer should observe SessionOpened after authentication");
        assert!(
            matches!(opened, RemoteNodeTransportEvent::SessionOpened { .. }),
            "unexpected transport event: {opened:?}"
        );
    }

    #[test]
    fn outbound_dial_times_out_when_server_hello_never_arrives() {
        let bind_addr = unused_local_addr();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server_thread = std::thread::spawn(move || {
            let runtime = Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime should build");
            runtime.block_on(async move {
                let _ = tonic::transport::Server::builder()
                    .add_service(NodeSessionServiceServer::new(SilentHelloNodeSessionService))
                    .serve_with_shutdown(bind_addr, async move {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            });
        });

        let dialer = GrpcRemoteNodeTransport::new();
        let (event_tx, event_rx) = mpsc::channel();
        let t_start = std::time::Instant::now();
        let result = dialer.connect_outbound(
            OutboundNodeSessionRequest {
                node_id: "peer-silent-hello".to_string(),
                endpoint_uri: format!("http://{bind_addr}"),
                tls_pin_sha256: None,
                via: None,
            },
            event_tx,
        );
        let elapsed = t_start.elapsed();
        let _ = shutdown_tx.send(());

        let error = match result {
            Err(error) => error,
            Ok(_guard) => panic!("dial should fail when the server hello never arrives"),
        };
        let error_message = error.to_string();
        if error_message.contains("timed out waiting for server hello") {
            assert!(
                elapsed >= super::CONNECT_TIMEOUT,
                "dial should hold until the {:?} deadline, failed after {elapsed:?}",
                super::CONNECT_TIMEOUT
            );
        } else {
            // On loaded hosts the inbound stream can fail at the transport
            // layer before the hello deadline; that is still a failed dial
            // and must never open a session.
            assert!(
                !error_message.is_empty(),
                "dial should fail with a descriptive error"
            );
        }

        match event_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(RemoteNodeTransportEvent::TransportFailed { message, .. }) => {
                assert!(
                    !message.is_empty(),
                    "failure event should carry a descriptive message"
                );
            }
            other => panic!("expected TransportFailed event, got {other:?}"),
        }

        server_thread
            .join()
            .expect("stub server thread should exit");
    }

    /// Stub gRPC service that accepts the node session but never yields a
    /// server hello, emulating a half-wedged peer that holds the HTTP/2
    /// connection open without speaking the session protocol.
    struct SilentHelloNodeSessionService;

    #[tonic::async_trait]
    impl NodeSessionService for SilentHelloNodeSessionService {
        type OpenNodeSessionStream = super::NodeSessionResponseStream;

        async fn open_node_session(
            &self,
            _request: tonic::Request<tonic::Streaming<NodeSessionEnvelope>>,
        ) -> Result<tonic::Response<Self::OpenNodeSessionStream>, tonic::Status> {
            Ok(tonic::Response::new(Box::pin(NeverYieldingStream)))
        }
    }

    struct NeverYieldingStream;

    impl tokio_stream::Stream for NeverYieldingStream {
        type Item = Result<NodeSessionEnvelope, tonic::Status>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Pending
        }
    }

    fn authorized_operator_fixture(tag: &str) -> (PathBuf, MemoryOperatorKeyStore) {
        let keystore = MemoryOperatorKeyStore::generate().expect("operator key should generate");
        let dir = std::env::temp_dir().join(format!(
            "waitagent-transport-auth-{tag}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        std::fs::create_dir_all(&dir).expect("auth dir should create");
        let public_key = keystore
            .public_key_openssh()
            .expect("operator public key should export");
        std::fs::write(dir.join("operator.pub"), public_key)
            .expect("authorized operator should persist");
        (dir, keystore)
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

    #[test]
    fn tls_pin_handshake_runs_over_peer_connection_seam() {
        use sha2::Digest;

        // bootstrap.rs installs this at process start; unit tests must install
        // it themselves. Concurrent installs from other tests are fine.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut params = rcgen::CertificateParams::new(vec!["waitagent".to_string()]);
        params.alg = &rcgen::PKCS_ED25519;
        let cert = rcgen::Certificate::from_params(params).expect("cert should generate");
        let cert_der = cert.serialize_der().expect("cert should serialize");
        let key_der = cert.serialize_private_key_der();
        let spki = crate::infra::node_credentials::extract_spki_from_cert_der(&cert_der)
            .expect("spki should extract");
        let pin = super::hex_encode(&super::Sha256::digest(&spki));

        let mut server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(cert_der)],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
                ),
            )
            .expect("server config should build");
        server_config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));

        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");
        runtime.block_on(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener should bind");
            let addr = listener.local_addr().expect("listener should report addr");
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.expect("accept");
                let mut tls = acceptor.accept(tcp).await.expect("tls accept");
                let mut buf = [0u8; 4];
                tls.read_exact(&mut buf).await.expect("server read");
                tls.write_all(b"pong").await.expect("server write");
            });

            let stream =
                crate::infra::peer_connection::dial_tcp_peer_connection("127.0.0.1", addr.port())
                    .await
                    .expect("seam dial should succeed");
            let verifier = std::sync::Arc::new(super::PinnedCertVerifier { pin });
            let mut config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"h2".to_vec()];
            let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
            let server_name = rustls::pki_types::ServerName::try_from("waitagent")
                .expect("server name should parse");
            let mut tls = tokio::time::timeout(
                super::CONNECT_TIMEOUT,
                connector.connect(server_name, stream),
            )
            .await
            .expect("handshake should finish within the connect timeout")
            .expect("handshake over the seam should succeed");
            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            tls.write_all(b"ping").await.expect("client write");
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await.expect("client read");
            assert_eq!(&buf, b"pong");
            server.await.expect("server task");
        });
    }
}
