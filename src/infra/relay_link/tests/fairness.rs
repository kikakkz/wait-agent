//! Fair scheduling and backpressure tests (issue #33): the per-link egress
//! scheduler must isolate a stalled stream from its peers (no head-of-line
//! blocking), interleave backlogged streams within the round budget, and let
//! control frames overtake bulk data. End-to-end stream credit (the nodes'
//! own window) stays the first backpressure layer.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::time::timeout;

use super::client::{
    long_lived_lifecycle, node_credentials_in, relay_client_config, relay_fingerprint,
    wait_admin_lists_node,
};
use super::*;
use crate::infra::relay_client::RelayClient;

const MAX_PAYLOAD_USIZE: usize = 16 * 1024;

type SplitLink = (ReadHalf<ClientTls>, WriteHalf<ClientTls>);

/// Runs one blocking `accept_inbound` on a std thread, returning the result
/// through a std channel.
fn spawn_accept(
    handle: std::sync::Arc<crate::infra::relay_client::RelayClientHandle>,
) -> std::sync::mpsc::Receiver<Option<Box<dyn crate::infra::peer_connection::PeerConnection>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.accept_inbound());
    });
    rx
}

/// Awaits an accept result WITHOUT parking the test runtime.
async fn await_accept(
    rx: std::sync::mpsc::Receiver<Option<Box<dyn crate::infra::peer_connection::PeerConnection>>>,
    context: &str,
) -> Box<dyn crate::infra::peer_connection::PeerConnection> {
    let context = context.to_string();
    tokio::task::spawn_blocking(move || {
        rx.recv_timeout(NO_DEADLOCK)
            .unwrap_or_else(|error| panic!("{context}: inbound accept should arrive: {error}"))
            .unwrap_or_else(|| panic!("{context}: client stopped before accepting"))
    })
    .await
    .unwrap_or_else(|error| panic!("accept waiter failed: {error}"))
}

fn split_link(link: ClientTls) -> SplitLink {
    tokio::io::split(link)
}

async fn read_one(link: &mut ReadHalf<ClientTls>) -> Frame {
    timeout(NO_DEADLOCK, read_frame(link))
        .await
        .expect("frame within deadline")
        .expect("frame decodes")
}

/// Raw links register explicitly (the mux client does this inside its run
/// loop); registration has no ack — the link simply becomes routable.
async fn register_raw_link(link: &mut ClientTls, fingerprint: &str) {
    write_frame(
        link,
        &Frame::Register {
            node_id: fingerprint.to_string(),
        },
    )
    .await
    .expect("register");
}

struct Pair {
    server: RunningServer,
    server_der: Vec<u8>,
    source: TestNode,
    target: TestNode,
    target_reader: ReadHalf<ClientTls>,
    #[allow(dead_code)]
    target_writer: WriteHalf<ClientTls>,
}

/// Registers source and target; the source's initial link is dropped (each
/// test opens its own), the target's link is split for the reader loop.
async fn registered_pair() -> Pair {
    install_provider();
    let source = TestNode::generate();
    let target = TestNode::generate();
    let mut server = start_test_server_with(
        &[source.fingerprint(), target.fingerprint()],
        long_lived_lifecycle(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let _source_link = register_node(&mut server, &source, &server_der).await;
    let target_link = register_node(&mut server, &target, &server_der).await;
    let (target_reader, target_writer) = split_link(target_link);
    Pair {
        server,
        server_der,
        source,
        target,
        target_reader,
        target_writer,
    }
}

/// Opens `count` streams from a fresh source link toward the target,
/// allocating the source-side odd ids `1, 3, ...`; returns the
/// relay-allocated target-side (even) ids in open order.
async fn open_streams_toward(
    source_link: &mut ClientTls,
    target_reader: &mut ReadHalf<ClientTls>,
    target_fp: &str,
    count: usize,
) -> Vec<u32> {
    let mut target_ids = Vec::with_capacity(count);
    for index in 0..count {
        let source_stream_id = 1 + 2 * index as u32;
        write_frame(
            source_link,
            &Frame::OpenStream {
                stream_id: source_stream_id,
                target_node_id: target_fp.to_string(),
            },
        )
        .await
        .expect("open stream");
        let frame = read_one(target_reader).await;
        match frame {
            Frame::OpenStream { stream_id, .. } => target_ids.push(stream_id),
            other => panic!("expected OpenStream, got {other:?}"),
        }
    }
    target_ids
}

#[tokio::test]
async fn no_head_of_line_blocking_for_a_stalled_stream() {
    let mut pair = registered_pair().await;
    let mut source_link = open_node_link(
        pair.server.server.local_addr(),
        &pair.source,
        &pair.server_der,
    )
    .await
    .expect("source handshake");
    register_raw_link(&mut source_link, &pair.source.fingerprint()).await;
    let target_ids = open_streams_toward(
        &mut source_link,
        &mut pair.target_reader,
        &pair.target.fingerprint(),
        2,
    )
    .await;
    let stalled_target_id = target_ids[0];
    let live_target_id = target_ids[1];

    // Pump the stalled stream well past one bucket's worth of data, then
    // send the live stream's marker.
    const PUMP_FRAMES: usize = 128; // 2 MiB: bucket 1 MiB + ingress slack
    let (_source_reader, mut source_writer) = split_link(source_link);
    let producer = tokio::spawn(async move {
        let payload = vec![0x11u8; MAX_PAYLOAD_USIZE];
        for _ in 0..PUMP_FRAMES {
            write_frame(
                &mut source_writer,
                &Frame::Data {
                    stream_id: 1,
                    payload: payload.clone(),
                },
            )
            .await
            .expect("stalled-stream frame");
        }
        write_frame(
            &mut source_writer,
            &Frame::Data {
                stream_id: 3,
                payload: b"marker".to_vec(),
            },
        )
        .await
        .expect("marker frame");
    });

    // The target reads everything but "processes" only the live stream:
    // the stalled stream's bytes are counted and dropped (its consumer is
    // absent).
    let mut stalled_bytes = 0usize;
    loop {
        let frame = read_one(&mut pair.target_reader).await;
        match frame {
            Frame::Data { stream_id, payload } if stream_id == stalled_target_id => {
                stalled_bytes += payload.len()
            }
            Frame::Data { stream_id, payload } if stream_id == live_target_id => {
                assert_eq!(payload, b"marker");
                break;
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    producer.await.expect("producer");
    assert!(
        stalled_bytes < PUMP_FRAMES * MAX_PAYLOAD_USIZE,
        "the marker must overtake the stalled stream's backlog (saw {stalled_bytes} bytes)"
    );

    pair.server.server.shutdown().await;
}

#[tokio::test]
async fn backlogged_streams_interleave_within_the_round_budget() {
    let mut pair = registered_pair().await;
    let mut source_link = open_node_link(
        pair.server.server.local_addr(),
        &pair.source,
        &pair.server_der,
    )
    .await
    .expect("source handshake");
    register_raw_link(&mut source_link, &pair.source.fingerprint()).await;
    let target_ids = open_streams_toward(
        &mut source_link,
        &mut pair.target_reader,
        &pair.target.fingerprint(),
        2,
    )
    .await;
    let first_target_id = target_ids[0];
    let second_target_id = target_ids[1];

    let (_source_reader, mut source_writer) = split_link(source_link);
    let producer = tokio::spawn(async move {
        let payload = vec![0u8; 4 * 1024];
        for _ in 0..400 {
            write_frame(
                &mut source_writer,
                &Frame::Data {
                    stream_id: 1,
                    payload: payload.clone(),
                },
            )
            .await
            .expect("stream 1 frame");
            write_frame(
                &mut source_writer,
                &Frame::Data {
                    stream_id: 3,
                    payload: payload.clone(),
                },
            )
            .await
            .expect("stream 3 frame");
        }
    });

    // 64 KiB round budget + one max frame: no stream may run further ahead
    // of the other while both hold backlog.
    const SKEW_BOUND_BYTES: usize = 2 * (64 * 1024 + MAX_PAYLOAD_USIZE);
    let mut first_bytes = 0usize;
    let mut second_bytes = 0usize;
    while first_bytes + second_bytes < 1024 * 1024 {
        let frame = read_one(&mut pair.target_reader).await;
        match frame {
            Frame::Data { stream_id, payload } if stream_id == first_target_id => {
                first_bytes += payload.len()
            }
            Frame::Data { stream_id, payload } if stream_id == second_target_id => {
                second_bytes += payload.len()
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        assert!(
            first_bytes.abs_diff(second_bytes) <= SKEW_BOUND_BYTES,
            "streams must stay within the round budget of each other: {first_bytes} vs {second_bytes}"
        );
    }
    assert!(
        first_bytes > 0 && second_bytes > 0,
        "both streams must progress"
    );
    producer.await.expect("producer");

    pair.server.server.shutdown().await;
}

#[tokio::test]
async fn control_frames_overtake_bulk_backlog() {
    let mut pair = registered_pair().await;
    let mut source_link = open_node_link(
        pair.server.server.local_addr(),
        &pair.source,
        &pair.server_der,
    )
    .await
    .expect("source handshake");
    register_raw_link(&mut source_link, &pair.source.fingerprint()).await;
    let target_ids = open_streams_toward(
        &mut source_link,
        &mut pair.target_reader,
        &pair.target.fingerprint(),
        2,
    )
    .await;
    let stalled_target_id = target_ids[0];
    let closing_target_id = target_ids[1];

    const PUMP_FRAMES: usize = 128;
    let (_source_reader, mut source_writer) = split_link(source_link);
    let producer = tokio::spawn(async move {
        let payload = vec![0x22u8; MAX_PAYLOAD_USIZE];
        for _ in 0..PUMP_FRAMES {
            write_frame(
                &mut source_writer,
                &Frame::Data {
                    stream_id: 1,
                    payload: payload.clone(),
                },
            )
            .await
            .expect("stalled-stream frame");
        }
        // Teardown the second stream: the relay forwards CloseStream to the
        // target as a priority frame on the target link's scheduler.
        write_frame(&mut source_writer, &Frame::CloseStream { stream_id: 3 })
            .await
            .expect("close stream");
    });

    let mut stalled_bytes = 0usize;
    loop {
        let frame = read_one(&mut pair.target_reader).await;
        match frame {
            Frame::Data { stream_id, payload } if stream_id == stalled_target_id => {
                stalled_bytes += payload.len()
            }
            Frame::CloseStream { stream_id } if stream_id == closing_target_id => break,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    producer.await.expect("producer");
    assert!(
        stalled_bytes < PUMP_FRAMES * MAX_PAYLOAD_USIZE,
        "the teardown must overtake the stalled stream's backlog (saw {stalled_bytes} bytes)"
    );

    pair.server.server.shutdown().await;
}

#[tokio::test]
async fn close_stream_arrives_under_bulk_backpressure() {
    let mut pair = registered_pair().await;
    let mut source_link = open_node_link(
        pair.server.server.local_addr(),
        &pair.source,
        &pair.server_der,
    )
    .await
    .expect("source handshake");
    register_raw_link(&mut source_link, &pair.source.fingerprint()).await;
    let target_ids = open_streams_toward(
        &mut source_link,
        &mut pair.target_reader,
        &pair.target.fingerprint(),
        2,
    )
    .await;
    let stalled_target_id = target_ids[0];

    // Pump stream 1 well past its 1 MiB bucket cap while the target never
    // consumes stream 1's data, then tear that same stream down: the
    // forwarded CloseStream reaches the target link's scheduler behind a
    // parked bulk arm, and must classify via the control channel instead of
    // queueing behind the stalled stream (issue #105).
    const PUMP_FRAMES: usize = 128; // 2 MiB: bucket 1 MiB + ingress slack
    let (_source_reader, mut source_writer) = split_link(source_link);
    let producer = tokio::spawn(async move {
        let payload = vec![0x33u8; MAX_PAYLOAD_USIZE];
        for _ in 0..PUMP_FRAMES {
            write_frame(
                &mut source_writer,
                &Frame::Data {
                    stream_id: 1,
                    payload: payload.clone(),
                },
            )
            .await
            .expect("stalled-stream frame");
        }
        write_frame(&mut source_writer, &Frame::CloseStream { stream_id: 1 })
            .await
            .expect("close stream");
    });

    // The target keeps reading the wire but drops stream 1's bytes (its
    // consumer is absent): the teardown must still arrive, ahead of the
    // bulk backlog that stalled the stream.
    let mut stalled_bytes = 0usize;
    loop {
        let frame = read_one(&mut pair.target_reader).await;
        match frame {
            Frame::Data { stream_id, payload } if stream_id == stalled_target_id => {
                stalled_bytes += payload.len()
            }
            Frame::CloseStream { stream_id } if stream_id == stalled_target_id => break,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    producer.await.expect("producer");
    assert!(
        stalled_bytes < PUMP_FRAMES * MAX_PAYLOAD_USIZE,
        "the stalled stream's own teardown must bypass its bulk backlog (saw {stalled_bytes} bytes)"
    );

    pair.server.server.shutdown().await;
}

#[tokio::test]
async fn end_to_end_stream_credit_bounds_a_stream_the_peer_never_reads() {
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
    let dir_a = temp_dir("relay-fairness-a");
    let dir_b = temp_dir("relay-fairness-b");
    let credentials_a = node_credentials_in(&dir_a, &node_a);
    let credentials_b = node_credentials_in(&dir_b, &node_b);

    let (event_tx_a, mut rx_a) = mpsc::channel(16);
    let handle_a = RelayClient::spawn(
        relay_client_config(
            addr,
            relay_fingerprint.clone(),
            credentials_a,
            Duration::from_millis(50),
        ),
        event_tx_a,
    );
    let (event_tx_b, mut rx_b) = mpsc::channel(16);
    let handle_b = RelayClient::spawn(
        relay_client_config(
            addr,
            relay_fingerprint,
            credentials_b,
            Duration::from_millis(50),
        ),
        event_tx_b,
    );
    wait_admin_lists_node(&admin_addr, &node_a.fingerprint()).await;
    wait_admin_lists_node(&admin_addr, &node_b.fingerprint()).await;
    // Drain the connect events so the receivers stay usable.
    super::client::expect_connected(&mut rx_a).await;
    super::client::expect_connected(&mut rx_b).await;

    // A opens two streams to B; handle calls that block run on scoped std
    // threads (block_on must not run on the test runtime).
    let handle_a = std::sync::Arc::new(handle_a);
    let handle_b = std::sync::Arc::new(handle_b);
    let (stream1, stream2) = std::thread::scope(|scope| {
        let a = &handle_a;
        let first = scope
            .spawn(|| a.open_stream(&node_b.fingerprint()).expect("stream 1"))
            .join()
            .expect("open thread");
        let second = scope
            .spawn(|| a.open_stream(&node_b.fingerprint()).expect("stream 2"))
            .join()
            .expect("open thread");
        (first, second)
    });
    // Accept strictly in open order: the first accept consumes the first
    // queued inbound stream. The blocking waits run on the blocking thread
    // pool: parking the test thread would freeze the relay (it shares this
    // single-threaded runtime).
    let b = handle_b.clone();
    let accept1_rx = spawn_accept(b);
    let accepted1 = await_accept(accept1_rx, "stream 1 inbound").await;
    let b = handle_b.clone();
    let accept2_rx = spawn_accept(b);
    let mut accepted2 = await_accept(accept2_rx, "stream 2 inbound").await;

    // B reads only stream 2: the full transfer completes.
    const TOTAL2: usize = 512 * 1024;
    let reader2 = tokio::spawn(async move {
        let mut received = 0usize;
        let mut buf = vec![0u8; 64 * 1024];
        while received < TOTAL2 {
            let n = timeout(NO_DEADLOCK, accepted2.read(&mut buf))
                .await
                .expect("stream 2 read deadline")
                .expect("stream 2 read");
            received += n;
        }
        received
    });
    // A writes on both streams; stream 1's writer must park once its
    // end-to-end credit (~256 KiB initial window, never granted because B
    // never reads it) is exhausted — while stream 2 flows to completion.
    let mut writer1 = stream1;
    let writer1_task = tokio::spawn(async move {
        let payload = vec![0xabu8; MAX_PAYLOAD_USIZE];
        for _ in 0..64 {
            writer1.write_all(&payload).await.expect("stream 1 write");
        }
        writer1
    });
    let mut writer2 = stream2;
    let payload2 = vec![0x5au8; MAX_PAYLOAD_USIZE];
    for _ in 0..(TOTAL2 / MAX_PAYLOAD_USIZE) {
        writer2.write_all(&payload2).await.expect("stream 2 write");
    }
    let received2 = timeout(NO_DEADLOCK, reader2)
        .await
        .expect("stream 2 transfer within deadline")
        .expect("reader task");
    assert_eq!(received2, TOTAL2, "stream 2 completes independently");

    // Stream 1 is still parked on credit: its writer has not finished the
    // 1 MiB it was asked for. (Reading stream 1 here would grant credit and
    // release it, so the parked check must come before any read of it.)
    let writer1_finished = timeout(Duration::from_millis(300), writer1_task).await;
    assert!(
        writer1_finished.is_err(),
        "stream 1's writer must stay parked on exhausted credit"
    );

    drop(accepted1);
    drop(writer2);
    for handle in [handle_a, handle_b] {
        match std::sync::Arc::try_unwrap(handle) {
            Ok(handle) => handle.cancel(),
            Err(_) => panic!("handle still shared at teardown"),
        }
    }
    server.server.shutdown().await;
}
