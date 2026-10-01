//! Capacity admission integration: register and open_stream are refused
//! with structured codes at the configured limits, replacements re-admit at
//! the cap, and the admin status exposes capacity and usage.

use super::*;
use crate::infra::relay_capacity::RelayCapacityConfig;
use crate::infra::relay_routing::error_code;

async fn start_with_capacity(whitelist: &[String], capacity: RelayCapacityConfig) -> RunningServer {
    start_test_server_with_capacity(whitelist, RelayLifecycleConfig::fast_for_tests(), capacity)
        .await
}

#[tokio::test]
async fn register_beyond_max_nodes_gets_structured_refusal() {
    let client_a = TestNode::generate();
    let client_b = TestNode::generate();
    let mut server = start_with_capacity(
        &[client_a.fingerprint(), client_b.fingerprint()],
        RelayCapacityConfig {
            max_nodes: 1,
            ..RelayCapacityConfig::default()
        },
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let _link_a = register_node(&mut server, &client_a, &server_der).await;
    active_connections_reaches(&server.server, 1).await;

    // A second NEW node over the cap: structured refusal, link closed.
    let mut link_b = open_node_link(server.server.local_addr(), &client_b, &server_der)
        .await
        .expect("handshake should succeed");
    write_frame(
        &mut link_b,
        &Frame::Register {
            node_id: client_b.fingerprint(),
        },
    )
    .await
    .expect("register should write");
    match read_link_frame(&mut link_b).await {
        Frame::Error {
            stream_id: 0,
            code,
            message,
        } => {
            assert_eq!(code, error_code::NODE_CAPACITY, "{message}");
        }
        other => panic!("expected NODE_CAPACITY refusal, got {other:?}"),
    }
    expect_link_closed(&mut link_b).await;
    active_connections_reaches(&server.server, 1).await;
    server.server.shutdown().await;
}

#[tokio::test]
async fn replacement_re_admits_at_the_node_cap() {
    let client = TestNode::generate();
    let mut server = start_with_capacity(
        &[client.fingerprint()],
        RelayCapacityConfig {
            max_nodes: 1,
            ..RelayCapacityConfig::default()
        },
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let first = register_node(&mut server, &client, &server_der).await;
    drop(first);
    // Wait for the first link to die so the replacement races nothing.
    active_connections_reaches(&server.server, 0).await;

    let _second = register_node(&mut server, &client, &server_der).await;
    active_connections_reaches(&server.server, 1).await;
    server.server.shutdown().await;
}

#[tokio::test]
async fn open_stream_beyond_max_streams_gets_structured_refusal() {
    let client_a = TestNode::generate();
    let client_b = TestNode::generate();
    let mut server = start_with_capacity(
        &[client_a.fingerprint(), client_b.fingerprint()],
        RelayCapacityConfig {
            max_streams: 1,
            ..RelayCapacityConfig::default()
        },
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut link_a = register_node(&mut server, &client_a, &server_der).await;
    let mut link_b = register_node(&mut server, &client_b, &server_der).await;

    // First stream fits; second is refused with STREAM_CAPACITY.
    write_frame(
        &mut link_a,
        &Frame::OpenStream {
            stream_id: 1,
            target_node_id: client_b.fingerprint(),
        },
    )
    .await
    .expect("open should write");
    let opened = read_link_frame(&mut link_b).await;
    let peer_stream = match opened {
        Frame::OpenStream { stream_id, .. } => stream_id,
        other => panic!("expected OpenStream at b, got {other:?}"),
    };

    write_frame(
        &mut link_a,
        &Frame::OpenStream {
            stream_id: 3,
            target_node_id: client_b.fingerprint(),
        },
    )
    .await
    .expect("second open should write");
    match read_link_frame(&mut link_a).await {
        Frame::Error {
            stream_id: 3,
            code,
            message,
        } => {
            assert_eq!(code, error_code::STREAM_CAPACITY, "{message}");
        }
        other => panic!("expected STREAM_CAPACITY refusal, got {other:?}"),
    }

    // The refused stream left no route behind: data on it is unknown.
    write_frame(
        &mut link_a,
        &Frame::Data {
            stream_id: 3,
            payload: b"nope".to_vec(),
        },
    )
    .await
    .expect("data should write");
    match read_link_frame(&mut link_a).await {
        Frame::Error {
            stream_id: 3, code, ..
        } => assert_eq!(code, error_code::STREAM_UNKNOWN),
        other => panic!("expected STREAM_UNKNOWN, got {other:?}"),
    }

    // The admitted stream still routes.
    write_frame(
        &mut link_a,
        &Frame::Data {
            stream_id: 1,
            payload: b"ok".to_vec(),
        },
    )
    .await
    .expect("data should write");
    match read_link_frame(&mut link_b).await {
        Frame::Data { stream_id, payload } => {
            assert_eq!(stream_id, peer_stream);
            assert_eq!(payload, b"ok".to_vec());
        }
        other => panic!("expected Data at b, got {other:?}"),
    }
    drop((link_a, link_b));
    server.server.shutdown().await;
}

#[tokio::test]
async fn throughput_threshold_refuses_new_opens_after_metering_data() {
    let client_a = TestNode::generate();
    let client_b = TestNode::generate();
    let mut server = start_with_capacity(
        &[client_a.fingerprint(), client_b.fingerprint()],
        RelayCapacityConfig {
            max_throughput_bytes_per_sec: 8,
            ..RelayCapacityConfig::default()
        },
    )
    .await;
    let server_der = server_cert_der(&server.config);

    let mut link_a = register_node(&mut server, &client_a, &server_der).await;
    let mut link_b = register_node(&mut server, &client_b, &server_der).await;

    // First open is admitted (meter at zero).
    write_frame(
        &mut link_a,
        &Frame::OpenStream {
            stream_id: 1,
            target_node_id: client_b.fingerprint(),
        },
    )
    .await
    .expect("open should write");
    let _opened = read_link_frame(&mut link_b).await;

    // Forward more than the 8 bytes/s threshold inside the window.
    write_frame(
        &mut link_a,
        &Frame::Data {
            stream_id: 1,
            payload: vec![0u8; 64],
        },
    )
    .await
    .expect("data should write");
    let _at_b = read_link_frame(&mut link_b).await;

    // The next open is refused while the window is over the threshold.
    write_frame(
        &mut link_a,
        &Frame::OpenStream {
            stream_id: 3,
            target_node_id: client_b.fingerprint(),
        },
    )
    .await
    .expect("open should write");
    match read_link_frame(&mut link_a).await {
        Frame::Error {
            stream_id: 3,
            code,
            message,
        } => {
            assert_eq!(code, error_code::THROUGHPUT_EXCEEDED, "{message}");
        }
        other => panic!("expected THROUGHPUT_EXCEEDED refusal, got {other:?}"),
    }
    drop((link_a, link_b));
    server.server.shutdown().await;
}

#[tokio::test]
async fn admin_status_exposes_capacity_and_usage() {
    let client_a = TestNode::generate();
    let client_b = TestNode::generate();
    let capacity = RelayCapacityConfig {
        max_nodes: 7,
        max_streams: 9,
        max_throughput_bytes_per_sec: 4096,
    };
    let mut server = start_with_capacity(
        &[client_a.fingerprint(), client_b.fingerprint()],
        capacity.clone(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();

    let mut link_a = register_node(&mut server, &client_a, &server_der).await;
    let mut link_b = register_node(&mut server, &client_b, &server_der).await;
    write_frame(
        &mut link_a,
        &Frame::OpenStream {
            stream_id: 1,
            target_node_id: client_b.fingerprint(),
        },
    )
    .await
    .expect("open should write");
    let _opened = read_link_frame(&mut link_b).await;

    let body = admin::admin_request(&admin_addr, r#"{"command":"status"}"#).await;
    let status: serde_json::Value = serde_json::from_str(&body).expect("status json");
    assert_eq!(status["ok"], true);
    assert_eq!(status["capacity"]["max_nodes"], 7);
    assert_eq!(status["capacity"]["max_streams"], 9);
    assert_eq!(status["capacity"]["max_throughput_bytes_per_sec"], 4096);
    assert_eq!(status["usage"]["registered_nodes"], 2);
    assert_eq!(status["usage"]["active_streams"], 1);
    drop(link_a);
    server.server.shutdown().await;
}
