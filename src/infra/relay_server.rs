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
//! Issue #209 demuxes one public port by ALPN (docs/relay-design.md
//! 身份认证与入网 单端口 ALPN 分流): no ALPN keeps this link handler,
//! `waitagent-enroll` runs the token-authenticated enrollment session, and
//! `h2`/`http/1.1` serves the dashboard when `--web` published one. The
//! legacy enrollment listener on `port + 1` stays for a deprecation
//! window (`legacy_enroll_listener` / `--no-legacy-enroll`).
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
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_admin::{relay_admin_addr, run_admin_listener, RelayAdminContext};
use crate::infra::relay_capacity::{RelayCapacityConfig, SharedUsageMeter, UsageMeter};
use crate::infra::relay_connection_table::{
    RelayConnectionTable, RelayLifecycleConfig, RelayLifecycleEvent,
};
use crate::infra::relay_enrollment::{
    handle_enrollment_frame, pem_encode_cert, EnrollmentTokenStore,
};
use crate::infra::relay_mux::frame::{read_frame, write_frame, Frame};
use crate::infra::relay_presence::PresenceHub;
use crate::infra::relay_routing::{error_code::RelayErrorCode, RoutingTable};
use crate::platform::remote_ipc::{RemoteControlAddr, RemoteControlAsyncListener};

// rustls re-exports `HandshakeSignatureValid` under `client::danger` for both
// verifier kinds; there is no `server::danger` re-export.
use rustls::client::danger::HandshakeSignatureValid;

/// Default relay listen port. Provisional until the relay.toml config slice;
/// the node side uses 7474.
pub const DEFAULT_RELAY_LISTEN_PORT: u16 = 7475;

/// The enrollment listener binds at `listen port + 1` (docs/relay-design.md
/// 身份认证与入网): the joining node derives the same address in
/// `relay_join`. Issue #209 demuxes enrollment onto the main port via ALPN;
/// the `+1` listener stays as an opt-out legacy window.
pub const RELAY_ENROLL_PORT_OFFSET: u16 = 1;

/// Issue #209: ALPN protocol id advertising the enrollment session on the
/// single relay port. The joining node offers it on the main port; relays
/// without the demux reject the handshake at the whitelist layer (the
/// joining cert is not registered yet) and the join falls back to the
/// legacy `port + 1` enroll listener. Single source for the server route
/// table and the client's join attempts.
pub const ALPN_PROTOCOL_ENROLL: &str = "waitagent-enroll";

/// ALPN ids routing to the dashboard on the single relay port (issue #209).
/// Browsers and curl offer these on every HTTPS request; anything else (or
/// nothing) keeps the historical link handler.
const ALPN_PROTOCOLS_WEB: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// Upper bound on a single mTLS handshake before the connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Issue #209: which handler a freshly accepted connection belongs to,
/// decided from the ALPN protocols in its ClientHello before the TLS
/// handshake is pinned to a `ServerConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlpnRoute {
    /// No/unknown ALPN: the node link protocol (back-compat; link clients
    /// never sent ALPN and still don't).
    Link,
    /// `waitagent-enroll`: the token-authenticated enrollment session.
    Enroll,
    /// `h2`/`http/1.1`: the dashboard web app (only when `--web` published
    /// one).
    Web,
}

/// Routes one connection by the ALPN protocols it offers. The first
/// recognized protocol wins; an empty or unrecognized offer is the link
/// handler so pre-#209 link clients keep connecting untouched.
fn route_alpn<'a>(protocols: impl IntoIterator<Item = &'a [u8]>) -> AlpnRoute {
    for protocol in protocols {
        if protocol == ALPN_PROTOCOL_ENROLL.as_bytes() {
            return AlpnRoute::Enroll;
        }
        if ALPN_PROTOCOLS_WEB.contains(&protocol) {
            return AlpnRoute::Web;
        }
    }
    AlpnRoute::Link
}

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

/// Enrollment token lifetimes for `relay invite` answers. Defaults follow
/// the store constants (short-lived one-time invites, long-lived deploy
/// tokens); `relay serve`'s relay.toml may override either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenTtlConfig {
    /// Default TTL for one-time invite tokens.
    pub invite: Duration,
    /// Default TTL for reusable deploy tokens.
    pub deploy: Duration,
}

impl Default for TokenTtlConfig {
    fn default() -> Self {
        Self {
            invite: crate::infra::relay_enrollment::DEFAULT_INVITE_TTL,
            deploy: crate::infra::relay_enrollment::DEFAULT_DEPLOY_TTL,
        }
    }
}

/// Request-extension marker set by the single-port TLS adapter (issue #209):
/// the connection arrived on the relay port's ALPN-demuxed web handler, so
/// the auth layer may mark session cookies `Secure`. The standalone HTTP
/// listener never sets it, keeping plain-`http` cookies unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedOverTls;

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
    /// Capacity knobs for admission control (容量评估与准入控制).
    pub capacity: RelayCapacityConfig,
    /// Enrollment token store persistence path (invite tokens survive
    /// restarts). Defaults to `waitagent_home()/relay-enroll-tokens.json`.
    pub tokens_path: PathBuf,
    /// Admin socket override (absolute UDS path; Unix only). `None` derives
    /// the socket from the listen address (port+20000 on Windows).
    pub admin_socket: Option<PathBuf>,
    /// Enrollment token TTLs answered by `relay invite`.
    pub token_ttls: TokenTtlConfig,
    /// Issue #209: bind the legacy enrollment listener at `port + 1`.
    /// Default `true` (deprecation window); `false` serves enrollment
    /// exclusively through the main port's ALPN demux.
    pub legacy_enroll_listener: bool,
    /// Issue #209: the dashboard app for the main port's web ALPN route,
    /// published by the `--web` half once its enrollment finished. Empty
    /// until then: web-ALPN connections arriving earlier are dropped with a
    /// debug log (the web enrollment needs this relay's admin socket, so
    /// the relay starts first). `OnceLock` — set once, read per connection,
    /// no lock on the accept path.
    pub web_app: Arc<std::sync::OnceLock<axum::Router>>,
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
            capacity: RelayCapacityConfig::default(),
            tokens_path: crate::infra::relay_enrollment::default_token_store_path(),
            admin_socket: None,
            token_ttls: TokenTtlConfig::default(),
            legacy_enroll_listener: true,
            web_app: Arc::new(std::sync::OnceLock::new()),
        }
    }
}

/// Resolves the admin socket address: the configured absolute UDS path when
/// set (Unix only — the Windows admin listener is the fixed loopback port
/// derived from `listen`), otherwise the listen-derived default.
fn admin_addr_for(
    configured: &Option<PathBuf>,
    listen: SocketAddr,
) -> Result<RemoteControlAddr, RelayServerError> {
    match configured {
        None => Ok(relay_admin_addr(listen)),
        Some(path) => {
            #[cfg(unix)]
            {
                Ok(RemoteControlAddr::Unix(path.clone()))
            }
            #[cfg(not(unix))]
            {
                Err(RelayServerError::AdminIo(
                    path.display().to_string(),
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "relay.toml `admin_socket` is only supported on Unix; on Windows the admin listener derives from --listen (listen port + 20000)",
                    ),
                ))
            }
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

/// rustls client-certificate verifier for the enrollment listener: requires
/// a client certificate and delegates the TLS 1.2/1.3 signature checks to
/// ring so the client must prove possession of the private key, but performs
/// NO whitelist check — the token inside the authenticated session is the
/// authorization (docs/relay-design.md 身份认证与入网: 无 token 者无法建立
/// 会话, and the whitelist write happens only after token redemption).
#[derive(Debug)]
struct AnyClientCertVerifier;

impl rustls::server::danger::ClientCertVerifier for AnyClientCertVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        // Any parseable identity is accepted at the transport layer; only
        // the fingerprint needs to be computable for the whitelist write.
        node_credentials::cert_fingerprint_from_der(end_entity.as_ref()).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        Ok(rustls::server::danger::ClientCertVerified::assertion())
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
    enroll_local_addr: Option<SocketAddr>,
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

    /// Returns the legacy enrollment listener address (`local_addr` port + 1),
    /// or `None` when the relay started with `legacy_enroll_listener = false`
    /// (issue #209: enrollment is served through the main port's ALPN demux
    /// only).
    pub fn enroll_local_addr(&self) -> Option<SocketAddr> {
        self.enroll_local_addr
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

    /// Returns a clone of the shutdown sender: sending `true` stops the
    /// accept loop, the sweeper, and every link task — the same path as the
    /// admin `shutdown` command — after which [`wait_until_stopped`](Self::wait_until_stopped)
    /// completes. Lets a co-runtime supervisor (e.g. the WebUI half of
    /// `relay serve --web`) bring the relay down without owning the handle.
    pub fn shutdown_sender(&self) -> watch::Sender<bool> {
        self.shutdown_tx.clone()
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

/// Starts the relay transport gate: binds the TLS listener (plus the
/// enrollment listener at `port + 1`) and the local admin socket, ensures
/// the relay's own credentials, and spawns the accept loop, the eviction
/// sweeper, and the admin listener. Every authenticated link runs the
/// register/heartbeat lifecycle and owns its connection-table entry until
/// unregister, loss, replacement, or eviction.
pub async fn start(config: RelayServeConfig) -> Result<StartedRelay, RelayServerError> {
    let started_at = std::time::Instant::now();
    let relay_fingerprint = node_credentials::ensure_credentials(&config.credentials)?;
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
    // Issue #209: one public port, three handlers demuxed by ALPN. The link
    // config pins no ALPN list so pre-#209 clients (which never sent ALPN)
    // keep connecting untouched; enroll and web configs pin their ALPN ids
    // so a connection can only negotiate the protocol it was routed to.
    let link_server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(WhitelistedClientCertVerifier::new(
            config.authorized_nodes_dir.clone(),
        )))
        .with_single_cert(certs.clone(), key.clone_key())?;
    let mut enroll_server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(AnyClientCertVerifier))
        .with_single_cert(certs.clone(), key.clone_key())?;
    enroll_server_config.alpn_protocols = vec![ALPN_PROTOCOL_ENROLL.as_bytes().to_vec()];
    let mut web_server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    web_server_config.alpn_protocols = ALPN_PROTOCOLS_WEB.iter().map(|id| id.to_vec()).collect();

    let (listener, enroll_listener) =
        bind_gate_pair(config.listen, config.legacy_enroll_listener).await?;
    let local_addr = listener.local_addr()?;
    let enroll_local_addr = enroll_listener
        .as_ref()
        .map(|listener| listener.local_addr())
        .transpose()?;
    // The admin socket is the bootstrap/emergency channel: a relay that
    // cannot bind it must not start half-managed.
    let admin_addr = admin_addr_for(&config.admin_socket, local_addr)?;
    let admin_listener = RemoteControlAsyncListener::bind(&admin_addr)
        .await
        .map_err(|error| RelayServerError::AdminIo(admin_addr.to_arg_string(), error))?;
    let link_config = Arc::new(link_server_config);
    let enroll_config = Arc::new(enroll_server_config);
    let web_config = Arc::new(web_server_config);
    let web_app = config.web_app.clone();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let table = Arc::new(RelayConnectionTable::default());
    let routing = Arc::new(RoutingTable::default());
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
    let lifecycle = config.lifecycle.clone();
    let capacity = config.capacity.clone();
    let meter: SharedUsageMeter = std::sync::Arc::new(UsageMeter::new());
    let tokens_path = config.tokens_path.clone();
    let tokens = Arc::new(
        EnrollmentTokenStore::load(&tokens_path).unwrap_or_else(|error| {
            ERROR_LOG.log_error(format!(
                "[relay] token store {} unreadable ({error}); starting empty",
                tokens_path.display()
            ));
            EnrollmentTokenStore::new()
        }),
    );
    let presence = Arc::new(PresenceHub::new());

    tokio::spawn(run_admin_listener(
        admin_listener,
        admin_addr.clone(),
        RelayAdminContext {
            table: table.clone(),
            routing: routing.clone(),
            listen: local_addr,
            capacity: capacity.clone(),
            meter: meter.clone(),
            tokens: tokens.clone(),
            tokens_path: tokens_path.clone(),
            whitelist_dir: config.authorized_nodes_dir.clone(),
            presence: presence.clone(),
            token_ttls: config.token_ttls,
        },
        shutdown_tx.clone(),
    ));

    let sweeper_table = table.clone();
    let sweeper_lifecycle = lifecycle.clone();
    let mut sweeper_shutdown = shutdown_tx.subscribe();
    let sweeper_events = events_tx.clone();
    let sweeper_presence = presence.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = sweeper_shutdown.changed() => return,
                _ = tokio::time::sleep(sweeper_lifecycle.sweep_interval) => {
                    for (node_id, entry) in sweeper_table.evict_idle(sweeper_lifecycle.offline_after) {
                        // Best-effort death notice before the teardown signal
                        // (issue #36): a full control queue must not block
                        // eviction, so the enqueue failure is ignored.
                        let _ = entry.outbound.try_send(Frame::Error {
                            stream_id: 0,
                            code: RelayErrorCode::HeartbeatLost.wire_value(),
                            message: RelayErrorCode::HeartbeatLost.message().to_string(),
                        });
                        let _ = entry.retire_tx.send(true);
                        let _ = sweeper_events.try_send(RelayLifecycleEvent::EvictedOffline { node_id: node_id.clone() });
                        sweeper_presence.publish(&node_id, false);
                    }
                }
            }
        }
    });

    let task_table = table.clone();
    let task_routing = routing.clone();
    let task_events = events_tx;
    let task_tokens = tokens.clone();
    let task_tokens_path = tokens_path.clone();
    let task_token_ttls = config.token_ttls;
    let task_whitelist_dir = config.authorized_nodes_dir.clone();
    let task_relay_fingerprint = relay_fingerprint.clone();
    let task_presence = presence.clone();

    // The main accept loop and the legacy enrollment loop each own clones;
    // neither outlives a `shutdown` signal.
    let main_link_config = link_config.clone();
    let main_enroll_config = enroll_config.clone();
    let main_web_config = web_config.clone();
    let main_web_app = web_app.clone();
    let legacy_tokens = task_tokens.clone();
    let legacy_whitelist_dir = task_whitelist_dir.clone();
    let legacy_relay_fingerprint = task_relay_fingerprint.clone();
    let legacy_register_timeout = lifecycle.register_timeout;
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((tcp, peer_addr)) = accepted else {
                        ERROR_LOG.log_error("[relay] listener accept failed; stopping".to_string());
                        break;
                    };
                    let link_config = main_link_config.clone();
                    let enroll_config = main_enroll_config.clone();
                    let web_config = main_web_config.clone();
                    let web_app = main_web_app.clone();
                    let table = task_table.clone();
                    let routing = task_routing.clone();
                    let events = task_events.clone();
                    let lifecycle = lifecycle.clone();
                    let capacity = capacity.clone();
                    let meter = meter.clone();
                    let presence = task_presence.clone();
                    let tokens = task_tokens.clone();
                    let tokens_path = task_tokens_path.clone();
                    let token_ttls = task_token_ttls;
                    let whitelist_dir = task_whitelist_dir.clone();
                    let relay_fingerprint = task_relay_fingerprint.clone();
                    tokio::spawn(async move {
                        // Peek the ClientHello, then pin the handshake to the
                        // matching ServerConfig (issue #209 single-port
                        // demux). Link clients send no ALPN and land on the
                        // link handler exactly as before.
                        let lazy = tokio_rustls::LazyConfigAcceptor::new(
                            rustls::server::Acceptor::default(),
                            tcp,
                        );
                        let accepted =
                            match tokio::time::timeout(HANDSHAKE_TIMEOUT, lazy).await {
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
                                Ok(Ok(accepted)) => accepted,
                            };
                        let route = match accepted.client_hello().alpn() {
                            Some(protocols) => route_alpn(protocols),
                            None => AlpnRoute::Link,
                        };
                        match route {
                            AlpnRoute::Link => {
                                let Some(tls) =
                                    finish_handshake(accepted, link_config, peer_addr, "relay")
                                        .await
                                else {
                                    return;
                                };
                                crate::infra::relay_link::run_link(
                                    tls, peer_addr, table, routing, events, lifecycle, capacity,
                                    meter, presence, local_addr, started_at, tokens, tokens_path,
                                    token_ttls, whitelist_dir,
                                )
                                .await;
                            }
                            AlpnRoute::Enroll => {
                                let Some(tls) = finish_handshake(
                                    accepted,
                                    enroll_config,
                                    peer_addr,
                                    "relay-enroll",
                                )
                                .await
                                else {
                                    return;
                                };
                                run_enrollment_connection(
                                    tls,
                                    peer_addr,
                                    tokens,
                                    whitelist_dir,
                                    relay_fingerprint,
                                    lifecycle.register_timeout,
                                )
                                .await;
                            }
                            AlpnRoute::Web => match web_app.get() {
                                Some(router) => {
                                    let Some(tls) = finish_handshake(
                                        accepted,
                                        web_config,
                                        peer_addr,
                                        "relay-web",
                                    )
                                    .await
                                    else {
                                        return;
                                    };
                                    serve_web_connection(tls, peer_addr, router.clone()).await;
                                }
                                None => {
                                    ERROR_LOG.log_debug(format!(
                                        "[relay-web] {peer_addr}: web ALPN but no web app is publishing yet; dropping"
                                    ));
                                }
                            },
                        }
                    });
                }
            }
        }
    });

    // Issue #209: the legacy enrollment listener (port + 1) runs as its own
    // loop, only when the deprecation window is open. Old relays' joins use
    // it; the main port's ALPN demux above serves enrollment regardless.
    if let Some(enroll_listener) = enroll_listener {
        let legacy_acceptor = TlsAcceptor::from(enroll_config);
        let mut legacy_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = legacy_shutdown.changed() => return,
                    accepted = enroll_listener.accept() => {
                        let Ok((tcp, peer_addr)) = accepted else {
                            ERROR_LOG.log_error(
                                "[relay] enrollment listener accept failed; stopping".to_string(),
                            );
                            return;
                        };
                        let acceptor = legacy_acceptor.clone();
                        let tokens = legacy_tokens.clone();
                        let whitelist_dir = legacy_whitelist_dir.clone();
                        let relay_fingerprint = legacy_relay_fingerprint.clone();
                        tokio::spawn(async move {
                            let handshake =
                                tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await;
                            let tls = match handshake {
                                Err(_) => {
                                    ERROR_LOG.log_error(format!(
                                        "[relay-enroll] {peer_addr}: handshake timed out"
                                    ));
                                    return;
                                }
                                Ok(Err(error)) => {
                                    ERROR_LOG.log_error(format!(
                                        "[relay-enroll] {peer_addr}: handshake rejected: {error}"
                                    ));
                                    return;
                                }
                                Ok(Ok(tls)) => tls,
                            };
                            run_enrollment_connection(
                                tls,
                                peer_addr,
                                tokens,
                                whitelist_dir,
                                relay_fingerprint,
                                legacy_register_timeout,
                            )
                            .await;
                        });
                    }
                }
            }
        });
    }

    Ok(StartedRelay {
        server: RelayServerHandle {
            local_addr,
            enroll_local_addr,
            admin_addr,
            table,
            shutdown_tx,
            task,
        },
        events: events_rx,
    })
}

/// Completes a peeked ClientHello against the route's `ServerConfig`,
/// logging refusals with the route's prefix. Returns the established TLS
/// stream or `None` when the handshake failed.
async fn finish_handshake(
    accepted: tokio_rustls::server::StartHandshake<TcpStream>,
    config: Arc<rustls::ServerConfig>,
    peer_addr: SocketAddr,
    log_prefix: &str,
) -> Option<tokio_rustls::server::TlsStream<TcpStream>> {
    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, accepted.into_stream(config)).await;
    match handshake {
        Err(_) => {
            ERROR_LOG.log_error(format!("[{log_prefix}] {peer_addr}: handshake timed out"));
            None
        }
        Ok(Err(error)) => {
            ERROR_LOG.log_error(format!(
                "[{log_prefix}] {peer_addr}: handshake rejected: {error}"
            ));
            None
        }
        Ok(Ok(tls)) => Some(tls),
    }
}

/// Serves one dashboard connection on the single-port TLS stream (issue
/// #209): bridges to the axum Router through hyper's auto HTTP/1+HTTP/2
/// builder, injecting the peer address (`ConnectInfo`) and the
/// [`ServedOverTls`] marker the auth layer reads to flag session cookies
/// `Secure`.
async fn serve_web_connection(
    tls: tokio_rustls::server::TlsStream<TcpStream>,
    peer_addr: SocketAddr,
    router: axum::Router,
) {
    use axum::extract::ConnectInfo;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::Service;

    let service = hyper_util::service::TowerToHyperService::new(tower::service_fn(
        move |mut request: axum::http::Request<hyper::body::Incoming>| {
            request.extensions_mut().insert(ConnectInfo(peer_addr));
            request.extensions_mut().insert(ServedOverTls);
            // The hyper boundary speaks the raw `Incoming` body; the axum
            // Router expects `axum::body::Body`. Extensions set above
            // survive `map`.
            let request = request.map(axum::body::Body::new);
            let mut router = router.clone();
            async move { router.call(request).await }
        },
    ));
    let result = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(tls), service)
        .await;
    if let Err(error) = result {
        ERROR_LOG.log_debug(format!(
            "[relay-web] {peer_addr}: connection ended: {error}"
        ));
    }
}

/// Binds the main TLS listener and the enrollment listener (`port + 1`).
///
/// With an explicit `listen` port a failed enrollment bind aborts the start
/// (the join side derives the address from the main port, so the pair must
/// exist). With port 0 (tests) the main bind resolves to an ephemeral port
/// whose `+1` may race a concurrent test, so the pair is re-rolled a few
/// times before giving up.
/// Binds the main TLS listener and, when `legacy_enroll` is set, the
/// enrollment listener at `port + 1` (issue #209's deprecation window;
/// `None` serves enrollment exclusively through the main port's ALPN
/// demux).
///
/// With an explicit `listen` port a failed enrollment bind aborts the start
/// (the join side derives the legacy address from the main port, so the
/// pair must exist while the window is open). With port 0 (tests) the main
/// bind resolves to an ephemeral port whose `+1` may race a concurrent
/// test, so the pair is re-rolled a few times before giving up.
async fn bind_gate_pair(
    listen: SocketAddr,
    legacy_enroll: bool,
) -> Result<(TcpListener, Option<TcpListener>), RelayServerError> {
    const EPHEMERAL_BIND_ATTEMPTS: usize = 8;
    if !legacy_enroll {
        return Ok((TcpListener::bind(listen).await?, None));
    }
    let mut attempts = 0usize;
    loop {
        let listener = TcpListener::bind(listen).await?;
        let local_addr = listener.local_addr()?;
        let enroll_addr = SocketAddr::new(
            local_addr.ip(),
            local_addr.port() + RELAY_ENROLL_PORT_OFFSET,
        );
        match TcpListener::bind(enroll_addr).await {
            Ok(enroll_listener) => return Ok((listener, Some(enroll_listener))),
            Err(error) => {
                drop(listener);
                attempts += 1;
                if listen.port() != 0 || attempts == EPHEMERAL_BIND_ATTEMPTS {
                    return Err(error.into());
                }
                ERROR_LOG.log_error(format!(
                    "[relay] enrollment bind at {enroll_addr} raced ({error}); retrying"
                ));
            }
        }
    }
}

/// Runs one enrollment session to completion: the TLS handshake already
/// happened (any client cert accepted — the whitelist is the token's job),
/// so read the single first frame under the register deadline, let
/// `handle_enrollment_frame` redeem the token / whitelist the peer / build
/// the answer, write it, and close.
async fn run_enrollment_connection(
    tls: crate::infra::relay_link::ServerTls,
    peer_addr: SocketAddr,
    tokens: Arc<EnrollmentTokenStore>,
    whitelist_dir: PathBuf,
    relay_fingerprint: String,
    register_timeout: Duration,
) {
    let Some(peer_fingerprint) = crate::infra::relay_link::peer_fingerprint(&tls, peer_addr) else {
        return;
    };
    let Some(peer_cert_pem) = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| pem_encode_cert(cert.as_ref()))
    else {
        // Mandatory client auth makes this an invariant; a missing peer
        // certificate means the rustls contract broke.
        ERROR_LOG.log_error(format!(
            "[relay-enroll] {peer_addr}: no peer certificate after mandatory client auth"
        ));
        return;
    };
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let (mut reader, mut writer) = tokio::io::split(tls);
    let first = tokio::time::timeout(register_timeout, read_frame(&mut reader)).await;
    let frame = match first {
        Err(_) => {
            ERROR_LOG.log_error(format!(
                "[relay-enroll] {peer_addr}: enrollment deadline exceeded"
            ));
            return;
        }
        Ok(Err(error)) => {
            ERROR_LOG.log_error(format!(
                "[relay-enroll] {peer_addr}: link closed before Enroll: {error}"
            ));
            return;
        }
        Ok(Ok(frame)) => frame,
    };
    let Some(response) = handle_enrollment_frame(
        frame,
        &peer_fingerprint,
        &peer_cert_pem,
        &tokens,
        &whitelist_dir,
        &relay_fingerprint,
        now_unix,
    ) else {
        return;
    };
    if let Err(error) = write_frame(&mut writer, &response).await {
        ERROR_LOG.log_error(format!(
            "[relay-enroll] {peer_addr}: enrollment response write failed: {error}"
        ));
    }
    // Graceful close: flush happened inside `write_frame`; dropping the
    // halves here sends close_notify and ends the session.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_routes_enroll_web_and_link() {
        // Issue #209 single-port route table.
        assert_eq!(
            route_alpn([ALPN_PROTOCOL_ENROLL.as_bytes()]),
            AlpnRoute::Enroll
        );
        assert_eq!(route_alpn([b"h2".as_slice()]), AlpnRoute::Web);
        assert_eq!(route_alpn([b"http/1.1".as_slice()]), AlpnRoute::Web);
        // Browsers offer h2 first; the web route wins over nothing else.
        assert_eq!(
            route_alpn([b"h2".as_slice(), b"http/1.1".as_slice()]),
            AlpnRoute::Web
        );
        // No ALPN (every pre-#209 link client) and unknown ids keep the link
        // handler — the compatibility anchor of the demux.
        assert_eq!(route_alpn(None::<&[u8]>), AlpnRoute::Link);
        assert_eq!(route_alpn([] as [&[u8]; 0]), AlpnRoute::Link);
        assert_eq!(route_alpn([b"mqtt".as_slice()]), AlpnRoute::Link);
        assert_eq!(
            route_alpn([b"unknown".as_slice(), ALPN_PROTOCOL_ENROLL.as_bytes()]),
            AlpnRoute::Enroll
        );
    }

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
