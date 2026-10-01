//! Relay transport gate and connection lifecycle: TLS listener requiring
//! client certificates whose SHA-256 SPKI fingerprint is whitelisted in an
//! `authorized_nodes/` directory; authenticated links then speak the framed
//! relay protocol (docs/relay-design.md 协议分层): the first frame must be
//! `Register`, after which the link is a connection-table entry kept alive
//! by heartbeats and removed by unregister, link loss, replacement, or
//! heartbeat-timeout eviction (node 生命周期).
//!
//! Device identity is the self-signed certificate fingerprint (no CA); the
//! relay requests a client certificate during the mTLS handshake, and a
//! fingerprint that is not whitelisted fails the handshake at the transport
//! layer — the same layer where revoked nodes fail on their next handshake
//! after their whitelist entry is removed.
//!
//! The connection table is memory-only (数据策略): the relay persists no
//! session or traffic data. Stream routing (`open_stream` between
//! registered nodes) lands with the routing slice.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_connection_table::{
    RelayConnectionTable, RelayLifecycleConfig, RelayLifecycleEvent,
};
use crate::infra::relay_mux::frame::{read_frame, Frame};

// rustls re-exports `HandshakeSignatureValid` under `client::danger` for both
// verifier kinds; there is no `server::danger` re-export.
use rustls::client::danger::HandshakeSignatureValid;

/// Default relay listen port. Provisional until the relay.toml config slice;
/// the node side uses 7474.
pub const DEFAULT_RELAY_LISTEN_PORT: u16 = 7475;

/// Upper bound on a single mTLS handshake before the connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors that can occur while starting the relay transport gate.
#[derive(Debug, Error)]
pub enum RelayServerError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("relay credentials error: {0}")]
    Credentials(#[from] node_credentials::NodeCredentialsError),
    #[error("no certificate found in {0}")]
    MissingCertificate(PathBuf),
    #[error("no private key found in {0}")]
    MissingPrivateKey(PathBuf),
    #[error("rustls error: {0}")]
    Tls(#[from] rustls::Error),
}

/// Configuration for [`start`].
#[derive(Debug, Clone)]
pub struct RelayServeConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Whitelist directory: one file per authorized node, named by the
    /// lowercase hex SHA-256 SPKI fingerprint of the node's certificate.
    pub authorized_nodes_dir: PathBuf,
    /// The relay's own identity (self-signed certificate).
    pub credentials: NodeCredentialPaths,
    /// Connection-table lifecycle timing (heartbeat eviction and the
    /// register deadline).
    pub lifecycle: RelayLifecycleConfig,
}

impl RelayServeConfig {
    /// Returns the default configuration for `listen`: whitelist under
    /// `~/.waitagent/authorized_nodes/`, credentials at the default paths,
    /// lifecycle timing per docs/relay-design.md (node 生命周期).
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            authorized_nodes_dir: crate::host::ssh::remote_host_home::waitagent_home()
                .join("authorized_nodes"),
            credentials: NodeCredentialPaths::default_paths(),
            lifecycle: RelayLifecycleConfig::default(),
        }
    }
}

/// Returns the whitelisted node fingerprints in `dir` (the lowercase file
/// names). A missing directory means an empty whitelist: nothing is
/// authorized until the invite/join flow adds entries. Mirrors the
/// `authorized_operators` scan in `operator_auth::list_authorized_operators`.
pub fn authorized_node_fingerprints(dir: &Path) -> io::Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        out.push(name.to_lowercase());
    }
    Ok(out)
}

/// rustls client-certificate verifier implementing the whitelist: the
/// end-entity certificate's SPKI SHA-256 fingerprint must match a file in
/// `authorized_nodes/`.
///
/// The TLS 1.2/1.3 signature checks are delegated to ring so the client must
/// prove possession of the private key — without them a replayed whitelisted
/// certificate would authenticate.
///
/// Note on ordering: in TLS 1.3 the server sends its flight (including the
/// certificate request) before the client certificate is evaluated, so a
/// rejection reaches the peer as a post-handshake alert — the server-side
/// handshake still fails at the transport layer and no application data is
/// accepted from the rejected peer.
#[derive(Debug)]
struct WhitelistedClientCertVerifier {
    authorized_nodes_dir: PathBuf,
}

impl WhitelistedClientCertVerifier {
    fn new(authorized_nodes_dir: PathBuf) -> Self {
        Self {
            authorized_nodes_dir,
        }
    }

    fn is_authorized(&self, fingerprint: &str) -> Result<bool, rustls::Error> {
        let whitelisted =
            authorized_node_fingerprints(&self.authorized_nodes_dir).map_err(|error| {
                rustls::Error::General(format!(
                    "authorized_nodes directory {} unreadable: {error}",
                    self.authorized_nodes_dir.display()
                ))
            })?;
        Ok(whitelisted
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(fingerprint)))
    }
}

impl rustls::server::danger::ClientCertVerifier for WhitelistedClientCertVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        let fingerprint = node_credentials::cert_fingerprint_from_der(end_entity.as_ref())
            .map_err(|_| {
                rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
            })?;
        if self.is_authorized(&fingerprint)? {
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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

/// Handle to a running relay transport gate.
pub struct RelayServerHandle {
    local_addr: SocketAddr,
    table: Arc<RelayConnectionTable>,
    shutdown_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl RelayServerHandle {
    /// Returns the address the listener actually bound to (port 0 resolves).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns the number of registered nodes in the connection table.
    pub fn active_connections(&self) -> usize {
        self.table.len()
    }

    /// Stops the accept loop, the sweeper, and every link task.
    ///
    /// Dropping the handle without calling `shutdown` has the same effect:
    /// the watch senders drop, the loops observe `changed()` resolving, and
    /// the accept loop breaks out of its select.
    // Used by tests today; the admin socket slice (#55) wires it into
    // graceful daemon shutdown.
    #[allow(dead_code)]
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.task.await;
    }
}

/// A running relay: the server handle plus the lifecycle event stream.
/// Events are ephemeral notifications (see [`RelayLifecycleEvent`]); dropping
/// the receiver never affects connection handling.
pub struct StartedRelay {
    pub server: RelayServerHandle,
    pub events: mpsc::Receiver<RelayLifecycleEvent>,
}

const EVENT_QUEUE: usize = 128;

/// Starts the relay transport gate: binds the listener, ensures the relay's
/// own credentials, and spawns the accept loop plus the eviction sweeper.
/// Every authenticated link runs the register/heartbeat lifecycle and owns
/// its connection-table entry until unregister, loss, replacement, or
/// eviction.
pub async fn start(config: RelayServeConfig) -> Result<StartedRelay, RelayServerError> {
    node_credentials::ensure_credentials(&config.credentials)?;
    let cert_pem = fs::read_to_string(&config.credentials.cert_path)?;
    let key_pem = fs::read_to_string(&config.credentials.key_path)?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes()).collect::<Result<_, _>>()?;
    if certs.is_empty() {
        return Err(RelayServerError::MissingCertificate(
            config.credentials.cert_path.clone(),
        ));
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or_else(|| RelayServerError::MissingPrivateKey(config.credentials.key_path.clone()))?;
    let server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(WhitelistedClientCertVerifier::new(
            config.authorized_nodes_dir.clone(),
        )))
        .with_single_cert(certs, key)?;

    let listener = TcpListener::bind(config.listen).await?;
    let local_addr = listener.local_addr()?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let table = Arc::new(RelayConnectionTable::default());
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
    let lifecycle = config.lifecycle.clone();

    let sweeper_table = table.clone();
    let sweeper_lifecycle = lifecycle.clone();
    let mut sweeper_shutdown = shutdown_tx.subscribe();
    let sweeper_events = events_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = sweeper_shutdown.changed() => return,
                _ = tokio::time::sleep(sweeper_lifecycle.sweep_interval) => {
                    for (node_id, entry) in sweeper_table.evict_idle(sweeper_lifecycle.offline_after) {
                        let _ = entry.retire_tx.send(true);
                        let _ = sweeper_events.try_send(RelayLifecycleEvent::EvictedOffline { node_id });
                    }
                }
            }
        }
    });

    let task_table = table.clone();
    let task_events = events_tx;

    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((tcp, peer_addr)) = accepted else {
                        ERROR_LOG.log_error("[relay] listener accept failed; stopping".to_string());
                        break;
                    };
                    let acceptor = acceptor.clone();
                    let table = task_table.clone();
                    let events = task_events.clone();
                    let lifecycle = lifecycle.clone();
                    tokio::spawn(async move {
                        let handshake =
                            tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await;
                        let tls = match handshake {
                            Err(_) => {
                                ERROR_LOG.log_error(format!(
                                    "[relay] {peer_addr}: handshake timed out"
                                ));
                                return;
                            }
                            Ok(Err(error)) => {
                                ERROR_LOG.log_error(format!(
                                    "[relay] {peer_addr}: handshake rejected: {error}"
                                ));
                                return;
                            }
                            Ok(Ok(tls)) => tls,
                        };
                        run_link_lifecycle(tls, peer_addr, table, events, lifecycle).await;
                    });
                }
            }
        }
    });

    Ok(StartedRelay {
        server: RelayServerHandle {
            local_addr,
            table,
            shutdown_tx,
            task,
        },
        events: events_rx,
    })
}

type ServerTls = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

/// Post-handshake link lifecycle: register, heartbeat, unregister.
async fn run_link_lifecycle(
    mut tls: ServerTls,
    peer_addr: SocketAddr,
    table: Arc<RelayConnectionTable>,
    events: mpsc::Sender<RelayLifecycleEvent>,
    lifecycle: RelayLifecycleConfig,
) {
    let peer_fingerprint = {
        let (_, server_conn) = tls.get_ref();
        match server_conn
            .peer_certificates()
            .and_then(|certs| certs.first())
        {
            Some(cert) => match node_credentials::cert_fingerprint_from_der(cert.as_ref()) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    ERROR_LOG.log_error(format!(
                        "[relay] {peer_addr}: peer certificate fingerprint failed: {error}"
                    ));
                    return;
                }
            },
            // Mandatory client auth makes this an invariant; a missing peer
            // certificate means the rustls contract broke — close the link
            // rather than continue without identity.
            None => {
                ERROR_LOG.log_error(format!(
                    "[relay] {peer_addr}: no peer certificate after mandatory client auth"
                ));
                return;
            }
        }
    };

    // The first frame must be Register within the deadline.
    let first = tokio::time::timeout(lifecycle.register_timeout, read_frame(&mut tls)).await;
    let node_id = match first {
        Err(_) => {
            ERROR_LOG.log_error(format!("[relay] {peer_addr}: register deadline exceeded"));
            return;
        }
        Ok(Err(error)) => {
            ERROR_LOG.log_error(format!(
                "[relay] {peer_addr}: link closed before register: {error}"
            ));
            return;
        }
        Ok(Ok(Frame::Register { node_id })) => node_id,
        Ok(Ok(other)) => {
            ERROR_LOG.log_error(format!(
                "[relay] {peer_addr}: first frame must be Register, got {other:?}"
            ));
            return;
        }
    };

    // The node id is self-proving only when it equals the mTLS fingerprint
    // (docs/relay-design.md node_id 策略); anything else is a
    // misconfiguration or an impostor and must never register.
    if !node_id.eq_ignore_ascii_case(&peer_fingerprint) {
        ERROR_LOG.log_error(format!(
            "[relay] {peer_addr}: register node id {node_id:?} does not match peer fingerprint {peer_fingerprint}"
        ));
        return;
    }

    let registered = table.register(&node_id);
    match &registered.previous {
        Some(previous) => {
            let _ = previous.retire_tx.send(true);
            let _ = events.try_send(RelayLifecycleEvent::Replaced {
                node_id: node_id.clone(),
            });
        }
        None => {
            let _ = events.try_send(RelayLifecycleEvent::Registered {
                node_id: node_id.clone(),
            });
        }
    }

    let mut retire_rx = registered.retire_rx;
    loop {
        tokio::select! {
            _ = retire_rx.changed() => {
                // Replaced or evicted: the table entry is already gone or
                // owned by a successor; never remove someone else's entry.
                table.remove_if_current(&node_id, registered.connection_id);
                break;
            }
            read = read_frame(&mut tls) => {
                match read {
                    Ok(Frame::Heartbeat) => table.touch(&node_id, registered.connection_id),
                    Ok(Frame::Unregister) => {
                        if table.remove_if_current(&node_id, registered.connection_id) {
                            let _ = events.try_send(RelayLifecycleEvent::Unregistered { node_id: node_id.clone() });
                        }
                        break;
                    }
                    Ok(other) => {
                        ERROR_LOG.log_error(format!(
                            "[relay] {peer_addr} ({node_id}): unexpected frame on relay link: {other:?}; stream routing lands with the routing slice"
                        ));
                        table.remove_if_current(&node_id, registered.connection_id);
                        break;
                    }
                    Err(error) => {
                        ERROR_LOG.log_error(format!(
                            "[relay] {peer_addr} ({node_id}): link closed: {error}"
                        ));
                        table.remove_if_current(&node_id, registered.connection_id);
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::time::timeout;

    const NO_DEADLOCK: Duration = Duration::from_secs(10);

    fn install_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-relay-server-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir should create");
        dir
    }

    struct TestNode {
        cert_der: Vec<u8>,
        key_der: Vec<u8>,
    }

    impl TestNode {
        fn generate() -> Self {
            let mut params = rcgen::CertificateParams::new(vec!["waitagent".to_string()]);
            params.alg = &rcgen::PKCS_ED25519;
            let cert = rcgen::Certificate::from_params(params).expect("cert should generate");
            Self {
                cert_der: cert.serialize_der().expect("cert should serialize"),
                key_der: cert.serialize_private_key_der(),
            }
        }

        fn fingerprint(&self) -> String {
            node_credentials::cert_fingerprint_from_der(&self.cert_der)
                .expect("fingerprint should compute")
        }
    }

    /// Test-only server verifier pinning the exact expected certificate DER.
    #[derive(Debug)]
    struct ExactCertVerifier {
        der: Vec<u8>,
    }

    impl rustls::client::danger::ServerCertVerifier for ExactCertVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            if end_entity.as_ref() == self.der.as_slice() {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::UnknownIssuer,
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

    fn server_cert_der(config: &RelayServeConfig) -> Vec<u8> {
        let pem = fs::read_to_string(&config.credentials.cert_path).expect("server cert readable");
        let mut reader = pem.as_bytes();
        let cert = rustls_pemfile::certs(&mut reader)
            .next()
            .expect("one cert")
            .expect("cert parses");
        cert.as_ref().to_vec()
    }

    async fn connect_client(
        addr: SocketAddr,
        client: Option<&TestNode>,
        server_der: &[u8],
    ) -> Result<(), String> {
        let verifier = Arc::new(ExactCertVerifier {
            der: server_der.to_vec(),
        });
        let builder = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier);
        let config = match client {
            Some(node) => {
                let cert = rustls::pki_types::CertificateDer::from(node.cert_der.clone());
                let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(node.key_der.clone()),
                );
                builder
                    .with_client_auth_cert(vec![cert], key)
                    .map_err(|error| error.to_string())?
            }
            None => builder.with_no_client_auth(),
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|error| error.to_string())?;
        let server_name = rustls::pki_types::ServerName::try_from("waitagent")
            .map_err(|error| error.to_string())?;
        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|error| error.to_string())?;
        // TLS 1.3 completes the client handshake when the server's Finished
        // is processed, but the relay evaluates the client certificate only
        // afterwards — a whitelist rejection reaches the client as a
        // post-handshake alert. Surface it here so accept and reject are
        // distinguishable; a held (accepted) connection stays silent.
        let mut buf = [0u8; 1];
        match tokio::time::timeout(Duration::from_millis(200), tls.read(&mut buf)).await {
            Ok(Err(error)) => Err(format!("post-handshake alert: {error}")),
            _ => Ok(()),
        }
    }

    struct RunningServer {
        server: RelayServerHandle,
        events: mpsc::Receiver<RelayLifecycleEvent>,
        config: RelayServeConfig,
    }

    async fn start_test_server_with(
        whitelist: &[String],
        lifecycle: RelayLifecycleConfig,
    ) -> RunningServer {
        install_provider();
        let dir = temp_dir("server");
        let whitelist_dir = dir.join("authorized_nodes");
        fs::create_dir_all(&whitelist_dir).expect("whitelist dir should create");
        for fingerprint in whitelist {
            fs::write(whitelist_dir.join(fingerprint), b"").expect("entry should write");
        }
        let config = RelayServeConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            authorized_nodes_dir: whitelist_dir,
            credentials: NodeCredentialPaths {
                key_path: dir.join("relay.key"),
                cert_path: dir.join("relay.crt"),
            },
            lifecycle,
        };
        let started = start(config.clone()).await.expect("server should start");
        RunningServer {
            server: started.server,
            events: started.events,
            config,
        }
    }

    async fn start_test_server(whitelist: &[String]) -> RunningServer {
        start_test_server_with(whitelist, RelayLifecycleConfig::default()).await
    }

    async fn active_connections_reaches(handle: &RelayServerHandle, expected: usize) {
        let deadline = std::time::Instant::now() + NO_DEADLOCK;
        while handle.active_connections() != expected {
            assert!(
                std::time::Instant::now() < deadline,
                "active_connections should reach {expected}, got {}",
                handle.active_connections()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn next_event(rx: &mut mpsc::Receiver<RelayLifecycleEvent>) -> RelayLifecycleEvent {
        timeout(NO_DEADLOCK, rx.recv())
            .await
            .expect("event should arrive within the deadline")
            .expect("event stream should stay open")
    }

    type ClientTls = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

    async fn open_node_link(
        addr: SocketAddr,
        client: &TestNode,
        server_der: &[u8],
    ) -> Result<ClientTls, String> {
        let verifier = Arc::new(ExactCertVerifier {
            der: server_der.to_vec(),
        });
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    client.cert_der.clone(),
                )],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(client.key_der.clone()),
                ),
            )
            .map_err(|error| error.to_string())?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|error| error.to_string())?;
        let server_name = rustls::pki_types::ServerName::try_from("waitagent")
            .map_err(|error| error.to_string())?;
        connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| e.to_string())
    }

    /// Reads until the link ends; a closed lifecycle link surfaces as EOF or
    /// an error, never as a hang.
    async fn expect_link_closed(tls: &mut ClientTls) {
        let mut buf = [0u8; 1];
        let outcome = timeout(NO_DEADLOCK, tls.read(&mut buf)).await;
        match outcome {
            Ok(Ok(0)) => {}
            Ok(Err(_)) => {}
            other => panic!("link should close, got {other:?}"),
        }
    }

    #[test]
    fn authorized_nodes_scan_lists_fingerprint_files() {
        let dir = temp_dir("scan");
        fs::write(dir.join("abcdef0123456789"), b"").unwrap();
        fs::write(dir.join("FEDCBA9876543210"), b"").unwrap();
        fs::create_dir(dir.join("not-a-file-entry")).unwrap();

        let mut found = authorized_node_fingerprints(&dir).expect("scan should succeed");
        found.sort();
        assert_eq!(found, vec!["abcdef0123456789", "fedcba9876543210"]);

        let empty = authorized_node_fingerprints(&dir.join("missing"))
            .expect("missing dir should scan empty");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn mtls_accepts_whitelisted_client() {
        let client = TestNode::generate();
        let server = start_test_server(&[client.fingerprint()]).await;
        let server_der = server_cert_der(&server.config);

        connect_client(server.server.local_addr(), Some(&client), &server_der)
            .await
            .expect("whitelisted client handshake should succeed");
        // The link is now awaiting Register; nothing is registered yet.
        active_connections_reaches(&server.server, 0).await;
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn mtls_rejects_unknown_fingerprint() {
        let client = TestNode::generate();
        let server = start_test_server(&[]).await;
        let server_der = server_cert_der(&server.config);

        let error = connect_client(server.server.local_addr(), Some(&client), &server_der)
            .await
            .expect_err("unknown fingerprint must fail the handshake");
        assert!(
            !error.is_empty(),
            "the client should observe a handshake failure, got: {error}"
        );
        active_connections_reaches(&server.server, 0).await;
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn mtls_rejects_missing_client_cert() {
        let server = start_test_server(&[]).await;
        let server_der = server_cert_der(&server.config);

        let error = connect_client(server.server.local_addr(), None, &server_der)
            .await
            .expect_err("offering no client certificate must fail the handshake");
        assert!(
            !error.is_empty(),
            "the client should observe a handshake failure, got: {error}"
        );
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn removed_fingerprint_fails_new_handshakes() {
        let client = TestNode::generate();
        let fingerprint = client.fingerprint();
        let server = start_test_server(&[fingerprint.clone()]).await;
        let server_der = server_cert_der(&server.config);

        let first_link = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("first handshake should succeed");

        // Revoke: the whitelist entry goes away, so the next handshake fails
        // at the transport layer while the earlier link stays open.
        fs::remove_file(server.config.authorized_nodes_dir.join(&fingerprint))
            .expect("revoke should remove the entry");
        let error = connect_client(server.server.local_addr(), Some(&client), &server_der)
            .await
            .expect_err("revoked fingerprint must fail new handshakes");
        assert!(!error.is_empty());
        drop(first_link);
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_terminates_listener_and_connections() {
        let client = TestNode::generate();
        let mut server = start_test_server(&[client.fingerprint()]).await;
        let server_der = server_cert_der(&server.config);

        let mut link = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("handshake should succeed");
        crate::infra::relay_mux::frame::write_frame(
            &mut link,
            &Frame::Register {
                node_id: client.fingerprint(),
            },
        )
        .await
        .expect("register should write");
        assert!(matches!(
            next_event(&mut server.events).await,
            RelayLifecycleEvent::Registered { .. }
        ));
        active_connections_reaches(&server.server, 1).await;

        let addr = server.server.local_addr();
        server.server.shutdown().await;

        // The listener port must be closed after shutdown; link tasks exit
        // on the shutdown watch and drop with their tasks.
        let connect = timeout(NO_DEADLOCK, tokio::net::TcpStream::connect(addr)).await;
        assert!(
            matches!(connect, Ok(Err(_))),
            "new connections must fail after shutdown, got: {connect:?}"
        );
    }

    #[tokio::test]
    async fn register_then_heartbeat_silence_evicts() {
        let client = TestNode::generate();
        let lifecycle = RelayLifecycleConfig::fast_for_tests();
        let offline = lifecycle.offline_after;
        let mut server = start_test_server_with(&[client.fingerprint()], lifecycle).await;
        let server_der = server_cert_der(&server.config);

        let mut link = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("handshake should succeed");
        crate::infra::relay_mux::frame::write_frame(
            &mut link,
            &Frame::Register {
                node_id: client.fingerprint(),
            },
        )
        .await
        .expect("register should write");
        let node_id = match next_event(&mut server.events).await {
            RelayLifecycleEvent::Registered { node_id } => node_id,
            other => panic!("expected Registered, got {other:?}"),
        };
        active_connections_reaches(&server.server, 1).await;

        // Heartbeats keep the entry alive past the offline deadline.
        for _ in 0..4 {
            tokio::time::sleep(offline / 2).await;
            crate::infra::relay_mux::frame::write_frame(&mut link, &Frame::Heartbeat)
                .await
                .expect("heartbeat should write");
        }
        assert_eq!(server.server.active_connections(), 1);

        // Silence beyond the offline deadline evicts the node and closes
        // the link.
        match next_event(&mut server.events).await {
            RelayLifecycleEvent::EvictedOffline { node_id: evicted } => {
                assert_eq!(evicted, node_id);
            }
            other => panic!("expected EvictedOffline, got {other:?}"),
        }
        active_connections_reaches(&server.server, 0).await;
        expect_link_closed(&mut link).await;
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn duplicate_register_replaces_stale_link() {
        let client = TestNode::generate();
        let mut server = start_test_server_with(
            &[client.fingerprint()],
            RelayLifecycleConfig::fast_for_tests(),
        )
        .await;
        let server_der = server_cert_der(&server.config);

        let mut stale = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("first handshake should succeed");
        crate::infra::relay_mux::frame::write_frame(
            &mut stale,
            &Frame::Register {
                node_id: client.fingerprint(),
            },
        )
        .await
        .expect("register should write");
        assert!(matches!(
            next_event(&mut server.events).await,
            RelayLifecycleEvent::Registered { .. }
        ));

        let mut fresh = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("second handshake should succeed");
        crate::infra::relay_mux::frame::write_frame(
            &mut fresh,
            &Frame::Register {
                node_id: client.fingerprint(),
            },
        )
        .await
        .expect("re-register should write");
        assert!(matches!(
            next_event(&mut server.events).await,
            RelayLifecycleEvent::Replaced { .. }
        ));
        active_connections_reaches(&server.server, 1).await;

        // The stale link is retired and closed; the fresh link stays open.
        expect_link_closed(&mut stale).await;
        drop(fresh);
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn unregister_removes_entry_and_closes_link() {
        let client = TestNode::generate();
        let mut server = start_test_server_with(
            &[client.fingerprint()],
            RelayLifecycleConfig::fast_for_tests(),
        )
        .await;
        let server_der = server_cert_der(&server.config);

        let mut link = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("handshake should succeed");
        let register = Frame::Register {
            node_id: client.fingerprint(),
        };
        crate::infra::relay_mux::frame::write_frame(&mut link, &register)
            .await
            .expect("register should write");
        assert!(matches!(
            next_event(&mut server.events).await,
            RelayLifecycleEvent::Registered { .. }
        ));
        active_connections_reaches(&server.server, 1).await;

        crate::infra::relay_mux::frame::write_frame(&mut link, &Frame::Unregister)
            .await
            .expect("unregister should write");
        assert!(matches!(
            next_event(&mut server.events).await,
            RelayLifecycleEvent::Unregistered { .. }
        ));
        active_connections_reaches(&server.server, 0).await;
        expect_link_closed(&mut link).await;
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn register_with_mismatched_node_id_closes_link() {
        let client = TestNode::generate();
        let server = start_test_server_with(
            &[client.fingerprint()],
            RelayLifecycleConfig::fast_for_tests(),
        )
        .await;
        let server_der = server_cert_der(&server.config);

        let mut link = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("handshake should succeed");
        crate::infra::relay_mux::frame::write_frame(
            &mut link,
            &Frame::Register {
                node_id: "impostor".to_string(),
            },
        )
        .await
        .expect("register should write");
        expect_link_closed(&mut link).await;
        active_connections_reaches(&server.server, 0).await;
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn first_frame_must_be_register() {
        let client = TestNode::generate();
        let server = start_test_server_with(
            &[client.fingerprint()],
            RelayLifecycleConfig::fast_for_tests(),
        )
        .await;
        let server_der = server_cert_der(&server.config);

        let mut link = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("handshake should succeed");
        crate::infra::relay_mux::frame::write_frame(&mut link, &Frame::Heartbeat)
            .await
            .expect("frame should write");
        expect_link_closed(&mut link).await;
        active_connections_reaches(&server.server, 0).await;
        server.server.shutdown().await;
    }

    #[tokio::test]
    async fn stream_frame_before_routing_fails_link() {
        let client = TestNode::generate();
        let mut server = start_test_server_with(
            &[client.fingerprint()],
            RelayLifecycleConfig::fast_for_tests(),
        )
        .await;
        let server_der = server_cert_der(&server.config);

        let mut link = open_node_link(server.server.local_addr(), &client, &server_der)
            .await
            .expect("handshake should succeed");
        crate::infra::relay_mux::frame::write_frame(
            &mut link,
            &Frame::Register {
                node_id: client.fingerprint(),
            },
        )
        .await
        .expect("register should write");
        assert!(matches!(
            next_event(&mut server.events).await,
            RelayLifecycleEvent::Registered { .. }
        ));
        crate::infra::relay_mux::frame::write_frame(
            &mut link,
            &Frame::Data {
                stream_id: 1,
                payload: b"too early".to_vec(),
            },
        )
        .await
        .expect("frame should write");
        expect_link_closed(&mut link).await;
        active_connections_reaches(&server.server, 0).await;
        server.server.shutdown().await;
    }
}
