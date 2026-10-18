//! `waitagent relay join <address> <token>`: the node side of the
//! enrollment session (docs/relay-design.md 身份认证与入网). Opens a TLS
//! connection to the relay's enrollment listener (`address port + 1`),
//! authenticates with the node's own certificate (the whitelist check is
//! deliberately absent — the token is the authorization), sends
//! `Frame::Enroll`, and on success pins the delivered relay fingerprint
//! into `relay.toml` for every later connection.
//!
//! The server-cert verifier accepts ANY end-entity certificate: the relay
//! fingerprint is learned from the authenticated enrollment response rather
//! than pinned beforehand, so the only transport requirement is that the
//! relay proves possession of its private key (ring signature delegation).

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;

use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::peer_connection::dial_peer_connection;
use crate::infra::proxy_stream::SshProxy;
use crate::infra::relay_mux::frame::{read_frame, write_frame, Frame};
use crate::infra::relay_routing::error_code::RelayErrorCode;
use crate::infra::relay_server::{DEFAULT_RELAY_LISTEN_PORT, RELAY_ENROLL_PORT_OFFSET};
use crate::infra::relay_toml_store::{RelayTomlConfig, RelayTomlStoreError};

/// Upper bound on the join TLS handshake, matching the relay's own
/// `HANDSHAKE_TIMEOUT`.
const JOIN_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the enrollment response after the `Enroll` frame went out.
const ENROLL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Issue #206: how many times a transiently-failing join is attempted
/// before the error surfaces. The first failure is attempt 1 of 3, so the
/// backoffs below sit between attempts.
const JOIN_MAX_ATTEMPTS: usize = 3;

/// Backoff after the first and second failed join attempts (1s, 2s).
const JOIN_RETRY_BACKOFF: [Duration; JOIN_MAX_ATTEMPTS - 1] =
    [Duration::from_secs(1), Duration::from_secs(2)];

/// What `relay join` established: the pinned relay identity and where it
/// was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinOutcome {
    /// Lowercase hex SHA-256 SPKI fingerprint of the relay certificate,
    /// learned from the authenticated enrollment response.
    pub relay_fingerprint: String,
    /// The relay.toml path the pin was written to.
    pub toml_path: std::path::PathBuf,
}

/// Errors of the join flow. Token rejections are typed so the CLI can print
/// actionable guidance (mint a fresh invite).
#[derive(Debug, Error)]
pub enum RelayJoinError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("node credentials error: {0}")]
    Credentials(#[from] node_credentials::NodeCredentialsError),
    #[error("tls error: {0}")]
    Tls(String),
    #[error("invalid relay address {0:?}: {1}")]
    Address(String, String),
    #[error("enrollment token was rejected (unknown or already used); ask the operator for a fresh invite")]
    TokenInvalid,
    #[error("enrollment token has expired; ask the operator for a fresh invite")]
    TokenExpired,
    #[error("relay enrollment protocol error: {0}")]
    Protocol(String),
    #[error("{0} timed out")]
    Timeout(&'static str),
    #[error("relay.toml store error: {0}")]
    Toml(#[from] RelayTomlStoreError),
    /// Issue #209: the main port (ALPN `waitagent-enroll`, per
    /// [`ALPN_PROTOCOL_ENROLL`]) failed its TLS handshake and the legacy
    /// enroll port (`+ 1`) attempt failed as well. The message names both
    /// paths so an operator of an old relay sees the whole negotiation.
    #[error(
        "enrollment failed on every path: main port (ALPN waitagent-enroll): {main}; legacy enrollment port (+1): {legacy}"
    )]
    BothPathsFailed {
        main: Box<RelayJoinError>,
        legacy: Box<RelayJoinError>,
    },
}

impl RelayJoinError {
    /// Issue #206: whether the join is worth another attempt. Dial/TLS/
    /// enrollment IO and timeout failures are transient (a proxy flap, a
    /// relay restart); token, address, protocol, credential, and store
    /// errors fail fast — retrying cannot change their outcome. A
    /// both-paths failure (issue #209) is worth another round exactly when
    /// the legacy attempt was transient — the main-port failure always is
    /// (protocol errors never reach the fallback).
    fn is_retryable(&self) -> bool {
        match self {
            Self::BothPathsFailed { legacy, .. } => legacy.is_retryable(),
            _ => matches!(self, Self::Io(_) | Self::Timeout(_) | Self::Tls(_)),
        }
    }
}

/// Issue #209: how one enrollment attempt ended, so the dual-path
/// negotiation knows whether the legacy `port + 1` attempt is worth making.
enum EnrollAttemptError {
    /// TCP/proxy dial failed before TLS began. Retryable (issue #206) but
    /// NOT a legacy fallback trigger: the relay is unreachable, and `+ 1`
    /// cannot be up when the main port refused the connection.
    Dial(RelayJoinError),
    /// The TLS handshake itself failed (refused, reset, timed out) — the
    /// signature of a pre-#209 relay, whose main port only accepts
    /// whitelisted link clients. The caller falls back to `port + 1`.
    Handshake(RelayJoinError),
    /// TLS succeeded and the Enroll protocol answered (or the connection
    /// died mid-session). The relay speaks the new protocol; the error is
    /// reported verbatim and never falls back.
    Protocol(RelayJoinError),
}

impl EnrollAttemptError {
    fn into_error(self) -> RelayJoinError {
        match self {
            Self::Dial(error) | Self::Handshake(error) | Self::Protocol(error) => error,
        }
    }
}

/// Enrolls this node at `address` (default port
/// [`DEFAULT_RELAY_LISTEN_PORT`]) using `token`, then writes the pinned
/// relay identity to `toml_path`. When `proxy` is given, every enrollment
/// dial goes through it (issue #206) and the written pin records
/// `use_proxy: true` so the persistent link reuses the proxy on every
/// reconnect.
///
/// Issue #209 negotiation: the first attempt offers the
/// `waitagent-enroll` ALPN on the main port (single-port relays demux it
/// to the enrollment session); only a TLS-handshake-class failure falls
/// back to the legacy `port + 1` enroll listener (old relays). Protocol
/// errors after a successful handshake are reported without a fallback.
///
/// A single transient dial/TLS/enrollment failure is retried (up to
/// [`JOIN_MAX_ATTEMPTS`] attempts, [`JOIN_RETRY_BACKOFF`] between them);
/// refusal-class errors surface immediately. Each retry re-walks the whole
/// negotiation.
pub async fn join_relay(
    address: &str,
    token: &str,
    credentials: &NodeCredentialPaths,
    toml_path: &Path,
    proxy: Option<SshProxy>,
) -> Result<JoinOutcome, RelayJoinError> {
    for attempt in 1..=JOIN_MAX_ATTEMPTS {
        match join_relay_once(address, token, credentials, toml_path, proxy.clone()).await {
            Ok(outcome) => return Ok(outcome),
            Err(error) if attempt < JOIN_MAX_ATTEMPTS && error.is_retryable() => {
                tokio::time::sleep(JOIN_RETRY_BACKOFF[attempt - 1]).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the loop always returns on the final attempt");
}

/// One enrollment round: main port with the enroll ALPN, then — on a
/// TLS-class failure only — the legacy `port + 1` listener. The retry
/// policy lives in [`join_relay`].
async fn join_relay_once(
    address: &str,
    token: &str,
    credentials: &NodeCredentialPaths,
    toml_path: &Path,
    proxy: Option<SshProxy>,
) -> Result<JoinOutcome, RelayJoinError> {
    let (host, port) = parse_relay_address(address)?;
    let (config, server_name) = build_enroll_connector(credentials)?;

    let main = EnrollAttempt {
        config: config.clone(),
        server_name: server_name.clone(),
        host: &host,
        port,
        proxy: proxy.as_ref(),
        enroll_alpn: true,
        token,
        toml_path,
    };
    match enroll_single_attempt(&main).await {
        Ok(outcome) => Ok(outcome),
        Err(EnrollAttemptError::Protocol(error)) | Err(EnrollAttemptError::Dial(error)) => {
            Err(error)
        }
        Err(EnrollAttemptError::Handshake(main_error)) => {
            crate::infra::error_log::ERROR_LOG.log_debug(format!(
                "[relay-join] main port (ALPN {}) TLS failed ({main_error}); trying the legacy enrollment port",
                crate::infra::relay_server::ALPN_PROTOCOL_ENROLL,
            ));
            let legacy = EnrollAttempt {
                config,
                server_name,
                host: &host,
                port: port + RELAY_ENROLL_PORT_OFFSET,
                proxy: proxy.as_ref(),
                enroll_alpn: false,
                token,
                toml_path,
            };
            match enroll_single_attempt(&legacy).await {
                Ok(outcome) => Ok(outcome),
                Err(fallback) => Err(RelayJoinError::BothPathsFailed {
                    main: Box::new(main_error),
                    legacy: Box::new(fallback.into_error()),
                }),
            }
        }
    }
}

/// One enrollment attempt's inputs (issue #209 dual-path negotiation).
struct EnrollAttempt<'a> {
    config: rustls::ClientConfig,
    server_name: rustls::pki_types::ServerName<'static>,
    host: &'a str,
    port: u16,
    proxy: Option<&'a SshProxy>,
    enroll_alpn: bool,
    token: &'a str,
    toml_path: &'a Path,
}

/// Loads the enrollment identity and builds the (ALPN-free) TLS client
/// config both attempts share; the enroll ALPN is pinned per attempt.
fn build_enroll_connector(
    credentials: &NodeCredentialPaths,
) -> Result<(rustls::ClientConfig, rustls::pki_types::ServerName<'static>), RelayJoinError> {
    node_credentials::ensure_credentials(credentials)?;
    let cert_pem = fs::read_to_string(&credentials.cert_path)?;
    let key_pem = fs::read_to_string(&credentials.key_path)?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(|error| RelayJoinError::Tls(error.to_string()))?;
    if certs.is_empty() {
        return Err(RelayJoinError::Credentials(
            node_credentials::NodeCredentialsError::MissingEndEntityCertificate,
        ));
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|error| RelayJoinError::Tls(error.to_string()))?
        .ok_or_else(|| {
            RelayJoinError::Tls(format!("no private key in {:?}", credentials.key_path))
        })?;

    let verifier = Arc::new(AnyServerCertVerifier);
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
        .map_err(|error| RelayJoinError::Tls(error.to_string()))?;
    let server_name = rustls::pki_types::ServerName::try_from("waitagent")
        .map_err(|error| RelayJoinError::Tls(error.to_string()))?;
    Ok((config, server_name))
}

/// One dial → TLS → Enroll → response sequence against `attempt.host:
/// attempt.port`. `attempt.enroll_alpn` selects the single-port attempt
/// (ALPN `waitagent-enroll`) vs. the legacy one (no ALPN, as every
/// pre-#209 join).
async fn enroll_single_attempt(
    attempt: &EnrollAttempt<'_>,
) -> Result<JoinOutcome, EnrollAttemptError> {
    let tcp = dial_peer_connection(attempt.proxy, attempt.host, attempt.port)
        .await
        .map_err(|error| EnrollAttemptError::Dial(error.into()))?;
    let mut config = attempt.config.clone();
    if attempt.enroll_alpn {
        config.alpn_protocols = vec![crate::infra::relay_server::ALPN_PROTOCOL_ENROLL
            .as_bytes()
            .to_vec()];
    }
    let connector = TlsConnector::from(Arc::new(config));
    let mut tls = match tokio::time::timeout(
        JOIN_HANDSHAKE_TIMEOUT,
        connector.connect(attempt.server_name.clone(), tcp),
    )
    .await
    {
        Err(_) => {
            return Err(EnrollAttemptError::Handshake(RelayJoinError::Timeout(
                "relay enrollment handshake",
            )));
        }
        Ok(Err(error)) => {
            return Err(EnrollAttemptError::Handshake(RelayJoinError::Tls(
                error.to_string(),
            )));
        }
        Ok(Ok(tls)) => tls,
    };
    run_enroll_session(
        &mut tls,
        attempt.token,
        attempt.toml_path,
        &format!("{}:{}", attempt.host, attempt.port),
        attempt.proxy.is_some(),
    )
    .await
    .map_err(EnrollAttemptError::Protocol)
}

/// The Enroll frame exchange and pin write, shared by both negotiation
/// attempts. `pin_address` is the operator-facing `host:port` the link
/// dials afterwards (the base port, no enrollment offset); `via_proxy`
/// lands in the written pin (issue #206).
async fn run_enroll_session<S>(
    tls: &mut S,
    token: &str,
    toml_path: &Path,
    pin_address: &str,
    via_proxy: bool,
) -> Result<JoinOutcome, RelayJoinError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    write_frame(
        tls,
        &Frame::Enroll {
            token: token.to_string(),
        },
    )
    .await
    .map_err(io::Error::from)?;
    let response = match tokio::time::timeout(ENROLL_RESPONSE_TIMEOUT, read_frame(tls)).await {
        Err(_) => return Err(RelayJoinError::Timeout("enrollment response")),
        Ok(Err(error)) => return Err(io::Error::from(error).into()),
        Ok(Ok(frame)) => frame,
    };
    match response {
        Frame::EnrollResponse { fingerprint } => {
            let relay_fingerprint = fingerprint.to_lowercase();
            let config = RelayTomlConfig {
                address: pin_address.to_string(),
                relay_fingerprint: relay_fingerprint.clone(),
                // `relay join` writes only the enrollment result; the
                // heartbeat cadence stays at the default until an operator
                // adds it to relay.toml by hand.
                // Issue #206: remember that this enrollment dialed through
                // the active proxy profile, so the persistent link reuses
                // the proxy on every reconnect instead of re-resolving it.
                use_proxy: via_proxy,
                ..RelayTomlConfig::default()
            };
            config.save(toml_path)?;
            Ok(JoinOutcome {
                relay_fingerprint,
                toml_path: toml_path.to_path_buf(),
            })
        }
        Frame::Error { code, message, .. } => match RelayErrorCode::from_wire(code) {
            Some(RelayErrorCode::TokenInvalid) => Err(RelayJoinError::TokenInvalid),
            Some(RelayErrorCode::TokenExpired) => Err(RelayJoinError::TokenExpired),
            _ => Err(RelayJoinError::Protocol(format!(
                "relay rejected enrollment with code 0x{code:04x}: {message}"
            ))),
        },
        other => Err(RelayJoinError::Protocol(format!(
            "unexpected enrollment response: {other:?}"
        ))),
    }
}

/// Splits `host[:port]`; a missing port defaults to
/// [`DEFAULT_RELAY_LISTEN_PORT`]. Shared by the join flow and the relay.toml
/// store's `address` validation.
pub(crate) fn parse_relay_address(address: &str) -> Result<(String, u16), RelayJoinError> {
    let address = address.trim();
    if address.is_empty() {
        return Err(RelayJoinError::Address(
            address.to_string(),
            "empty address".to_string(),
        ));
    }
    match address.rsplit_once(':') {
        Some((host, port)) => {
            let host = host.trim();
            if host.is_empty() {
                return Err(RelayJoinError::Address(
                    address.to_string(),
                    "missing host".to_string(),
                ));
            }
            let port = port.trim().parse::<u16>().map_err(|_| {
                RelayJoinError::Address(address.to_string(), "port is not a number".to_string())
            })?;
            Ok((host.to_string(), port))
        }
        None => Ok((address.to_string(), DEFAULT_RELAY_LISTEN_PORT)),
    }
}

/// Builds a client TLS config trusting ANY server certificate. The Enroll
/// session uses this philosophy (the relay fingerprint is learned from the
/// authenticated response, not pinned beforehand); issue #208's self-service
/// enrollment HTTPS legs (`/auth/join-request`, `/auth/enroll`) reuse it for
/// the same reason — a not-yet-enrolled node has nothing to pin against.
/// `alpn_protocols` selects the application layer (`http/1.1` for the web
/// demux route).
pub fn any_server_tls_config(alpn_protocols: Vec<Vec<u8>>) -> rustls::ClientConfig {
    let verifier = Arc::new(AnyServerCertVerifier);
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = alpn_protocols;
    config
}

/// Issue #208: one HTTPS POST against the relay's dashboard surface (the
/// web ALPN route from issue #209's single-port demux), dialing through
/// `proxy` when one is given — the local→relay legs follow the same active
/// proxy profile as the enrollment itself. The request is HTTP/1.1 written
/// directly on the TLS stream (the response is small and hop-by-hop); a
/// non-2xx answer surfaces the server's body so the relay's actionable
/// errors (e.g. "use `relay invite`") reach the operator verbatim.
///
/// Returns `(status, body)`.
pub async fn post_enroll_api(
    relay_address: &str,
    proxy: Option<&SshProxy>,
    path: &str,
    body: &str,
) -> Result<(u16, String), RelayJoinError> {
    let (host, port) = parse_relay_address(relay_address)?;
    let tcp = dial_peer_connection(proxy, &host, port).await?;
    let connector = TlsConnector::from(Arc::new(any_server_tls_config(vec![b"http/1.1".to_vec()])));
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|error| RelayJoinError::Tls(error.to_string()))?;
    let mut tls = tokio::time::timeout(JOIN_HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| RelayJoinError::Timeout("relay dashboard handshake"))??;
    let request = format!(
        "POST {path} HTTP/1.1\r\nhost: {host}:{port}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    );
    tls.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, response_body) = text.split_once("\r\n\r\n").ok_or_else(|| {
        RelayJoinError::Protocol(format!("relay answered {path} without a header block"))
    })?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            RelayJoinError::Protocol(format!("relay answered {path} without a status code"))
        })?;
    Ok((status, response_body.to_string()))
}

/// rustls server-cert verifier for the enrollment session: any end-entity
/// certificate is accepted (the fingerprint is learned from the
/// authenticated response afterwards), while TLS 1.2/1.3 signature checks
/// are delegated to ring so the relay must prove key possession.
#[derive(Debug)]
struct AnyServerCertVerifier;

impl rustls::client::danger::ServerCertVerifier for AnyServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // The certificate must at least carry a usable identity; the pin
        // comes from the EnrollResponse, not from here.
        node_credentials::cert_fingerprint_from_der(end_entity.as_ref()).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        Ok(rustls::client::danger::ServerCertVerified::assertion())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_without_port_defaults_to_the_relay_port() {
        let (host, port) = parse_relay_address("relay.example").expect("parse");
        assert_eq!(host, "relay.example");
        assert_eq!(port, DEFAULT_RELAY_LISTEN_PORT);
    }

    #[test]
    fn address_with_explicit_port_wins() {
        let (host, port) = parse_relay_address("relay.example:9999").expect("parse");
        assert_eq!(host, "relay.example");
        assert_eq!(port, 9999);
    }

    #[test]
    fn address_rejects_garbage() {
        assert!(matches!(
            parse_relay_address(""),
            Err(RelayJoinError::Address(..))
        ));
        assert!(matches!(
            parse_relay_address(":9999"),
            Err(RelayJoinError::Address(..))
        ));
        assert!(matches!(
            parse_relay_address("host:notaport"),
            Err(RelayJoinError::Address(..))
        ));
    }

    #[test]
    fn retryable_classifier_only_covers_transient_failures() {
        let io_error = RelayJoinError::Io(io::Error::from(io::ErrorKind::ConnectionRefused));
        assert!(io_error.is_retryable());
        assert!(RelayJoinError::Timeout("enrollment response").is_retryable());
        assert!(RelayJoinError::Tls("handshake reset".to_string()).is_retryable());

        let credentials_error = RelayJoinError::Credentials(
            node_credentials::NodeCredentialsError::MissingEndEntityCertificate,
        );
        assert!(!credentials_error.is_retryable());
        assert!(!RelayJoinError::Address("x".to_string(), "bad".to_string()).is_retryable());
        assert!(!RelayJoinError::TokenInvalid.is_retryable());
        assert!(!RelayJoinError::TokenExpired.is_retryable());
        assert!(!RelayJoinError::Protocol("code".to_string()).is_retryable());

        let path =
            std::env::temp_dir().join(format!("waitagent-join-toml-err-{}", std::process::id()));
        std::fs::write(&path, "surprise = 1\n").expect("write should succeed");
        let store_error = RelayTomlConfig::load(&path).expect_err("unknown key should fail");
        std::fs::remove_file(&path).ok();
        assert!(!RelayJoinError::Toml(store_error).is_retryable());

        // Issue #209: a both-paths failure retries exactly when the legacy
        // attempt was transient (the main-port half always is — protocol
        // errors never reach the fallback).
        let transient = RelayJoinError::BothPathsFailed {
            main: Box::new(RelayJoinError::Tls("whitelist rejected".to_string())),
            legacy: Box::new(RelayJoinError::Io(io::Error::from(
                io::ErrorKind::ConnectionRefused,
            ))),
        };
        assert!(transient.is_retryable());
        let refused_token = RelayJoinError::BothPathsFailed {
            main: Box::new(RelayJoinError::Tls("whitelist rejected".to_string())),
            legacy: Box::new(RelayJoinError::TokenInvalid),
        };
        assert!(!refused_token.is_retryable());
    }

    #[tokio::test]
    async fn join_retries_a_refused_dial_then_surfaces_the_io_error() {
        // A closed loopback port fails the dial with a retryable
        // ConnectionRefused; the loop must burn both backoffs (1s + 2s)
        // across three attempts before surfacing the error.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir =
            std::env::temp_dir().join(format!("waitagent-join-retry-creds-{}", std::process::id()));
        let credentials = NodeCredentialPaths {
            key_path: dir.join("node.key"),
            cert_path: dir.join("node.crt"),
        };
        let started = std::time::Instant::now();
        let error = join_relay(
            "127.0.0.1:1",
            "token",
            &credentials,
            &dir.join("relay.toml"),
            None,
        )
        .await
        .expect_err("dialing a closed port must fail");
        assert!(
            matches!(error, RelayJoinError::Io(_)),
            "the surfaced error is the dial failure: {error:?}"
        );
        assert!(
            started.elapsed() >= JOIN_RETRY_BACKOFF[0] + JOIN_RETRY_BACKOFF[1],
            "both retry backoffs must elapse, got {:?}",
            started.elapsed()
        );
        crate::infra::best_effort::remove_dir_all(&dir);
    }
}
