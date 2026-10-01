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
//! session or traffic data. Stream routing between registered nodes lives
//! in `relay_routing` (the leg table) and `relay_link` (the per-link
//! protocol loop).

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
use crate::infra::relay_admin::{relay_admin_addr, run_admin_listener};
use crate::infra::relay_connection_table::{
    RelayConnectionTable, RelayLifecycleConfig, RelayLifecycleEvent,
};
use crate::infra::relay_routing::RoutingTable;
use crate::platform::remote_ipc::{RemoteControlAddr, RemoteControlAsyncListener};

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
    #[error("admin socket {0} failed to bind: {1}")]
    AdminIo(String, io::Error),
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
    admin_addr: RemoteControlAddr,
    table: Arc<RelayConnectionTable>,
    shutdown_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl RelayServerHandle {
    /// Returns the address the listener actually bound to (port 0 resolves).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns the address of the local admin socket.
    pub fn admin_addr(&self) -> &RemoteControlAddr {
        &self.admin_addr
    }

    /// Returns the number of registered nodes in the connection table.
    pub fn active_connections(&self) -> usize {
        self.table.len()
    }

    /// Waits until the accept loop stops (admin `shutdown`, listener error,
    /// or a dropped shutdown sender) without triggering the shutdown itself.
    pub async fn wait_until_stopped(self) {
        let _ = self.task.await;
    }

    /// Stops the accept loop, the sweeper, and every link task.
    ///
    /// Dropping the handle without calling `shutdown` has the same effect:
    /// the watch senders drop, the loops observe `changed()` resolving, and
    /// the accept loop breaks out of its select.
    // Used by tests today; the admin `shutdown` command is the runtime path.
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

/// Starts the relay transport gate: binds the TLS listener and the local
/// admin socket, ensures the relay's own credentials, and spawns the accept
/// loop, the eviction sweeper, and the admin listener. Every authenticated
/// link runs the register/heartbeat lifecycle and owns its connection-table
/// entry until unregister, loss, replacement, or eviction.
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
    // The admin socket is the bootstrap/emergency channel: a relay that
    // cannot bind it must not start half-managed.
    let admin_addr = relay_admin_addr(local_addr);
    let admin_listener = RemoteControlAsyncListener::bind(&admin_addr)
        .await
        .map_err(|error| RelayServerError::AdminIo(admin_addr.to_arg_string(), error))?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let table = Arc::new(RelayConnectionTable::default());
    let routing = Arc::new(RoutingTable::default());
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
    let lifecycle = config.lifecycle.clone();

    tokio::spawn(run_admin_listener(
        admin_listener,
        admin_addr.clone(),
        table.clone(),
        local_addr,
        shutdown_tx.clone(),
    ));

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
    let task_routing = routing.clone();
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
                    let routing = task_routing.clone();
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
                        crate::infra::relay_link::run_link(
                            tls, peer_addr, table, routing, events, lifecycle,
                        )
                        .await;
                    });
                }
            }
        }
    });

    Ok(StartedRelay {
        server: RelayServerHandle {
            local_addr,
            admin_addr,
            table,
            shutdown_tx,
            task,
        },
        events: events_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorized_nodes_scan_lists_fingerprint_files() {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-relay-server-scan-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir should create");
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
}
