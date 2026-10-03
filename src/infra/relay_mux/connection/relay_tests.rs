//! Relay-link mode tests for [`MuxConnection`]: relay-routed inbound opens,
//! `CloseStream`/stream-scoped `Error` teardown, connection-level error
//! forwarding to the control channel, and the relay-only outbound helpers.
//! The raw duplex end speaks frames directly, mirroring the node-to-node
//! tests' harness.

use super::*;
use crate::infra::relay_mux::frame::write_frame;
use crate::infra::relay_routing::error_code::RelayErrorCode;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const NO_DEADLOCK: Duration = Duration::from_secs(10);

fn raw_pair(
    buffer: usize,
) -> (
    MuxConnection,
    tokio::io::DuplexStream,
    mpsc::Receiver<Frame>,
) {
    let (raw, mux_io) = tokio::io::duplex(buffer);
    let (control_tx, control_rx) = mpsc::channel(16);
    let mux = MuxConnection::spawn_relay_link(mux_io, control_tx);
    (mux, raw, control_rx)
}

#[tokio::test]
async fn relay_open_stream_even_is_accepted() {
    let (mut mux, mut raw, _control_rx) = raw_pair(4096);
    write_frame(
        &mut raw,
        &Frame::OpenStream {
            stream_id: 2,
            target_node_id: "peer-node".to_string(),
        },
    )
    .await
    .expect("raw open_stream");

    let stream = timeout(NO_DEADLOCK, mux.accept())
        .await
        .expect("accept should not deadlock")
        .expect("mux should accept the relay-routed stream");
    assert_eq!(stream.id(), 2, "the relay allocates even ids for us");
    assert!(!mux.is_closed());
}

#[tokio::test]
async fn relay_open_stream_odd_fails_connection() {
    let (mut mux, mut raw, _control_rx) = raw_pair(4096);
    write_frame(
        &mut raw,
        &Frame::OpenStream {
            stream_id: 1,
            target_node_id: "misroute".to_string(),
        },
    )
    .await
    .expect("raw open_stream");

    let accepted = timeout(NO_DEADLOCK, mux.accept())
        .await
        .expect("accept should not hang");
    assert!(
        accepted.is_none(),
        "a misrouted odd OpenStream must fail the connection"
    );
    assert!(mux.is_closed());
}

#[tokio::test]
async fn relay_connection_error_forwards_to_control_and_stays_alive() {
    let (mut mux, mut raw, mut control_rx) = raw_pair(4096);
    write_frame(
        &mut raw,
        &Frame::Error {
            stream_id: 0,
            code: 0x0002,
            message: "registration refused".to_string(),
        },
    )
    .await
    .expect("raw error");

    let frame = timeout(NO_DEADLOCK, control_rx.recv())
        .await
        .expect("control frame should arrive")
        .expect("control channel should stay open");
    assert!(
        matches!(
            frame,
            Frame::Error {
                stream_id: 0,
                code: 0x0002,
                ..
            }
        ),
        "connection-level errors go to the control channel: {frame:?}"
    );
    assert!(
        !mux.is_closed(),
        "connection-level errors never kill the mux; the client decides"
    );

    // The link keeps serving streams afterwards.
    write_frame(
        &mut raw,
        &Frame::OpenStream {
            stream_id: 2,
            target_node_id: "peer-node".to_string(),
        },
    )
    .await
    .expect("raw open_stream");
    let accepted = timeout(NO_DEADLOCK, mux.accept())
        .await
        .expect("accept should not deadlock");
    assert!(accepted.is_some(), "the link must keep accepting streams");
}

#[tokio::test]
async fn relay_stream_error_resets_that_stream_only() {
    let (mux, mut raw, _control_rx) = raw_pair(4096);
    let mut stream = mux.open_stream_to("node-b").await.expect("open");
    let raw_open = timeout(NO_DEADLOCK, read_frame(&mut raw))
        .await
        .expect("open frame within deadline")
        .expect("open frame decodes");
    assert!(
        matches!(
            raw_open,
            Frame::OpenStream {
                stream_id: 1,
                ref target_node_id
            } if target_node_id == "node-b"
        ),
        "open_stream_to emits an odd-id OpenStream with the target: {raw_open:?}"
    );

    write_frame(
        &mut raw,
        &Frame::Error {
            stream_id: 1,
            code: 0x0001,
            message: "target unknown".to_string(),
        },
    )
    .await
    .expect("raw stream error");

    let mut buf = [0u8; 1];
    let error = timeout(NO_DEADLOCK, stream.read(&mut buf))
        .await
        .expect("read should fail, not hang")
        .expect_err("the refused stream must surface the reset");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    assert!(
        !mux.is_closed(),
        "a stream-scoped error must not kill the link"
    );
}

#[tokio::test]
async fn relay_stream_error_keeps_code_and_message() {
    let (mux, mut raw, _control_rx) = raw_pair(4096);
    let mut stream = mux.open_stream_to("node-b").await.expect("open");
    let _raw_open = timeout(NO_DEADLOCK, read_frame(&mut raw))
        .await
        .expect("open frame within deadline")
        .expect("open frame decodes");

    write_frame(
        &mut raw,
        &Frame::Error {
            stream_id: 1,
            code: RelayErrorCode::TargetUnknown.wire_value(),
            message: "target unknown".to_string(),
        },
    )
    .await
    .expect("raw stream error");

    let mut buf = [0u8; 1];
    let error = timeout(NO_DEADLOCK, stream.read(&mut buf))
        .await
        .expect("read should fail, not hang")
        .expect_err("the refused stream must surface the reset");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    let reset = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<MuxResetError>())
        .expect("the relay error code and message must ride the reset");
    assert_eq!(reset.code, RelayErrorCode::TargetUnknown.wire_value());
    assert_eq!(reset.message, "target unknown");
    assert!(
        !mux.is_closed(),
        "a stream-scoped error must not kill the link"
    );
}

#[tokio::test]
async fn relay_close_stream_keeps_the_plain_reset_error() {
    let (mux, mut raw, _control_rx) = raw_pair(4096);
    let mut stream = mux.open_stream_to("node-b").await.expect("open");
    let _raw_open = read_frame(&mut raw).await.expect("open frame decodes");

    write_frame(&mut raw, &Frame::CloseStream { stream_id: 1 })
        .await
        .expect("raw close_stream");

    let mut buf = [0u8; 1];
    let error = timeout(NO_DEADLOCK, stream.read(&mut buf))
        .await
        .expect("read should fail, not hang")
        .expect_err("CloseStream must reset the read leg");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    assert!(
        error
            .get_ref()
            .and_then(|source| source.downcast_ref::<MuxResetError>())
            .is_none(),
        "a CloseStream teardown carries no relay error payload: {error}"
    );
}

#[tokio::test]
async fn relay_close_stream_tears_down_both_legs() {
    let (mux, mut raw, _control_rx) = raw_pair(4096);
    let mut stream = mux.open_stream_to("node-b").await.expect("open");
    let _raw_open = read_frame(&mut raw).await.expect("open frame decodes");

    write_frame(&mut raw, &Frame::CloseStream { stream_id: 1 })
        .await
        .expect("raw close_stream");

    let mut buf = [0u8; 1];
    let error = timeout(NO_DEADLOCK, stream.read(&mut buf))
        .await
        .expect("read should fail, not hang")
        .expect_err("CloseStream must reset the read leg");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    let write_error = timeout(NO_DEADLOCK, stream.write_all(b"nope"))
        .await
        .expect("write should fail, not hang")
        .expect_err("CloseStream must reset the write leg");
    assert_eq!(write_error.kind(), std::io::ErrorKind::BrokenPipe);
    assert!(!mux.is_closed(), "the link survives a stream teardown");
}

#[tokio::test]
async fn relay_open_frame_fails_connection() {
    let (mut mux, mut raw, _control_rx) = raw_pair(4096);
    write_frame(&mut raw, &Frame::Open { stream_id: 2 })
        .await
        .expect("raw open");

    let accepted = timeout(NO_DEADLOCK, mux.accept())
        .await
        .expect("accept should not hang");
    assert!(
        accepted.is_none(),
        "a node-to-node Open on a relay link must fail the connection"
    );
    assert!(mux.is_closed());
}

#[tokio::test]
async fn send_control_rejects_stream_frames_and_sends_register_heartbeat() {
    let (mux, mut raw, _control_rx) = raw_pair(4096);
    let rejected = mux
        .send_control(Frame::Data {
            stream_id: 1,
            payload: b"nope".to_vec(),
        })
        .expect_err("Data is not a relay control frame");
    assert!(
        matches!(rejected, MuxError::ProtocolViolation(_)),
        "unexpected rejection: {rejected}"
    );

    mux.send_control(Frame::Register {
        node_id: "node-a".to_string(),
    })
    .expect("register should queue");
    let frame = timeout(NO_DEADLOCK, read_frame(&mut raw))
        .await
        .expect("register within deadline")
        .expect("register decodes");
    assert!(
        matches!(frame, Frame::Register { ref node_id } if node_id == "node-a"),
        "send_control emits the register frame: {frame:?}"
    );

    mux.send_control(Frame::Heartbeat)
        .expect("heartbeat should queue");
    let frame = timeout(NO_DEADLOCK, read_frame(&mut raw))
        .await
        .expect("heartbeat within deadline")
        .expect("heartbeat decodes");
    assert!(
        matches!(frame, Frame::Heartbeat),
        "send_control emits the heartbeat frame: {frame:?}"
    );
}

#[tokio::test]
async fn open_stream_to_rejects_oversize_target() {
    let (mux, _raw, _control_rx) = raw_pair(4096);
    let oversize = "x".repeat(MAX_NODE_ID_LEN as usize + 1);
    let error = mux
        .open_stream_to(&oversize)
        .await
        .expect_err("an oversize target must be refused locally");
    assert!(
        matches!(error, MuxError::ProtocolViolation(_)),
        "unexpected error: {error}"
    );
    assert!(!mux.is_closed(), "local validation must not kill the link");
}

#[tokio::test]
async fn relay_presence_forwards_to_control_and_stays_alive() {
    let (mux, mut raw, mut control_rx) = raw_pair(4096);
    write_frame(
        &mut raw,
        &Frame::Presence {
            node_id: "node-a".to_string(),
            online: true,
        },
    )
    .await
    .expect("raw presence");
    let frame = timeout(NO_DEADLOCK, control_rx.recv())
        .await
        .expect("control frame should arrive")
        .expect("control channel should stay open");
    assert!(
        matches!(
            frame,
            Frame::Presence { ref node_id, online: true } if node_id == "node-a"
        ),
        "presence transitions go to the control channel: {frame:?}"
    );
    assert!(
        !mux.is_closed(),
        "presence transitions never kill the mux; the client decides"
    );

    write_frame(
        &mut raw,
        &Frame::Presence {
            node_id: "node-a".to_string(),
            online: false,
        },
    )
    .await
    .expect("raw presence");
    let frame = timeout(NO_DEADLOCK, control_rx.recv())
        .await
        .expect("second control frame should arrive")
        .expect("control channel should stay open");
    assert!(
        matches!(frame, Frame::Presence { online: false, .. }),
        "later transitions keep flowing: {frame:?}"
    );
}

#[tokio::test]
async fn relay_watch_from_relay_fails_connection() {
    let (mut mux, mut raw, mut control_rx) = raw_pair(4096);
    write_frame(
        &mut raw,
        &Frame::Watch {
            node_id: "node-b".to_string(),
        },
    )
    .await
    .expect("raw watch");

    let accepted = timeout(NO_DEADLOCK, mux.accept())
        .await
        .expect("accept should not hang");
    assert!(
        accepted.is_none(),
        "Watch from the relay is an unexpected-direction frame and must fail the link"
    );
    assert!(mux.is_closed());
    assert!(
        control_rx.try_recv().is_err(),
        "nothing is forwarded for a violating frame"
    );
}

#[tokio::test]
async fn relay_helpers_fail_on_node_to_node_connections() {
    let (a, _b) = tokio::io::duplex(4096);
    let client = MuxConnection::spawn(a, MuxRole::Client);

    let error = client
        .open_stream_to("node-b")
        .await
        .expect_err("open_stream_to requires relay-link mode");
    assert!(
        matches!(error, MuxError::NotRelayLink),
        "unexpected error: {error}"
    );
    let error = client
        .send_control(Frame::Heartbeat)
        .expect_err("send_control requires relay-link mode");
    assert!(
        matches!(error, MuxError::NotRelayLink),
        "unexpected error: {error}"
    );
}
