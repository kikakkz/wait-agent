//! End-to-end tests for the relay transport gate and connection lifecycle:
//! real TLS over loopback TCP, whitelisted mTLS handshakes, and the framed
//! register/heartbeat protocol. Stream-routing end-to-end tests live in the
//! [`routing`] child module.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_connection_table::{RelayLifecycleConfig, RelayLifecycleEvent};
use crate::infra::relay_mux::frame::{read_frame, write_frame, Frame};
use crate::infra::relay_routing::error_code;
use crate::infra::relay_server::{start, RelayServeConfig, RelayServerHandle};

mod routing;

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

pub(super) struct TestNode {
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

pub(super) fn server_cert_der(config: &RelayServeConfig) -> Vec<u8> {
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
    let server_name =
        rustls::pki_types::ServerName::try_from("waitagent").map_err(|error| error.to_string())?;
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

pub(super) struct RunningServer {
    server: RelayServerHandle,
    events: mpsc::Receiver<RelayLifecycleEvent>,
    config: RelayServeConfig,
}

pub(super) async fn start_test_server_with(
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

pub(super) async fn active_connections_reaches(handle: &RelayServerHandle, expected: usize) {
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

pub(super) type ClientTls = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

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
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                client.key_der.clone(),
            )),
        )
        .map_err(|error| error.to_string())?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|error| error.to_string())?;
    let server_name =
        rustls::pki_types::ServerName::try_from("waitagent").map_err(|error| error.to_string())?;
    connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| e.to_string())
}

/// Reads one frame from a node link, deadline-guarded so a routing bug
/// surfaces as a test failure instead of a hang.
pub(super) async fn read_link_frame(tls: &mut ClientTls) -> Frame {
    timeout(NO_DEADLOCK, read_frame(tls))
        .await
        .expect("frame within deadline")
        .expect("frame decodes")
}

/// Opens a link for `client`, registers it, and waits for the `Registered`
/// lifecycle event. Returns the registered link.
pub(super) async fn register_node(
    server: &mut RunningServer,
    client: &TestNode,
    server_der: &[u8],
) -> ClientTls {
    let mut link = open_node_link(server.server.local_addr(), client, server_der)
        .await
        .expect("handshake should succeed");
    write_frame(
        &mut link,
        &Frame::Register {
            node_id: client.fingerprint(),
        },
    )
    .await
    .expect("register should write");
    match next_event(&mut server.events).await {
        RelayLifecycleEvent::Registered { .. } => {}
        other => panic!("expected Registered, got {other:?}"),
    }
    link
}

/// Reads until the link ends; a closed lifecycle link surfaces as EOF or
/// an error, never as a hang.
pub(super) async fn expect_link_closed(tls: &mut ClientTls) {
    let mut buf = [0u8; 1];
    let outcome = timeout(NO_DEADLOCK, tls.read(&mut buf)).await;
    match outcome {
        Ok(Ok(0)) => {}
        Ok(Err(_)) => {}
        other => panic!("link should close, got {other:?}"),
    }
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

    // Kept alive across shutdown: link tasks must exit on the shutdown watch.
    let _link = register_node(&mut server, &client, &server_der).await;
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
    write_frame(
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
        write_frame(&mut link, &Frame::Heartbeat)
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

    let mut stale = register_node(&mut server, &client, &server_der).await;

    let mut fresh = open_node_link(server.server.local_addr(), &client, &server_der)
        .await
        .expect("second handshake should succeed");
    write_frame(
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

    let mut link = register_node(&mut server, &client, &server_der).await;
    active_connections_reaches(&server.server, 1).await;

    write_frame(&mut link, &Frame::Unregister)
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
    write_frame(
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
    write_frame(&mut link, &Frame::Heartbeat)
        .await
        .expect("frame should write");
    expect_link_closed(&mut link).await;
    active_connections_reaches(&server.server, 0).await;
    server.server.shutdown().await;
}

#[tokio::test]
async fn data_on_unknown_stream_gets_error_and_link_stays() {
    let client = TestNode::generate();
    let mut server = start_test_server_with(
        &[client.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut link = register_node(&mut server, &client, &server_der).await;
    active_connections_reaches(&server.server, 1).await;

    write_frame(
        &mut link,
        &Frame::Data {
            stream_id: 1,
            payload: b"too early".to_vec(),
        },
    )
    .await
    .expect("frame should write");
    assert!(
        matches!(
            read_link_frame(&mut link).await,
            Frame::Error {
                stream_id: 1,
                code: error_code::STREAM_UNKNOWN,
                ..
            }
        ),
        "data on a stream that was never opened must get STREAM_UNKNOWN"
    );

    // The link survives the bad stream frame: a heartbeat is accepted, and
    // an orderly unregister still works.
    write_frame(&mut link, &Frame::Heartbeat)
        .await
        .expect("heartbeat should write");
    write_frame(&mut link, &Frame::Unregister)
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
