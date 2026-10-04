//! Presence channel tests (issue #32, step 3): raw relay links observing
//! presence replay/fan-out for watch/unregister/link-loss/admin-remove/
//! eviction/replacement, plus the node client handle semantics (explicit
//! watch, implicit watch on open_stream, re-declaration across reconnects).

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use super::admin::admin_request;
use super::client::{
    expect_connected, long_lived_lifecycle, next_client_event, node_credentials_in,
    relay_client_config, relay_fingerprint, wait_admin_lists_node,
};
use super::*;
use crate::infra::peer_connection::PeerConnection;
use crate::infra::relay_client::{RelayClient, RelayClientEvent, RelayClientHandle};

const NO_PRESENCE_WINDOW: Duration = Duration::from_millis(300);

/// Writes `Frame::Watch` for `target` on `link` and reads the replay answer.
async fn watch_and_read_replay(link: &mut ClientTls, target: &str) -> Frame {
    write_frame(
        link,
        &Frame::Watch {
            node_id: target.to_string(),
        },
    )
    .await
    .expect("watch should write");
    read_link_frame(link).await
}

#[tokio::test]
async fn watch_replays_online_and_unknown_targets() {
    install_provider();
    let watcher_node = TestNode::generate();
    let target = TestNode::generate();
    let mut server = start_test_server_with(
        &[watcher_node.fingerprint(), target.fingerprint()],
        long_lived_lifecycle(),
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut watcher = register_node(&mut server, &watcher_node, &server_der).await;
    let _target_link = register_node(&mut server, &target, &server_der).await;

    let replay = watch_and_read_replay(&mut watcher, &target.fingerprint()).await;
    assert!(
        matches!(
            replay,
            Frame::Presence { ref node_id, online: true } if node_id == &target.fingerprint()
        ),
        "watching a registered target replays online: {replay:?}"
    );

    let never_seen = TestNode::generate();
    let replay = watch_and_read_replay(&mut watcher, &never_seen.fingerprint()).await;
    assert!(
        matches!(
            replay,
            Frame::Presence { ref node_id, online: false } if node_id == &never_seen.fingerprint()
        ),
        "watching an unknown target replays offline: {replay:?}"
    );

    server.server.shutdown().await;
}

#[tokio::test]
async fn unregister_publishes_offline_to_watchers() {
    install_provider();
    let watcher_node = TestNode::generate();
    let target = TestNode::generate();
    let mut server = start_test_server_with(
        &[watcher_node.fingerprint(), target.fingerprint()],
        long_lived_lifecycle(),
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut watcher = register_node(&mut server, &watcher_node, &server_der).await;
    let mut target_link = register_node(&mut server, &target, &server_der).await;
    let replay = watch_and_read_replay(&mut watcher, &target.fingerprint()).await;
    assert!(matches!(replay, Frame::Presence { online: true, .. }));

    write_frame(&mut target_link, &Frame::Unregister)
        .await
        .expect("unregister should write");
    let frame = read_link_frame(&mut watcher).await;
    assert!(
        matches!(
            frame,
            Frame::Presence { ref node_id, online: false } if node_id == &target.fingerprint()
        ),
        "unregister must publish offline: {frame:?}"
    );

    server.server.shutdown().await;
}

#[tokio::test]
async fn link_drop_publishes_offline_to_watchers() {
    install_provider();
    let watcher_node = TestNode::generate();
    let target = TestNode::generate();
    let mut server = start_test_server_with(
        &[watcher_node.fingerprint(), target.fingerprint()],
        long_lived_lifecycle(),
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut watcher = register_node(&mut server, &watcher_node, &server_der).await;
    let target_link = register_node(&mut server, &target, &server_der).await;
    let replay = watch_and_read_replay(&mut watcher, &target.fingerprint()).await;
    assert!(matches!(replay, Frame::Presence { online: true, .. }));

    // A process kill equivalent: the socket dies without an Unregister.
    drop(target_link);
    let frame = read_link_frame(&mut watcher).await;
    assert!(
        matches!(
            frame,
            Frame::Presence { ref node_id, online: false } if node_id == &target.fingerprint()
        ),
        "link loss must publish offline via the epilogue: {frame:?}"
    );

    server.server.shutdown().await;
}

#[tokio::test]
async fn admin_remove_publishes_offline_to_watchers() {
    install_provider();
    let watcher_node = TestNode::generate();
    let target = TestNode::generate();
    let mut server = start_test_server_with(
        &[watcher_node.fingerprint(), target.fingerprint()],
        long_lived_lifecycle(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();

    let mut watcher = register_node(&mut server, &watcher_node, &server_der).await;
    let _target_link = register_node(&mut server, &target, &server_der).await;
    let replay = watch_and_read_replay(&mut watcher, &target.fingerprint()).await;
    assert!(matches!(replay, Frame::Presence { online: true, .. }));

    let body = admin_request(
        &admin_addr,
        &format!(
            r#"{{"command":"remove","fingerprint":"{}"}}"#,
            target.fingerprint()
        ),
    )
    .await;
    let removed: serde_json::Value = serde_json::from_str(&body).expect("remove json");
    assert_eq!(removed["ok"], true);

    let frame = read_link_frame(&mut watcher).await;
    assert!(
        matches!(
            frame,
            Frame::Presence { ref node_id, online: false } if node_id == &target.fingerprint()
        ),
        "admin remove must publish offline: {frame:?}"
    );

    server.server.shutdown().await;
}

/// Reads the next frame, heartbeating the WATCHER link whenever the read
/// times out — the watcher must survive the fast eviction window it is
/// observing.
async fn read_frame_with_watcher_keepalive(link: &mut ClientTls) -> Frame {
    loop {
        match timeout(Duration::from_millis(100), read_link_frame(link)).await {
            Ok(frame) => return frame,
            Err(_) => {
                write_frame(link, &Frame::Heartbeat)
                    .await
                    .expect("watcher heartbeat should write");
            }
        }
    }
}

#[tokio::test]
async fn eviction_publishes_offline_to_watchers() {
    install_provider();
    let watcher_node = TestNode::generate();
    let target = TestNode::generate();
    let mut server = start_test_server_with(
        &[watcher_node.fingerprint(), target.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut watcher = register_node(&mut server, &watcher_node, &server_der).await;
    // Registered but silent: the sweeper evicts it after the offline window.
    let _target_link = register_node(&mut server, &target, &server_der).await;
    let replay = watch_and_read_replay(&mut watcher, &target.fingerprint()).await;
    assert!(matches!(replay, Frame::Presence { online: true, .. }));

    let frame = timeout(NO_DEADLOCK, read_frame_with_watcher_keepalive(&mut watcher))
        .await
        .expect("eviction presence should arrive");
    assert!(
        matches!(
            frame,
            Frame::Presence { ref node_id, online: false } if node_id == &target.fingerprint()
        ),
        "eviction must publish offline: {frame:?}"
    );

    server.server.shutdown().await;
}

#[tokio::test]
async fn duplicate_register_replacement_publishes_nothing() {
    install_provider();
    let watcher_node = TestNode::generate();
    let target = TestNode::generate();
    let mut server = start_test_server_with(
        &[watcher_node.fingerprint(), target.fingerprint()],
        long_lived_lifecycle(),
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut watcher = register_node(&mut server, &watcher_node, &server_der).await;
    let _first_link = register_node(&mut server, &target, &server_der).await;
    let replay = watch_and_read_replay(&mut watcher, &target.fingerprint()).await;
    assert!(matches!(replay, Frame::Presence { online: true, .. }));

    // A second link from the same node replaces the first (reconnect):
    // watchers must NOT see an offline/online churn.
    let mut second = open_node_link(server.server.local_addr(), &target, &server_der)
        .await
        .expect("second handshake should succeed");
    write_frame(
        &mut second,
        &Frame::Register {
            node_id: target.fingerprint(),
        },
    )
    .await
    .expect("re-register should write");
    assert!(
        matches!(
            next_event(&mut server.events).await,
            RelayLifecycleEvent::Replaced { .. }
        ),
        "the second register replaces the first"
    );

    let unexpected = timeout(NO_PRESENCE_WINDOW, read_link_frame(&mut watcher)).await;
    assert!(
        unexpected.is_err(),
        "replacement must not churn presence frames"
    );

    server.server.shutdown().await;
}

#[tokio::test]
async fn watcher_link_drop_purges_its_interests() {
    install_provider();
    let surviving_node = TestNode::generate();
    let doomed_node = TestNode::generate();
    let target = TestNode::generate();
    let mut server = start_test_server_with(
        &[
            surviving_node.fingerprint(),
            doomed_node.fingerprint(),
            target.fingerprint(),
        ],
        long_lived_lifecycle(),
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut surviving = register_node(&mut server, &surviving_node, &server_der).await;
    let mut doomed = register_node(&mut server, &doomed_node, &server_der).await;
    let target_link_holder = register_node(&mut server, &target, &server_der).await;
    let replay = watch_and_read_replay(&mut surviving, &target.fingerprint()).await;
    assert!(matches!(replay, Frame::Presence { online: true, .. }));
    let replay = watch_and_read_replay(&mut doomed, &target.fingerprint()).await;
    assert!(matches!(replay, Frame::Presence { online: true, .. }));

    // The doomed watcher goes away; the later transition must not panic and
    // must still reach the surviving watcher exactly once.
    drop(doomed);
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(target_link_holder);

    let frame = read_link_frame(&mut surviving).await;
    assert!(
        matches!(frame, Frame::Presence { online: false, .. }),
        "the surviving watcher still receives transitions: {frame:?}"
    );

    server.server.shutdown().await;
}

struct ClientTrio {
    server: RunningServer,
    /// `None` in slots whose handle was consumed by a test (e.g. cancelled to
    /// simulate a peer going away).
    handles: [Option<Arc<RelayClientHandle>>; 3],
    event_rxs: [mpsc::Receiver<RelayClientEvent>; 3],
    fps: [String; 3],
}

/// Three connected, registered clients.
async fn spawn_client_trio() -> ClientTrio {
    install_provider();
    let nodes = [
        TestNode::generate(),
        TestNode::generate(),
        TestNode::generate(),
    ];
    let fingerprints = [
        nodes[0].fingerprint(),
        nodes[1].fingerprint(),
        nodes[2].fingerprint(),
    ];
    let server = start_test_server_with(&fingerprints, long_lived_lifecycle()).await;
    let admin_addr = server.server.admin_addr().clone();
    let relay_fingerprint = relay_fingerprint(&server);
    let addr = server.server.local_addr();

    let mut handles = Vec::with_capacity(3);
    let mut event_rxs = Vec::with_capacity(3);
    for (index, node) in nodes.iter().enumerate() {
        let dir = temp_dir(&format!("relay-presence-{index}"));
        let credentials = node_credentials_in(&dir, node);
        let (event_tx, event_rx) = mpsc::channel(16);
        let handle = RelayClient::spawn(
            relay_client_config(
                addr,
                relay_fingerprint.clone(),
                credentials,
                Duration::from_millis(50),
            ),
            event_tx,
        );
        handles.push(Some(Arc::new(handle)));
        event_rxs.push(event_rx);
    }
    for (index, rx) in event_rxs.iter_mut().enumerate() {
        expect_connected(rx).await;
        wait_admin_lists_node(&admin_addr, &fingerprints[index]).await;
    }

    ClientTrio {
        server,
        handles: [handles.remove(0), handles.remove(0), handles.remove(0)],
        event_rxs: [
            event_rxs.remove(0),
            event_rxs.remove(0),
            event_rxs.remove(0),
        ],
        fps: fingerprints,
    }
}

impl ClientTrio {
    async fn finish(self) {
        for handle in self.handles.into_iter().flatten() {
            match Arc::try_unwrap(handle) {
                Ok(handle) => handle.cancel(),
                Err(_) => panic!("handle still shared at teardown"),
            }
        }
        self.server.server.shutdown().await;
    }
}

async fn expect_presence(rx: &mut mpsc::Receiver<RelayClientEvent>, node_id: &str, online: bool) {
    match next_client_event(rx).await {
        RelayClientEvent::Presence {
            node_id: seen,
            online: seen_online,
        } if seen == node_id && seen_online == online => {}
        other => panic!("expected Presence({node_id}, {online}), got {other:?}"),
    }
}

/// `open_stream` blocks on the client runtime; drive it from a scoped std
/// thread like the stream tests do.
fn open_stream_blocking(
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

/// Takes the handle out of a trio slot and cancels it (brief join; safe on
/// the test thread).
fn cancel_slot(slot: &mut Option<Arc<RelayClientHandle>>, context: &str) {
    let handle = slot
        .take()
        .unwrap_or_else(|| panic!("{context}: slot empty"));
    match Arc::try_unwrap(handle) {
        Ok(handle) => handle.cancel(),
        Err(_) => panic!("{context}: handle still shared"),
    }
}

#[tokio::test]
async fn handle_watch_observes_cancelled_peer_going_offline() {
    let mut trio = spawn_client_trio().await;
    // watch is a synchronous handle method (try_send based) — safe to call
    // directly from the async test context.
    trio.handles[2]
        .as_ref()
        .expect("C present")
        .watch(&trio.fps[0])
        .expect("watch should record and declare");
    expect_presence(&mut trio.event_rxs[2], &trio.fps[0], true).await;

    // Cancel the watched peer: its link dies without an Unregister, so the
    // relay epilogue publishes offline.
    cancel_slot(&mut trio.handles[0], "watched peer");
    expect_presence(&mut trio.event_rxs[2], &trio.fps[0], false).await;

    trio.finish().await;
}

#[tokio::test]
async fn open_stream_establishes_an_implicit_watch() {
    let mut trio = spawn_client_trio().await;
    let stream = open_stream_blocking(trio.handles[1].as_ref().expect("B present"), &trio.fps[2]);
    drop(stream);

    // The implicit watch answers with exactly one replay before any
    // transition; consume it first so the queue holds only the upcoming
    // offline. Reading the replay here also fixes its order ahead of the
    // teardown: on a slow runner (Windows CI, issue #119) the replay can
    // otherwise race the cancel and surface before the transition.
    expect_presence(&mut trio.event_rxs[1], &trio.fps[2], true).await;

    // Cancelling the stream target publishes offline to the implicit watcher.
    cancel_slot(&mut trio.handles[2], "stream target");
    expect_presence(&mut trio.event_rxs[1], &trio.fps[2], false).await;

    trio.finish().await;
}

#[tokio::test]
async fn reconnect_redeclares_watches_and_replays_presence() {
    let mut trio = spawn_client_trio().await;
    let admin_addr = trio.server.server.admin_addr().clone();
    trio.handles[2]
        .as_ref()
        .expect("C present")
        .watch(&trio.fps[0])
        .expect("watch should record and declare");
    expect_presence(&mut trio.event_rxs[2], &trio.fps[0], true).await;

    // Force C's link through the reconnect path (admin remove + re-admit,
    // mirroring the stream tests): the client must re-declare the persisted
    // watch on the new connection and receive a fresh replay.
    let body = admin_request(
        &admin_addr,
        &format!(r#"{{"command":"remove","fingerprint":"{}"}}"#, trio.fps[2]),
    )
    .await;
    let removed: serde_json::Value = serde_json::from_str(&body).expect("remove json");
    assert_eq!(removed["ok"], true);
    fs::write(
        trio.server.config.authorized_nodes_dir.join(&trio.fps[2]),
        b"",
    )
    .expect("whitelist should re-admit C");
    expect_connected(&mut trio.event_rxs[2]).await;
    expect_presence(&mut trio.event_rxs[2], &trio.fps[0], true).await;

    trio.finish().await;
}
