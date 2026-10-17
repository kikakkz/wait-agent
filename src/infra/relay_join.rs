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
}

impl RelayJoinError {
    /// Issue #206: whether the join is worth another attempt. Dial/TLS/
    /// enrollment IO and timeout failures are transient (a proxy flap, a
    /// relay restart); token, address, protocol, credential, and store
    /// errors fail fast — retrying cannot change their outcome.
    fn is_retryable(&self) -> bool {
        matches!(self, Self::Io(_) | Self::Timeout(_) | Self::Tls(_))
    }
}

/// Enrolls this node at `address` (default port
/// [`DEFAULT_RELAY_LISTEN_PORT`], enrollment at `+ 1`) using `token`, then
/// writes the pinned relay identity to `toml_path`. When `proxy` is given,
/// the enrollment dial goes through it (issue #206) and the written pin
/// records `use_proxy: true` so the persistent link reuses the proxy on
/// every reconnect.
///
/// A single transient dial/TLS/enrollment failure is retried (up to
/// [`JOIN_MAX_ATTEMPTS`] attempts, [`JOIN_RETRY_BACKOFF`] between them);
/// refusal-class errors surface immediately.
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

/// One enrollment attempt; the retry policy lives in [`join_relay`].
async fn join_relay_once(
    address: &str,
    token: &str,
    credentials: &NodeCredentialPaths,
    toml_path: &Path,
    proxy: Option<SshProxy>,
) -> Result<JoinOutcome, RelayJoinError> {
    let (host, port) = parse_relay_address(address)?;
    let enroll_port = port + RELAY_ENROLL_PORT_OFFSET;

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
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from("waitagent")
        .map_err(|error| RelayJoinError::Tls(error.to_string()))?;

    let tcp = dial_peer_connection(proxy.as_ref(), &host, enroll_port).await?;
    let mut tls =
        match tokio::time::timeout(JOIN_HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp))
            .await
        {
            Err(_) => return Err(RelayJoinError::Timeout("relay enrollment handshake")),
            Ok(Err(error)) => return Err(RelayJoinError::Tls(error.to_string())),
            Ok(Ok(tls)) => tls,
        };

    write_frame(
        &mut tls,
        &Frame::Enroll {
            token: token.to_string(),
        },
    )
    .await
    .map_err(io::Error::from)?;
    let response = match tokio::time::timeout(ENROLL_RESPONSE_TIMEOUT, read_frame(&mut tls)).await {
        Err(_) => return Err(RelayJoinError::Timeout("enrollment response")),
        Ok(Err(error)) => return Err(io::Error::from(error).into()),
        Ok(Ok(frame)) => frame,
    };
    match response {
        Frame::EnrollResponse { fingerprint } => {
            let relay_fingerprint = fingerprint.to_lowercase();
            let config = RelayTomlConfig {
                address: format!("{host}:{port}"),
                relay_fingerprint: relay_fingerprint.clone(),
                // `relay join` writes only the enrollment result; the
                // heartbeat cadence stays at the default until an operator
                // adds it to relay.toml by hand.
                // Issue #206: remember that this enrollment dialed through
                // the active proxy profile, so the persistent link reuses
                // the proxy on every reconnect instead of re-resolving it.
                use_proxy: proxy.is_some(),
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
