//! Routing end-to-end tests: two registered nodes, routed streams opened
//! via `OpenStream`, forwarding of Data/Window/half-close, structured
//! errors, and teardown on CloseStream or peer disconnect. The shared
//! harness lives in the parent `tests` module.

use super::*;

/// Opens a routed stream from `a` (stream 1) to `b` and returns b's
/// relay-allocated (even) stream id after asserting the forwarded open.
async fn open_routed_stream(
    link_a: &mut ClientTls,
    link_b: &mut ClientTls,
    a: &TestNode,
    b: &TestNode,
) -> u32 {
    write_frame(
        link_a,
        &Frame::OpenStream {
            stream_id: 1,
            target_node_id: b.fingerprint(),
        },
    )
    .await
    .expect("open should write");
    match read_link_frame(link_b).await {
        Frame::OpenStream {
            stream_id,
            target_node_id,
        } => {
            assert_eq!(
                stream_id % 2,
                0,
                "the relay allocates even stream ids on the target leg"
            );
            assert_eq!(stream_id, 2, "first relay-allocated id starts at 2");
            assert_eq!(
                target_node_id,
                a.fingerprint(),
                "the forwarded open names the requester"
            );
            stream_id
        }
        other => panic!("expected OpenStream, got {other:?}"),
    }
}

#[tokio::test]
async fn open_stream_routes_data_end_to_end() {
    let a = TestNode::generate();
    let b = TestNode::generate();
    let mut server = start_test_server_with(
        &[a.fingerprint(), b.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let mut link_a = register_node(&mut server, &a, &server_der).await;
    let mut link_b = register_node(&mut server, &b, &server_der).await;

    let b_stream = open_routed_stream(&mut link_a, &mut link_b, &a, &b).await;

    write_frame(
        &mut link_a,
        &Frame::Data {
            stream_id: 1,
            payload: b"ping".to_vec(),
        },
    )
    .await
    .expect("ping should write");
    assert_eq!(
        read_link_frame(&mut link_b).await,
        Frame::Data {
            stream_id: b_stream,
            payload: b"ping".to_vec(),
        }
    );

    write_frame(
        &mut link_b,
        &Frame::Data {
            stream_id: b_stream,
            payload: b"pong".to_vec(),
        },
    )
    .await
    .expect("pong should write");
    assert_eq!(
        read_link_frame(&mut link_a).await,
        Frame::Data {
            stream_id: 1,
            payload: b"pong".to_vec(),
        }
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn window_frames_forwarded() {
    let a = TestNode::generate();
    let b = TestNode::generate();
    let mut server = start_test_server_with(
        &[a.fingerprint(), b.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let mut link_a = register_node(&mut server, &a, &server_der).await;
    let mut link_b = register_node(&mut server, &b, &server_der).await;

    let b_stream = open_routed_stream(&mut link_a, &mut link_b, &a, &b).await;

    write_frame(
        &mut link_a,
        &Frame::Window {
            stream_id: 1,
            credit: 4096,
        },
    )
    .await
    .expect("window should write");
    assert_eq!(
        read_link_frame(&mut link_b).await,
        Frame::Window {
            stream_id: b_stream,
            credit: 4096,
        }
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn unknown_target_gets_structured_error() {
    let a = TestNode::generate();
    let b = TestNode::generate();
    let mut server = start_test_server_with(
        &[a.fingerprint(), b.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let mut link_a = register_node(&mut server, &a, &server_der).await;
    let _link_b = register_node(&mut server, &b, &server_der).await;

    write_frame(
        &mut link_a,
        &Frame::OpenStream {
            stream_id: 1,
            target_node_id: "deadbeef".to_string(),
        },
    )
    .await
    .expect("open should write");
    assert!(
        matches!(
            read_link_frame(&mut link_a).await,
            Frame::Error {
                stream_id: 1,
                code: error_code::TARGET_UNKNOWN,
                ..
            }
        ),
        "an unknown target must get TARGET_UNKNOWN"
    );

    // The failed open created no route: data on the stream id is unknown.
    write_frame(
        &mut link_a,
        &Frame::Data {
            stream_id: 1,
            payload: b"x".to_vec(),
        },
    )
    .await
    .expect("data should write");
    assert!(
        matches!(
            read_link_frame(&mut link_a).await,
            Frame::Error {
                stream_id: 1,
                code: error_code::STREAM_UNKNOWN,
                ..
            }
        ),
        "no route was created, so data gets STREAM_UNKNOWN"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn half_close_keeps_reverse_leg_then_both_close_drops() {
    let a = TestNode::generate();
    let b = TestNode::generate();
    let mut server = start_test_server_with(
        &[a.fingerprint(), b.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let mut link_a = register_node(&mut server, &a, &server_der).await;
    let mut link_b = register_node(&mut server, &b, &server_der).await;

    let b_stream = open_routed_stream(&mut link_a, &mut link_b, &a, &b).await;

    // a half-closes its write leg; b sees the close on its stream id.
    write_frame(&mut link_a, &Frame::Close { stream_id: 1 })
        .await
        .expect("close should write");
    assert_eq!(
        read_link_frame(&mut link_b).await,
        Frame::Close {
            stream_id: b_stream
        }
    );

    // The reverse leg stays open: b can still send data to a.
    write_frame(
        &mut link_b,
        &Frame::Data {
            stream_id: b_stream,
            payload: b"still open".to_vec(),
        },
    )
    .await
    .expect("reverse data should write");
    assert_eq!(
        read_link_frame(&mut link_a).await,
        Frame::Data {
            stream_id: 1,
            payload: b"still open".to_vec(),
        }
    );

    // b closes its leg too; both directions closed drops the pair.
    write_frame(
        &mut link_b,
        &Frame::Close {
            stream_id: b_stream,
        },
    )
    .await
    .expect("close should write");
    assert_eq!(
        read_link_frame(&mut link_a).await,
        Frame::Close { stream_id: 1 }
    );

    write_frame(
        &mut link_a,
        &Frame::Data {
            stream_id: 1,
            payload: b"x".to_vec(),
        },
    )
    .await
    .expect("data should write");
    assert!(
        matches!(
            read_link_frame(&mut link_a).await,
            Frame::Error {
                stream_id: 1,
                code: error_code::STREAM_UNKNOWN,
                ..
            }
        ),
        "a fully closed pair is dropped, so data gets STREAM_UNKNOWN"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn node_close_stream_tears_down_both_legs() {
    let a = TestNode::generate();
    let b = TestNode::generate();
    let mut server = start_test_server_with(
        &[a.fingerprint(), b.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let mut link_a = register_node(&mut server, &a, &server_der).await;
    let mut link_b = register_node(&mut server, &b, &server_der).await;

    let b_stream = open_routed_stream(&mut link_a, &mut link_b, &a, &b).await;

    write_frame(&mut link_a, &Frame::CloseStream { stream_id: 1 })
        .await
        .expect("close-stream should write");
    assert_eq!(
        read_link_frame(&mut link_b).await,
        Frame::CloseStream {
            stream_id: b_stream
        }
    );

    // Both legs are gone: b's data on the torn-down stream is unknown.
    write_frame(
        &mut link_b,
        &Frame::Data {
            stream_id: b_stream,
            payload: b"x".to_vec(),
        },
    )
    .await
    .expect("data should write");
    assert!(
        matches!(
            read_link_frame(&mut link_b).await,
            Frame::Error {
                stream_id,
                code: error_code::STREAM_UNKNOWN,
                ..
            } if stream_id == b_stream
        ),
        "CloseStream tears down both legs"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn peer_disconnect_sends_close_stream_and_drops_routes() {
    let a = TestNode::generate();
    let b = TestNode::generate();
    let mut server = start_test_server_with(
        &[a.fingerprint(), b.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let mut link_a = register_node(&mut server, &a, &server_der).await;
    let mut link_b = register_node(&mut server, &b, &server_der).await;

    open_routed_stream(&mut link_a, &mut link_b, &a, &b).await;
    drop(link_b);

    // The relay notifies a and drops the route.
    assert_eq!(
        read_link_frame(&mut link_a).await,
        Frame::CloseStream { stream_id: 1 },
        "a surviving peer hears CloseStream when the other link dies"
    );
    write_frame(
        &mut link_a,
        &Frame::Data {
            stream_id: 1,
            payload: b"x".to_vec(),
        },
    )
    .await
    .expect("data should write");
    assert!(
        matches!(
            read_link_frame(&mut link_a).await,
            Frame::Error {
                stream_id: 1,
                code: error_code::STREAM_UNKNOWN,
                ..
            }
        ),
        "the torn-down route answers with STREAM_UNKNOWN"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn even_open_stream_id_fails_link() {
    let a = TestNode::generate();
    let b = TestNode::generate();
    let mut server = start_test_server_with(
        &[a.fingerprint(), b.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let mut link_a = register_node(&mut server, &a, &server_der).await;
    active_connections_reaches(&server.server, 1).await;

    // Even stream ids are relay-initiated; a node claiming one is a
    // protocol violation and the link is torn down.
    write_frame(
        &mut link_a,
        &Frame::OpenStream {
            stream_id: 2,
            target_node_id: b.fingerprint(),
        },
    )
    .await
    .expect("open should write");
    expect_link_closed(&mut link_a).await;
    active_connections_reaches(&server.server, 0).await;
    server.server.shutdown().await;
}

#[tokio::test]
async fn self_target_routes() {
    let a = TestNode::generate();
    let mut server =
        start_test_server_with(&[a.fingerprint()], RelayLifecycleConfig::fast_for_tests()).await;
    let server_der = server_cert_der(&server.config);
    let mut link_a = register_node(&mut server, &a, &server_der).await;

    write_frame(
        &mut link_a,
        &Frame::OpenStream {
            stream_id: 1,
            target_node_id: a.fingerprint(),
        },
    )
    .await
    .expect("open should write");
    assert_eq!(
        read_link_frame(&mut link_a).await,
        Frame::OpenStream {
            stream_id: 2,
            target_node_id: a.fingerprint(),
        },
        "opening toward self is forwarded back on a fresh relay stream id"
    );

    write_frame(
        &mut link_a,
        &Frame::Data {
            stream_id: 1,
            payload: b"loop".to_vec(),
        },
    )
    .await
    .expect("data should write");
    assert_eq!(
        read_link_frame(&mut link_a).await,
        Frame::Data {
            stream_id: 2,
            payload: b"loop".to_vec(),
        }
    );
    server.server.shutdown().await;
}
