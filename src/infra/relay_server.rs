//! Relay transport gate: TLS listener requiring client certificates whose
//! SHA-256 SPKI fingerprint is whitelisted in an `authorized_nodes/`
//! directory.
//!
//! Governing design: docs/relay-design.md (身份认证与入网 / node 生命周期 /
//! 管理通道). Device identity is the self-signed certificate fingerprint
//! (no CA); the relay requests a client certificate during the mTLS
//! handshake, and a fingerprint that is not whitelisted fails the handshake
//! at the transport layer — the same layer where revoked nodes fail on
//! their next handshake after their whitelist entry is removed.
//!
//! This slice is the transport gate only: authenticated connections are held
//! open until shutdown; the register/heartbeat protocol loop that reads from
//! them lands with the connection-table slice.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::node_credentials::{self, NodeCredentialPaths};

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
}

impl RelayServeConfig {
    /// Returns the default configuration for `listen`: whitelist under
    /// `~/.waitagent/authorized_nodes/`, credentials at the default paths.
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            authorized_nodes_dir: crate::host::ssh::remote_host_home::waitagent_home()
                .join("authorized_nodes"),
            credentials: NodeCredentialPaths::default_paths(),
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
    active: Arc<AtomicUsize>,
    shutdown_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl RelayServerHandle {
    /// Returns the address the listener actually bound to (port 0 resolves).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns the number of authenticated connections currently held.
    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Stops the accept loop and closes all held connections.
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

/// Starts the relay transport gate: binds the listener, ensures the relay's
/// own credentials, and spawns the accept loop. Authenticated connections
/// are held until shutdown — the protocol loop lands with the
/// connection-table slice.
pub async fn start(config: RelayServeConfig) -> Result<RelayServerHandle, RelayServerError> {
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
    let active = Arc::new(AtomicUsize::new(0));
    let task_shutdown_tx = shutdown_tx.clone();
    let task_active = active.clone();

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
                    let active = task_active.clone();
                    let mut conn_shutdown = task_shutdown_tx.subscribe();
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
                        active.fetch_add(1, Ordering::SeqCst);
                        // Transport gate: hold the authenticated connection
                        // until shutdown. The register/heartbeat protocol
                        // loop replaces this hold in the connection-table
                        // slice.
                        let _tls = tls;
                        let _ = conn_shutdown.changed().await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }
        }
    });

    Ok(RelayServerHandle {
        local_addr,
        active,
        shutdown_tx,
        task,
    })
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
        handle: RelayServerHandle,
        config: RelayServeConfig,
    }

    async fn start_test_server(whitelist: &[String]) -> RunningServer {
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
        };
        let handle = start(config.clone()).await.expect("server should start");
        RunningServer { handle, config }
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

        connect_client(server.handle.local_addr(), Some(&client), &server_der)
            .await
            .expect("whitelisted client handshake should succeed");
        active_connections_reaches(&server.handle, 1).await;
        server.handle.shutdown().await;
    }

    #[tokio::test]
    async fn mtls_rejects_unknown_fingerprint() {
        let client = TestNode::generate();
        let server = start_test_server(&[]).await;
        let server_der = server_cert_der(&server.config);

        let error = connect_client(server.handle.local_addr(), Some(&client), &server_der)
            .await
            .expect_err("unknown fingerprint must fail the handshake");
        assert!(
            !error.is_empty(),
            "the client should observe a handshake failure, got: {error}"
        );
        active_connections_reaches(&server.handle, 0).await;
        server.handle.shutdown().await;
    }

    #[tokio::test]
    async fn mtls_rejects_missing_client_cert() {
        let server = start_test_server(&[]).await;
        let server_der = server_cert_der(&server.config);

        let error = connect_client(server.handle.local_addr(), None, &server_der)
            .await
            .expect_err("offering no client certificate must fail the handshake");
        assert!(
            !error.is_empty(),
            "the client should observe a handshake failure, got: {error}"
        );
        server.handle.shutdown().await;
    }

    #[tokio::test]
    async fn removed_fingerprint_fails_new_handshakes() {
        let client = TestNode::generate();
        let fingerprint = client.fingerprint();
        let server = start_test_server(&[fingerprint.clone()]).await;
        let server_der = server_cert_der(&server.config);

        connect_client(server.handle.local_addr(), Some(&client), &server_der)
            .await
            .expect("first handshake should succeed");
        active_connections_reaches(&server.handle, 1).await;

        // Revoke: the whitelist entry goes away, so the next handshake fails
        // at the transport layer while the already-authenticated connection
        // stays held.
        fs::remove_file(server.config.authorized_nodes_dir.join(&fingerprint))
            .expect("revoke should remove the entry");
        let error = connect_client(server.handle.local_addr(), Some(&client), &server_der)
            .await
            .expect_err("revoked fingerprint must fail new handshakes");
        assert!(!error.is_empty());
        active_connections_reaches(&server.handle, 1).await;
        server.handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_terminates_listener_and_held_connections() {
        let client = TestNode::generate();
        let server = start_test_server(&[client.fingerprint()]).await;
        let server_der = server_cert_der(&server.config);

        connect_client(server.handle.local_addr(), Some(&client), &server_der)
            .await
            .expect("handshake should succeed");
        active_connections_reaches(&server.handle, 1).await;

        let addr = server.handle.local_addr();
        server.handle.shutdown().await;

        // The listener port must be closed after shutdown; held connections
        // exit on the shutdown watch and drop with their tasks.
        let connect = timeout(NO_DEADLOCK, tokio::net::TcpStream::connect(addr)).await;
        assert!(
            matches!(connect, Ok(Err(_))),
            "new connections must fail after shutdown, got: {connect:?}"
        );
    }
}
