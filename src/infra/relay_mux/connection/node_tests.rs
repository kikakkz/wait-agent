//! Node-to-node mode tests for [`MuxConnection`]: frame dispatch, stream
//! lifecycle, backpressure, and teardown over an in-memory duplex pair.
//! Kept in a child module so `connection.rs` stays under the file-length
//! cap; everything here reaches the internals through `super::*`.

use super::*;
use crate::infra::relay_mux::frame::write_frame;
use crate::infra::relay_mux::DEFAULT_STREAM_WINDOW;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const NO_DEADLOCK: Duration = Duration::from_secs(10);

fn pair(buffer: usize) -> (MuxConnection, MuxConnection) {
    let (a, b) = tokio::io::duplex(buffer);
    (
        MuxConnection::spawn(a, MuxRole::Client),
        MuxConnection::spawn(b, MuxRole::Server),
    )
}

#[tokio::test]
async fn open_accept_and_echo_both_directions() {
    let (client, mut server) = pair(4096);
    let mut client_stream = client.open_stream().await.expect("open");
    let mut server_stream = timeout(NO_DEADLOCK, server.accept())
        .await
        .expect("accept should not deadlock")
        .expect("server should accept");
    assert_eq!(server_stream.id(), client_stream.id());

    client_stream
        .write_all(b"ping")
        .await
        .expect("client write");
    let mut buf = [0u8; 4];
    timeout(NO_DEADLOCK, server_stream.read_exact(&mut buf))
        .await
        .expect("read should not deadlock")
        .expect("server read");
    assert_eq!(&buf, b"ping");

    server_stream
        .write_all(b"pong")
        .await
        .expect("server write");
    timeout(NO_DEADLOCK, client_stream.read_exact(&mut buf))
        .await
        .expect("read should not deadlock")
        .expect("client read");
    assert_eq!(&buf, b"pong");
}

#[tokio::test]
async fn stream_ids_follow_role_parity() {
    let (mut client, mut server) = pair(4096);
    let first = client.open_stream().await.expect("open first");
    let second = client.open_stream().await.expect("open second");
    assert_eq!(first.id(), 1, "client allocates odd ids");
    assert_eq!(second.id(), 3, "client ids step by 2");
    let accepted_first = server.accept().await.expect("accept first");
    let accepted_second = server.accept().await.expect("accept second");
    assert_eq!(accepted_first.id(), 1);
    assert_eq!(accepted_second.id(), 3);

    let server_stream = server.open_stream().await.expect("server open");
    assert_eq!(server_stream.id(), 2, "server allocates even ids");
    let accepted = client.accept().await.expect("client accept");
    assert_eq!(accepted.id(), 2);
}

#[tokio::test]
async fn half_close_drains_buffered_data_then_eof_and_reverse_leg_stays_open() {
    let (client, mut server) = pair(4096);
    let mut client_stream = client.open_stream().await.expect("open");
    let mut server_stream = server.accept().await.expect("accept");

    client_stream
        .write_all(b"before-close")
        .await
        .expect("client write");
    client_stream.shutdown().await.expect("client half-close");

    let mut received = Vec::new();
    timeout(NO_DEADLOCK, server_stream.read_to_end(&mut received))
        .await
        .expect("read should not deadlock")
        .expect("server read to eof");
    assert_eq!(received, b"before-close");

    // The peer's write leg is still open after our half-close.
    server_stream
        .write_all(b"after-peer-close")
        .await
        .expect("server can still write");
    let mut buf = [0u8; 16];
    client_stream
        .read_exact(&mut buf)
        .await
        .expect("client read on open leg");
    assert_eq!(&buf, b"after-peer-close");
}

#[tokio::test]
async fn write_after_shutdown_fails() {
    let (client, mut server) = pair(4096);
    let mut client_stream = client.open_stream().await.expect("open");
    let _server_stream = server.accept().await.expect("accept");
    client_stream.shutdown().await.expect("half-close");
    let error = client_stream
        .write_all(b"nope")
        .await
        .expect_err("write after half-close must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn window_backpressure_parks_sender_until_peer_reads() {
    let (client, mut server) = pair(64 * 1024);
    let client_stream = client.open_stream().await.expect("open");
    let mut server_stream = server.accept().await.expect("accept");

    let total = (DEFAULT_STREAM_WINDOW + 64 * 1024) as usize;
    let payload = vec![0xabu8; total];
    let mut writer = client_stream;
    let writer = tokio::spawn(async move {
        writer
            .write_all(&payload)
            .await
            .expect("write_all should finish once credit flows");
    });
    let mut writer = writer;

    // The sender exhausts the initial window and parks: without a reader
    // it must not complete on its own.
    let premature = timeout(Duration::from_millis(300), &mut writer).await;
    assert!(
        premature.is_err(),
        "sender must park once the initial window is exhausted"
    );

    // Once the peer reads, grants flow and the parked writer completes
    // with every byte delivered in order.
    let mut received = vec![0u8; total];
    timeout(NO_DEADLOCK, server_stream.read_exact(&mut received))
        .await
        .expect("read should not deadlock")
        .expect("server read all");
    assert!(received.iter().all(|byte| *byte == 0xab));

    timeout(NO_DEADLOCK, writer)
        .await
        .expect("writer should complete after grants")
        .expect("writer task");
}

#[tokio::test]
async fn concurrent_streams_keep_per_stream_order() {
    let (client, mut server) = pair(64 * 1024);
    const STREAMS: usize = 8;
    const CHUNKS: usize = 4;
    const CHUNK: usize = 16 * 1024;

    let mut client_streams = Vec::with_capacity(STREAMS);
    for _ in 0..STREAMS {
        client_streams.push(client.open_stream().await.expect("open"));
    }

    // Interleave writes across streams; each stream's payload is a
    // single repeated tag byte so any cross-stream corruption shows up
    // as a wrong tag.
    let mut writers = Vec::with_capacity(STREAMS);
    for (index, mut stream) in client_streams.into_iter().enumerate() {
        writers.push(tokio::spawn(async move {
            let tag = index as u8;
            for _ in 0..CHUNKS {
                stream
                    .write_all(&vec![tag; CHUNK])
                    .await
                    .expect("interleaved write");
            }
        }));
    }

    let mut accepted = Vec::with_capacity(STREAMS);
    for expected_id in [1u32, 3, 5, 7, 9, 11, 13, 15] {
        let stream = server.accept().await.expect("accept");
        assert_eq!(stream.id(), expected_id);
        accepted.push(stream);
    }

    let mut readers = Vec::with_capacity(STREAMS);
    for (index, mut stream) in accepted.into_iter().enumerate() {
        readers.push(tokio::spawn(async move {
            let mut payload = vec![0u8; CHUNKS * CHUNK];
            stream
                .read_exact(&mut payload)
                .await
                .expect("read full stream");
            let tag = index as u8;
            assert!(
                payload.iter().all(|byte| *byte == tag),
                "stream {index} must carry only its tag byte"
            );
        }));
    }

    for handle in writers {
        timeout(NO_DEADLOCK, handle)
            .await
            .expect("writer should finish")
            .expect("writer task");
    }
    for handle in readers {
        timeout(NO_DEADLOCK, handle)
            .await
            .expect("reader should finish")
            .expect("reader task");
    }
}

#[tokio::test]
async fn data_for_unknown_stream_resets_connection() {
    // The raw end acts as the server peer (even stream ids).
    let (raw, mux_io) = tokio::io::duplex(4096);
    let mut mux = MuxConnection::spawn(mux_io, MuxRole::Client);
    let mut raw = raw;

    write_frame(&mut raw, &Frame::Open { stream_id: 2 })
        .await
        .expect("raw open");
    let mut accepted = mux.accept().await.expect("accept raw-opened stream");
    write_frame(
        &mut raw,
        &Frame::Data {
            stream_id: 3,
            payload: b"stray".to_vec(),
        },
    )
    .await
    .expect("raw data for unknown stream");

    let mut buf = [0u8; 1];
    let error = timeout(NO_DEADLOCK, accepted.read(&mut buf))
        .await
        .expect("read should fail, not hang")
        .expect_err("live stream must observe the reset");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    assert!(mux.is_closed());
    assert!(
        mux.open_stream().await.is_err(),
        "opens must fail after the reset"
    );
}

#[tokio::test]
async fn open_with_local_parity_resets_connection() {
    // The raw end sends an odd id (client parity) to a client-role
    // connection: only the peer's parity is legal for peer opens.
    let (raw, mux_io) = tokio::io::duplex(4096);
    let mut mux = MuxConnection::spawn(mux_io, MuxRole::Client);
    let mut raw = raw;

    write_frame(&mut raw, &Frame::Open { stream_id: 3 })
        .await
        .expect("raw open with client parity");

    let accepted = timeout(NO_DEADLOCK, mux.accept())
        .await
        .expect("accept should not hang");
    assert!(
        accepted.is_none(),
        "a violating open must fail the connection"
    );
    assert!(mux.is_closed());
}

#[tokio::test]
async fn dropping_connection_unblocks_peer_streams() {
    let (client, mut server) = pair(4096);
    let mut client_stream = client.open_stream().await.expect("open");
    client_stream
        .write_all(b"payload")
        .await
        .expect("client write");
    let mut server_stream = server.accept().await.expect("accept");

    drop(client);
    drop(client_stream);

    // The peer must observe the connection end within the deadline —
    // either a clean EOF after draining or a reset — and never hang.
    let mut received = Vec::new();
    let outcome = timeout(NO_DEADLOCK, server_stream.read_to_end(&mut received)).await;
    let result = outcome.expect("peer read must not hang");
    if result.is_err() {
        assert_eq!(
            result.expect_err("reset path").kind(),
            std::io::ErrorKind::ConnectionReset
        );
    }
}
