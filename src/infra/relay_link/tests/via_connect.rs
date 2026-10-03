//! End-to-end relay-via dial tests (issue #35 PR-B): a pinned inner-TLS
//! connection dialed through the relay's routed streams — handshake against
//! the peer's certificate fingerprint plus echo over the established stream.
//! The direct dialer over loopback TCP mirrors the pair so both dial paths
//! are proven against the same peer identity. Handle calls that block run on
//! scoped std threads; the async test runtime stays free for the relay.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use super::client::{
    expect_connected, long_lived_lifecycle, node_credentials_in, relay_client_config,
    relay_fingerprint, wait_admin_lists_node,
};
use super::*;
use crate::infra::node_credentials::NodeCredentialPaths;
use crate::infra::relay_client::RelayClient;
use crate::infra::remote_grpc_transport::{
    test_dial_with_connector, test_direct_dialer, test_relay_dialer,
};

const PAYLOAD: &[u8] = b"ping-over-the-relay";

struct ViaPair {
    server: RunningServer,
    handle_a: Arc<crate::infra::relay_client::RelayClientHandle>,
    handle_b: Arc<crate::infra::relay_client::RelayClientHandle>,
    #[allow(dead_code)]
    event_rxs: Vec<mpsc::Receiver<crate::infra::relay_client::RelayClientEvent>>,
    fp_b: String,
    credentials_b: NodeCredentialPaths,
}

async fn spawn_pair() -> ViaPair {
    install_provider();
    let node_a = TestNode::generate();
    let node_b = TestNode::generate();
    let server = start_test_server_with(
        &[node_a.fingerprint(), node_b.fingerprint()],
        long_lived_lifecycle(),
    )
    .await;
    let admin_addr = server.server.admin_addr().clone();
    let relay_fp = relay_fingerprint(&server);
    let addr = server.server.local_addr();
    let dir_a = temp_dir("relay-via-connect-a");
    let dir_b = temp_dir("relay-via-connect-b");
    let credentials_a = node_credentials_in(&dir_a, &node_a);
    let credentials_b = node_credentials_in(&dir_b, &node_b);

    let mut event_rxs = Vec::new();
    let (tx_a, rx_a) = mpsc::channel(16);
    let handle_a = RelayClient::spawn(
        relay_client_config(
            addr,
            relay_fp.clone(),
            credentials_a,
            Duration::from_millis(50),
        ),
        tx_a,
    );
    let (tx_b, rx_b) = mpsc::channel(16);
    let handle_b = RelayClient::spawn(
        relay_client_config(
            addr,
            relay_fp,
            credentials_b.clone(),
            Duration::from_millis(50),
        ),
        tx_b,
    );
    let mut rx_a = rx_a;
    let mut rx_b = rx_b;
    expect_connected(&mut rx_a).await;
    expect_connected(&mut rx_b).await;
    wait_admin_lists_node(&admin_addr, &node_a.fingerprint()).await;
    wait_admin_lists_node(&admin_addr, &node_b.fingerprint()).await;
    event_rxs.push(rx_a);
    event_rxs.push(rx_b);

    ViaPair {
        server,
        handle_a: Arc::new(handle_a),
        handle_b: Arc::new(handle_b),
        event_rxs,
        fp_b: node_b.fingerprint(),
        credentials_b,
    }
}

impl ViaPair {
    async fn finish(self) {
        for handle in [self.handle_a, self.handle_b] {
            match Arc::try_unwrap(handle) {
                Ok(handle) => handle.cancel(),
                Err(_) => panic!("handle still shared at teardown"),
            }
        }
        self.server.server.shutdown().await;
    }
}

/// Loads the peer's written PEMs into a server config that presents the peer
/// identity (the fingerprint the client pins).
fn peer_server_config(credentials: &NodeCredentialPaths) -> Arc<rustls::ServerConfig> {
    let cert_pem = fs::read_to_string(&credentials.cert_path).expect("peer cert readable");
    let key_pem = fs::read_to_string(&credentials.key_path).expect("peer key readable");
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .expect("peer cert parses");
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .expect("peer key parses")
        .expect("peer key present");
    Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("peer server config builds"),
    )
}

/// Echoes until the peer closes, the deadline passes, or the connection
/// errors. All three are clean exits for this fixture: the test's
/// assertions live on the client side (dial, handshake, echo equality,
/// shutdown), so a peer-side read error after teardown begins — or a stalled
/// close cascade under load (relay scheduler backpressure, issue #33
/// territory) — must fail the peer gracefully, never hang it.
async fn echo_until_close_or_deadline<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    deadline: Duration,
) {
    let mut buf = [0u8; 512];
    loop {
        let read = tokio::time::timeout(deadline, stream.read(&mut buf)).await;
        let n = match read {
            Err(_) => return,
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return,
        };
        if stream.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

#[tokio::test]
async fn relay_via_dial_completes_inner_tls_pin_handshake_and_echo() {
    let pair = spawn_pair().await;

    // Peer side: accept the routed inbound stream on a std thread (the
    // handle call blocks), then serve the pinned identity over TLS and echo.
    let peer_handle = pair.handle_b.clone();
    let peer_config = peer_server_config(&pair.credentials_b);
    let (peer_done_tx, peer_done_rx) = std::sync::mpsc::channel();
    let peer_thread = std::thread::spawn(move || {
        let conn = peer_handle
            .accept_inbound()
            .expect("peer accepts the routed inbound stream");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("peer runtime");
        runtime.block_on(async move {
            let tls = tokio_rustls::TlsAcceptor::from(peer_config)
                .accept(conn)
                .await
                .expect("peer inner-TLS handshake");
            echo_until_close_or_deadline(tls, Duration::from_secs(5)).await;
        });
        let _ = peer_done_tx.send(());
    });

    // Client side: the pinned connector dials through the relay. The URI
    // host is unused on the relay path (the pin is the routed target); the
    // call runs in this async context, proving the spawn_blocking bridge.
    let mut io = timeout(
        NO_DEADLOCK,
        test_dial_with_connector(
            "http://unused:443",
            &pair.fp_b,
            test_relay_dialer(pair.handle_a.clone()),
        ),
    )
    .await
    .expect("relay dial within deadline")
    .expect("relay dial + inner TLS handshake succeed");
    timeout(NO_DEADLOCK, io.write_all(PAYLOAD))
        .await
        .expect("client write within deadline")
        .expect("client write");
    let mut received = vec![0u8; PAYLOAD.len()];
    timeout(NO_DEADLOCK, io.read_exact(&mut received))
        .await
        .expect("echo within deadline")
        .expect("client read");
    assert_eq!(received, PAYLOAD, "echo over the relay-routed TLS stream");
    // close_notify so the peer observes a clean close (a bare drop sends
    // nothing on the routed stream).
    timeout(NO_DEADLOCK, io.shutdown())
        .await
        .expect("client shutdown within deadline")
        .expect("client half-close");
    drop(io);

    // The peer bounds its own echo loop; wait for its done signal and its
    // thread WITHOUT parking the test runtime on a blocking join.
    tokio::task::spawn_blocking(move || {
        peer_done_rx
            .recv_timeout(NO_DEADLOCK)
            .expect("peer echo completes within deadline");
        peer_thread.join().expect("peer thread");
    })
    .await
    .expect("peer waiter");
    pair.finish().await;
}

#[tokio::test]
async fn direct_dial_still_completes_inner_tls_pin_handshake_and_echo() {
    let pair = spawn_pair().await;
    let peer_config = peer_server_config(&pair.credentials_b);

    // Loopback TCP peer presenting the same pinned identity.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener binds");
    let addr = listener.local_addr().expect("listener addr");
    let (peer_done_tx, peer_done_rx) = std::sync::mpsc::channel();
    let peer_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("peer accepts");
        let tls = tokio_rustls::TlsAcceptor::from(peer_config)
            .accept(stream)
            .await
            .expect("peer inner-TLS handshake");
        echo_until_close_or_deadline(tls, Duration::from_secs(5)).await;
        let _ = peer_done_tx.send(());
    });

    let mut io = timeout(
        NO_DEADLOCK,
        test_dial_with_connector(&format!("http://{addr}"), &pair.fp_b, test_direct_dialer()),
    )
    .await
    .expect("direct dial within deadline")
    .expect("direct dial + inner TLS handshake succeed");
    timeout(NO_DEADLOCK, io.write_all(PAYLOAD))
        .await
        .expect("client write within deadline")
        .expect("client write");
    let mut received = vec![0u8; PAYLOAD.len()];
    timeout(NO_DEADLOCK, io.read_exact(&mut received))
        .await
        .expect("echo within deadline")
        .expect("client read");
    assert_eq!(received, PAYLOAD, "echo over the direct TLS stream");
    timeout(NO_DEADLOCK, io.shutdown())
        .await
        .expect("client shutdown within deadline")
        .expect("client half-close");
    drop(io);

    tokio::task::spawn_blocking(move || {
        peer_done_rx
            .recv_timeout(NO_DEADLOCK)
            .expect("peer echo completes within deadline");
    })
    .await
    .expect("peer waiter");
    timeout(NO_DEADLOCK, peer_task)
        .await
        .expect("peer task within deadline")
        .expect("peer task");
    pair.finish().await;
}
