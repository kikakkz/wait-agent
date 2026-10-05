//! Shared two-connected-clients fixture for relay stream tests: the real
//! relay server plus two registered node clients whose relay-routed streams
//! the tests exercise. `RelayClientHandle::open_stream`/`accept_inbound`
//! block the calling thread (`block_on`/`blocking_recv` panic in async
//! contexts), so the blocking calls run on scoped std threads and the async
//! test runtime stays free for the relay server and stream I/O.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::client::{
    expect_connected, long_lived_lifecycle, node_credentials_in, relay_client_config,
    relay_fingerprint, wait_admin_lists_node,
};
use super::*;
use crate::infra::node_credentials::NodeCredentialPaths;
use crate::infra::peer_connection::PeerConnection;
use crate::infra::relay_client::{RelayClient, RelayClientEvent, RelayClientHandle};

pub(super) struct NodePair {
    pub(super) server: RunningServer,
    pub(super) handle_a: Arc<RelayClientHandle>,
    pub(super) handle_b: Arc<RelayClientHandle>,
    pub(super) event_rx_a: mpsc::Receiver<RelayClientEvent>,
    // Held (not asserted on) so B's event channel stays open for the run.
    pub(super) _event_rx_b: mpsc::Receiver<RelayClientEvent>,
    pub(super) fp_a: String,
    pub(super) fp_b: String,
    /// B's node credential files; B's certificate doubles as the TLS
    /// identity tests serve when accepting relay-routed inbound streams
    /// (the routed pin is B's relay fingerprint, the SPKI hash of this
    /// certificate).
    pub(super) credentials_b: NodeCredentialPaths,
}

/// Spawns the relay plus two connected, registered clients. Both
/// registrations are confirmed through the admin socket so an open cannot
/// race a not-yet-registered target.
pub(super) async fn spawn_connected_pair() -> NodePair {
    install_provider();
    let node_a = TestNode::generate();
    let node_b = TestNode::generate();
    let server = start_test_server_with(
        &[node_a.fingerprint(), node_b.fingerprint()],
        long_lived_lifecycle(),
    )
    .await;
    let admin_addr = server.server.admin_addr().clone();
    let relay_fingerprint = relay_fingerprint(&server);
    let addr = server.server.local_addr();
    let dir_a = temp_dir("relay-streams-a");
    let dir_b = temp_dir("relay-streams-b");
    let credentials_a = node_credentials_in(&dir_a, &node_a);
    let credentials_b = node_credentials_in(&dir_b, &node_b);

    let (event_tx_a, mut event_rx_a) = mpsc::channel(16);
    let handle_a = RelayClient::spawn(
        relay_client_config(
            addr,
            relay_fingerprint.clone(),
            credentials_a,
            Duration::from_millis(50),
        ),
        event_tx_a,
    );
    let (event_tx_b, mut event_rx_b) = mpsc::channel(16);
    let handle_b = RelayClient::spawn(
        relay_client_config(
            addr,
            relay_fingerprint,
            credentials_b.clone(),
            Duration::from_millis(50),
        ),
        event_tx_b,
    );

    expect_connected(&mut event_rx_a).await;
    expect_connected(&mut event_rx_b).await;
    wait_admin_lists_node(&admin_addr, &node_a.fingerprint()).await;
    wait_admin_lists_node(&admin_addr, &node_b.fingerprint()).await;

    NodePair {
        server,
        handle_a: Arc::new(handle_a),
        handle_b: Arc::new(handle_b),
        event_rx_a,
        _event_rx_b: event_rx_b,
        fp_a: node_a.fingerprint(),
        fp_b: node_b.fingerprint(),
        credentials_b,
    }
}

impl NodePair {
    pub(super) async fn finish(self) {
        cancel_handle(self.handle_a);
        cancel_handle(self.handle_b);
        self.server.server.shutdown().await;
    }
}

pub(super) fn cancel_handle(handle: Arc<RelayClientHandle>) {
    match Arc::try_unwrap(handle) {
        Ok(handle) => handle.cancel(),
        Err(_) => panic!("handle is still shared with an accept thread at teardown"),
    }
}

/// `open_stream` blocks its caller; run it on a scoped std thread so the
/// async test runtime is never blocked.
pub(super) fn open_stream_blocking(
    handle: &RelayClientHandle,
    target_node_id: &str,
) -> Box<dyn PeerConnection> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| handle.open_stream(target_node_id).expect("open stream"))
            .join()
            .expect("open thread should not panic")
    })
}

/// `accept_inbound` blocks until a relay-routed inbound stream arrives; run
/// it on a std thread and hand the result back through a std channel.
pub(super) fn accept_inbound_on_thread(
    handle: Arc<RelayClientHandle>,
) -> std::sync::mpsc::Receiver<Option<Box<dyn PeerConnection>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.accept_inbound());
    });
    rx
}

/// Awaits the inbound accept result WITHOUT blocking the single-threaded
/// test runtime (the relay server runs on it): the std-channel wait happens
/// on the blocking thread pool. A blocked test thread would freeze the
/// relay's link loops and starve every routed frame.
pub(super) async fn recv_inbound(
    rx: std::sync::mpsc::Receiver<Option<Box<dyn PeerConnection>>>,
    context: &str,
) -> Box<dyn PeerConnection> {
    let context = context.to_string();
    tokio::task::spawn_blocking({
        let context = context.clone();
        move || {
            rx.recv_timeout(NO_DEADLOCK)
                .unwrap_or_else(|error| panic!("{context}: inbound accept should arrive: {error}"))
                .unwrap_or_else(|| panic!("{context}: client stopped before accepting"))
        }
    })
    .await
    .unwrap_or_else(|error| panic!("{context}: accept waiter failed: {error}"))
}
