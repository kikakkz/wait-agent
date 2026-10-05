//! End-to-end relay stream tests (issue #32, step 2): two node relay
//! clients routing node-to-node streams through the real relay server.
//! `RelayClientHandle::open_stream`/`accept_inbound` block the calling
//! thread (`block_on`/`blocking_recv` panic in async contexts), so the test
//! bodies drive them from scoped std threads; the async test runtime only
//! runs the relay server and the stream I/O.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use super::admin::admin_request;
use super::client::{
    expect_connected, expect_disconnected, next_client_event, node_credentials_in,
    relay_client_config,
};
use super::pair::{
    accept_inbound_on_thread, cancel_handle, open_stream_blocking, recv_inbound,
    spawn_connected_pair,
};
use super::*;
use crate::infra::relay_client::{RelayClient, RelayClientError, RelayClientEvent};

#[tokio::test]
async fn open_stream_echoes_both_directions() {
    let pair = spawn_connected_pair().await;
    let accepted_rx = accept_inbound_on_thread(pair.handle_b.clone());
    let mut a_stream = open_stream_blocking(&pair.handle_a, &pair.fp_b);
    let mut b_stream = recv_inbound(accepted_rx, "echo setup").await;

    a_stream
        .write_all(b"ping")
        .await
        .expect("A should write through the relay");
    let mut buf = [0u8; 4];
    timeout(NO_DEADLOCK, b_stream.read_exact(&mut buf))
        .await
        .expect("B read should not deadlock")
        .expect("B should read through the relay");
    assert_eq!(&buf, b"ping");

    b_stream
        .write_all(b"pong")
        .await
        .expect("B should write back");
    timeout(NO_DEADLOCK, a_stream.read_exact(&mut buf))
        .await
        .expect("A read should not deadlock")
        .expect("A should read the reply");
    assert_eq!(&buf, b"pong");

    pair.finish().await;
}

#[tokio::test]
async fn half_close_drains_then_eof_and_reverse_leg_stays_open() {
    let pair = spawn_connected_pair().await;
    let accepted_rx = accept_inbound_on_thread(pair.handle_b.clone());
    let mut a_stream = open_stream_blocking(&pair.handle_a, &pair.fp_b);
    let mut b_stream = recv_inbound(accepted_rx, "half-close setup").await;

    a_stream
        .write_all(b"before-close")
        .await
        .expect("A write before half-close");
    a_stream.shutdown().await.expect("A half-close");

    // B reads the buffered bytes, then EOF; the relay close_leg is
    // directional.
    let mut received = Vec::new();
    timeout(NO_DEADLOCK, b_stream.read_to_end(&mut received))
        .await
        .expect("B read should drain to EOF, not hang")
        .expect("B read after peer half-close");
    assert_eq!(received, b"before-close");

    b_stream
        .write_all(b"after-peer-close")
        .await
        .expect("B write leg is still open");
    let mut buf = [0u8; 16];
    timeout(NO_DEADLOCK, a_stream.read_exact(&mut buf))
        .await
        .expect("A read on the open leg should not deadlock")
        .expect("A read on the open leg");
    assert_eq!(&buf, b"after-peer-close");

    pair.finish().await;
}

#[tokio::test]
async fn peer_reconnect_resets_routed_streams() {
    let mut pair = spawn_connected_pair().await;
    let accepted_rx = accept_inbound_on_thread(pair.handle_b.clone());
    let mut a_stream = open_stream_blocking(&pair.handle_a, &pair.fp_b);
    let mut b_stream = recv_inbound(accepted_rx, "reconnect setup").await;

    a_stream
        .write_all(b"hello")
        .await
        .expect("A write before the drop");
    let mut buf = [0u8; 5];
    timeout(NO_DEADLOCK, b_stream.read_exact(&mut buf))
        .await
        .expect("B read should not deadlock")
        .expect("B read before the drop");
    assert_eq!(&buf, b"hello");

    // Retire A's link the way a process restart would (a plain shutdown
    // leaves link tasks alive), then re-admit it so the auto-retry succeeds.
    let admin_addr = pair.server.server.admin_addr().clone();
    let body = admin_request(
        &admin_addr,
        &format!(r#"{{"command":"remove","fingerprint":"{}"}}"#, pair.fp_a),
    )
    .await;
    let removed: serde_json::Value = serde_json::from_str(&body).expect("remove json");
    assert_eq!(removed["ok"], true);
    expect_disconnected(&mut pair.event_rx_a).await;
    fs::write(
        pair.server.config.authorized_nodes_dir.join(&pair.fp_a),
        b"",
    )
    .expect("whitelist should re-admit A");
    expect_connected(&mut pair.event_rx_a).await;

    // B's stream leg dies with the route: reset or EOF, never a hang.
    let mut tail = [0u8; 1];
    let outcome = timeout(NO_DEADLOCK, b_stream.read(&mut tail))
        .await
        .expect("B's stream must end after the peer link dies, not hang");
    match outcome {
        Ok(0) => {}
        Ok(_) => panic!("B's stream must not deliver data after the peer link died"),
        Err(error) => {
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset,
                "unexpected error: {error}"
            );
        }
    }

    // A re-registered on its own: a fresh stream flows end to end again.
    let accepted_rx = accept_inbound_on_thread(pair.handle_b.clone());
    let mut fresh_a = open_stream_blocking(&pair.handle_a, &pair.fp_b);
    let mut fresh_b = recv_inbound(accepted_rx, "post-reconnect open").await;
    fresh_a
        .write_all(b"back")
        .await
        .expect("A write after reconnect");
    let mut buf = [0u8; 4];
    timeout(NO_DEADLOCK, fresh_b.read_exact(&mut buf))
        .await
        .expect("B read after reconnect should not deadlock")
        .expect("B read after reconnect");
    assert_eq!(&buf, b"back");

    pair.finish().await;
}

#[tokio::test]
async fn large_transfer_completes_with_window_grants_flowing() {
    let pair = spawn_connected_pair().await;
    let accepted_rx = accept_inbound_on_thread(pair.handle_b.clone());
    let a_stream = open_stream_blocking(&pair.handle_a, &pair.fp_b);
    let mut b_stream = recv_inbound(accepted_rx, "window setup").await;

    // 1 MiB is well past the 256 KiB initial window: the transfer only
    // completes if Window grants flow reader → relay → writer.
    const TOTAL: usize = 1024 * 1024;
    let payload = vec![0x5au8; TOTAL];
    let mut writer_task = tokio::spawn(async move {
        let mut writer = a_stream;
        writer
            .write_all(&payload)
            .await
            .expect("write_all should finish once grants flow");
    });

    // With nobody reading, the writer parks once the initial window is
    // exhausted.
    let premature = timeout(Duration::from_millis(300), &mut writer_task).await;
    assert!(
        premature.is_err(),
        "writer must park once the initial window is exhausted"
    );

    let mut received = vec![0u8; TOTAL];
    timeout(NO_DEADLOCK, b_stream.read_exact(&mut received))
        .await
        .expect("read should not deadlock")
        .expect("reader should receive every byte");
    assert!(received.iter().all(|byte| *byte == 0x5a));
    timeout(NO_DEADLOCK, &mut writer_task)
        .await
        .expect("writer should complete after grants flow")
        .expect("writer task");

    pair.finish().await;
}

#[tokio::test]
async fn open_to_unknown_target_errors_on_use_and_link_stays_up() {
    let pair = spawn_connected_pair().await;
    let unknown_fingerprint = "e".repeat(64);
    let mut refused = open_stream_blocking(&pair.handle_a, &unknown_fingerprint);

    // The open is queued locally; the refusal arrives asynchronously as an
    // Error frame for the stream and surfaces on use.
    let mut buf = [0u8; 1];
    let outcome = timeout(NO_DEADLOCK, refused.read(&mut buf))
        .await
        .expect("the refusal must surface on stream use, not hang");
    let error = outcome.expect_err("read on a refused stream must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);

    // The link survives the refusal: B stays reachable in both directions.
    let accepted_rx = accept_inbound_on_thread(pair.handle_b.clone());
    let mut a_stream = open_stream_blocking(&pair.handle_a, &pair.fp_b);
    let mut b_stream = recv_inbound(accepted_rx, "link-alive probe").await;
    a_stream
        .write_all(b"still-up")
        .await
        .expect("A write after the refusal");
    let mut buf = [0u8; 8];
    timeout(NO_DEADLOCK, b_stream.read_exact(&mut buf))
        .await
        .expect("B read after the refusal should not deadlock")
        .expect("B read after the refusal");
    assert_eq!(&buf, b"still-up");

    pair.finish().await;
}

#[tokio::test]
async fn handle_stream_apis_report_not_connected_without_a_link() {
    install_provider();
    let node = TestNode::generate();
    let dir = temp_dir("relay-streams-not-connected");
    let credentials = node_credentials_in(&dir, &node);
    // A port nothing listens on: the client retries forever without ever
    // installing an opener, so the stream APIs must report NotConnected.
    let dead_addr = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("probe addr");

    let (event_tx, mut event_rx) = mpsc::channel(16);
    let handle = RelayClient::spawn(
        relay_client_config(
            dead_addr,
            node.fingerprint(),
            credentials,
            Duration::from_millis(50),
        ),
        event_tx,
    );
    let handle = Arc::new(handle);
    // Consume the Connecting event; the dial failures then retry quietly.
    match next_client_event(&mut event_rx).await {
        RelayClientEvent::Connecting { .. } => {}
        other => panic!("expected Connecting first, got {other:?}"),
    }
    let open_result =
        std::thread::scope(|scope| scope.spawn(|| handle.open_stream("any-target")).join())
            .expect("probe thread should not panic");
    match open_result {
        Err(RelayClientError::NotConnected) => {}
        Err(other) => panic!("unexpected error from open_stream: {other}"),
        Ok(_) => panic!("open_stream without a link must report NotConnected"),
    }

    cancel_handle(handle);
}
