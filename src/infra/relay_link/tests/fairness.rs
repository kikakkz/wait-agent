//! Fair scheduling and backpressure tests (issue #33): the per-link egress
//! scheduler must isolate a stalled stream from its peers (no head-of-line
//! blocking), interleave backlogged streams within the round budget, and let
//! control frames overtake bulk data. End-to-end stream credit (the nodes'
//! own window) stays the first backpressure layer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// Awaits the relay→`node_id` link's egress scheduler classifying
/// `additional` frames beyond `baseline` in total (issue #122).
/// Classification is the ordering point these tests assert on: any wire-level
/// rendezvous is blind to frames the kernel socket buffer hides, and a
/// control-channel bypass can even answer before a bulk frame is classified.
async fn await_classified(node_id: &str, baseline: u64, additional: u64) {
    timeout(NO_DEADLOCK, async {
        loop {
            if let Some(count) =
                crate::infra::relay_scheduler::test_registry::classified_frames(node_id).await
            {
                if count >= baseline + additional {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{additional} frames classified on {node_id} within deadline"));
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
    let scheduler_key = pair.target.fingerprint();

    // Pump the stalled stream well past one bucket, then the live stream's
    // marker. The drain runs concurrently: bulk classification only proceeds
    // while the stalled bucket empties, and the bucket only empties while
    // the target reads. The overtake assertion is anchored at the scheduler
    // (issue #122): the relay→target socket can silently hold frames in the
    // kernel buffer, so wire order alone cannot show scheduler fairness, and
    // a control-channel rendezvous can fire before the marker is even
    // classified. The drain therefore counts stalled bytes that arrive AFTER
    // the marker was classified (observed through the scheduler's test
    // registry); from that moment the deficit-round-robin scheduler must
    // egress the marker within roughly one stalled-bucket round — never
    // behind the backlog.
    const PUMP_FRAMES: usize = 512; // 8 MiB: well above the 1 MiB bucket
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

    // The two forwarded OpenStream frames classified before the pump; the
    // barrier counts relative to the pre-pump snapshot.
    let pre_pump = crate::infra::relay_scheduler::test_registry::classified_frames(&scheduler_key)
        .await
        .expect("target link scheduler registered");
    let barrier = Arc::new(AtomicBool::new(false));
    let drain = tokio::spawn({
        let barrier = barrier.clone();
        async move {
            let mut baseline = None;
            let mut stalled_bytes = 0usize;
            loop {
                if baseline.is_none() && barrier.load(Ordering::Relaxed) {
                    baseline = Some(stalled_bytes);
                }
                let frame = read_one(&mut pair.target_reader).await;
                match frame {
                    Frame::Data { stream_id, payload } if stream_id == stalled_target_id => {
                        stalled_bytes += payload.len()
                    }
                    Frame::Data { stream_id, payload } if stream_id == live_target_id => {
                        assert_eq!(payload, b"marker");
                        return (baseline.unwrap_or(stalled_bytes), stalled_bytes);
                    }
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        }
    });

    await_classified(&scheduler_key, pre_pump, PUMP_FRAMES as u64 + 1).await;
    barrier.store(true, Ordering::Relaxed);

    let (baseline, stalled_bytes) = drain.await.expect("drain");
    let post_classification_bytes = stalled_bytes - baseline;
    const BUCKET_BYTES: usize = 1024 * 1024;
    const ROUND_BYTES: usize = 64 * 1024;
    // A pre-classification frame still unread when the drain observes the
    // flag is counted post-barrier; the concurrent drain keeps the socket
    // buffer near empty, so a few frames cover it.
    const FLAG_CHECK_LAG_BYTES: usize = 8 * MAX_PAYLOAD_USIZE;
    assert!(
        post_classification_bytes <= BUCKET_BYTES + ROUND_BYTES + FLAG_CHECK_LAG_BYTES,
        "once classified, the marker must egress within one stalled-bucket round, not behind the backlog (saw {post_classification_bytes} bytes after classification)"
    );

    producer.await.expect("producer");
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
    let scheduler_key = pair.target.fingerprint();

    // Pump the stalled stream well past one bucket, then tear the second
    // stream down: the forwarded CloseStream classifies on the target link's
    // control channel and must egress before the stalled bulk still queued
    // in the scheduler (issue #105). Anchored at classification like
    // `no_head_of_line_blocking_for_a_stalled_stream` (issue #122): the
    // kernel socket buffer can hide stalled frames from any wire-level
    // assertion, so the bound below counts only bytes emitted after the
    // teardown classified.
    const PUMP_FRAMES: usize = 512; // 8 MiB: well above the 1 MiB bucket
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
        write_frame(&mut source_writer, &Frame::CloseStream { stream_id: 3 })
            .await
            .expect("close stream");
    });

    let pre_pump = crate::infra::relay_scheduler::test_registry::classified_frames(&scheduler_key)
        .await
        .expect("target link scheduler registered");
    let barrier = Arc::new(AtomicBool::new(false));
    let drain = tokio::spawn({
        let barrier = barrier.clone();
        async move {
            let mut baseline = None;
            let mut stalled_bytes = 0usize;
            loop {
                if baseline.is_none() && barrier.load(Ordering::Relaxed) {
                    baseline = Some(stalled_bytes);
                }
                let frame = read_one(&mut pair.target_reader).await;
                match frame {
                    Frame::Data { stream_id, payload } if stream_id == stalled_target_id => {
                        stalled_bytes += payload.len()
                    }
                    Frame::CloseStream { stream_id } if stream_id == closing_target_id => {
                        return (baseline.unwrap_or(stalled_bytes), stalled_bytes)
                    }
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        }
    });

    await_classified(&scheduler_key, pre_pump, PUMP_FRAMES as u64 + 1).await;
    barrier.store(true, Ordering::Relaxed);

    let (baseline, stalled_bytes) = drain.await.expect("drain");
    let post_classification_bytes = stalled_bytes - baseline;
    // A control frame egresses from the priority queue, ahead of every
    // queued bulk frame; only the flag-check lag can credit it with stalled
    // bytes.
    const FLAG_CHECK_LAG_BYTES: usize = 8 * MAX_PAYLOAD_USIZE;
    assert!(
        post_classification_bytes <= FLAG_CHECK_LAG_BYTES,
        "once classified, the teardown must egress ahead of the scheduler's bulk backlog (saw {post_classification_bytes} bytes after classification)"
    );

    producer.await.expect("producer");
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
    let scheduler_key = pair.target.fingerprint();

    // Pump stream 1 well past its 1 MiB bucket cap, then tear that same
    // stream down: the forwarded CloseStream reaches the target link's
    // scheduler behind a parked bulk arm and must classify via the control
    // channel instead of queueing behind the stalled stream (issue #105).
    // Anchored at classification like the sibling tests (issue #122): only
    // bytes emitted after the teardown classified may be counted against
    // the bypass, never the kernel-buffered backlog.
    const PUMP_FRAMES: usize = 512; // 8 MiB: well above the 1 MiB bucket
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

    let pre_pump = crate::infra::relay_scheduler::test_registry::classified_frames(&scheduler_key)
        .await
        .expect("target link scheduler registered");
    let barrier = Arc::new(AtomicBool::new(false));
    let drain = tokio::spawn({
        let barrier = barrier.clone();
        async move {
            let mut baseline = None;
            let mut stalled_bytes = 0usize;
            loop {
                if baseline.is_none() && barrier.load(Ordering::Relaxed) {
                    baseline = Some(stalled_bytes);
                }
                let frame = read_one(&mut pair.target_reader).await;
                match frame {
                    Frame::Data { stream_id, payload } if stream_id == stalled_target_id => {
                        stalled_bytes += payload.len()
                    }
                    Frame::CloseStream { stream_id } if stream_id == stalled_target_id => {
                        return (baseline.unwrap_or(stalled_bytes), stalled_bytes)
                    }
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        }
    });

    await_classified(&scheduler_key, pre_pump, PUMP_FRAMES as u64 + 1).await;
    barrier.store(true, Ordering::Relaxed);

    // The target keeps reading the wire but drops stream 1's bytes (its
    // consumer is absent): the teardown must still arrive, ahead of the
    // bulk still queued in the scheduler.
    let (baseline, stalled_bytes) = drain.await.expect("drain");
    let post_classification_bytes = stalled_bytes - baseline;
    const FLAG_CHECK_LAG_BYTES: usize = 8 * MAX_PAYLOAD_USIZE;
    assert!(
        post_classification_bytes <= FLAG_CHECK_LAG_BYTES,
        "once classified, the teardown must bypass the stalled stream's queued bulk (saw {post_classification_bytes} bytes after classification)"
    );

    producer.await.expect("producer");
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
