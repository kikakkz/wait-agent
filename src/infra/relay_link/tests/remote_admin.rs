//! Node-channel admin integration (docs/relay-design.md 管理通道): a
//! registered node sends `Frame::AdminRequest` over its authenticated link
//! and the relay answers with the status snapshot on `Frame::AdminResponse`.
//! Read-only in this slice: write commands get a structured rejection, and
//! an admin request never substitutes for `Register`.

use super::admin::admin_request;
use super::*;

use crate::infra::relay_mux::frame::{write_frame, Frame};

#[tokio::test]
async fn registered_node_requests_status_over_its_link() {
    let client = TestNode::generate();
    let mut server = start_test_server(&[client.fingerprint()]).await;
    let server_der = server_cert_der(&server.config);
    let mut link = register_node(&mut server, &client, &server_der).await;
    active_connections_reaches(&server.server, 1).await;

    write_frame(
        &mut link,
        &Frame::AdminRequest {
            seq: 7,
            command: r#"{"command":"status"}"#.to_string(),
        },
    )
    .await
    .expect("admin request should write");
    let frame = read_link_frame(&mut link).await;
    let Frame::AdminResponse { seq, body } = frame else {
        panic!("expected AdminResponse, got {frame:?}");
    };
    assert_eq!(seq, 7, "the response correlates by sequence number");
    let response: serde_json::Value = serde_json::from_str(&body).expect("status should be json");
    assert_eq!(response["ok"], true, "status should succeed: {body}");
    let status = &response["status"];
    assert_eq!(
        status["listen"].as_str(),
        Some(server.server.local_addr().to_string().as_str())
    );
    assert!(
        status["uptime_ms"].as_u64().is_some(),
        "uptime rides the node channel: {body}"
    );
    assert_eq!(status["registered_nodes"], 1);
    let node_ids: Vec<&str> = status["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|node| node["node_id"].as_str().expect("node id"))
        .collect();
    assert_eq!(node_ids, vec![client.fingerprint().as_str()]);
    assert!(
        status["nodes"][0]["idle_ms"].is_u64(),
        "idle rides per node: {body}"
    );
    assert!(status["capacity"]["max_nodes"].is_u64(), "{body}");
    assert_eq!(status["usage"]["registered_nodes"], 1, "{body}");

    // The link stays up and serves a second, differently-sequenced request.
    write_frame(
        &mut link,
        &Frame::AdminRequest {
            seq: 8,
            command: r#"{"command":"status"}"#.to_string(),
        },
    )
    .await
    .expect("second admin request should write");
    let frame = read_link_frame(&mut link).await;
    let Frame::AdminResponse { seq, body } = frame else {
        panic!("expected AdminResponse, got {frame:?}");
    };
    assert_eq!(seq, 8);
    let response: serde_json::Value = serde_json::from_str(&body).expect("status should be json");
    assert_eq!(response["status"]["registered_nodes"], 1);

    server.server.shutdown().await;
}

#[tokio::test]
async fn write_commands_get_a_structured_read_only_rejection() {
    let client = TestNode::generate();
    let mut server = start_test_server(&[client.fingerprint()]).await;
    let server_der = server_cert_der(&server.config);
    let mut link = register_node(&mut server, &client, &server_der).await;

    write_frame(
        &mut link,
        &Frame::AdminRequest {
            seq: 1,
            command: r#"{"command":"invite"}"#.to_string(),
        },
    )
    .await
    .expect("request should write");
    let frame = read_link_frame(&mut link).await;
    let Frame::AdminResponse { seq, body } = frame else {
        panic!("expected AdminResponse, got {frame:?}");
    };
    assert_eq!(seq, 1);
    let response: serde_json::Value = serde_json::from_str(&body).expect("envelope should be json");
    assert_eq!(
        response["ok"], false,
        "invite must be refused over the node channel: {body}"
    );
    assert!(
        response["error"]
            .as_str()
            .expect("error message")
            .contains("write commands"),
        "{body}"
    );

    // The refusal did not mint anything: the local admin invite flow is
    // untouched (token store still answers a real invite).
    let admin_addr = server.server.admin_addr().clone();
    let body = admin_request(&admin_addr, r#"{"command":"invite"}"#).await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("invite json");
    assert_eq!(
        response["ok"], true,
        "the local admin channel still mints: {body}"
    );

    server.server.shutdown().await;
}

#[tokio::test]
async fn admin_request_before_register_closes_the_link() {
    let client = TestNode::generate();
    let server = start_test_server(&[client.fingerprint()]).await;
    let server_der = server_cert_der(&server.config);

    let mut link = open_node_link(server.server.local_addr(), &client, &server_der)
        .await
        .expect("handshake should succeed");
    write_frame(
        &mut link,
        &Frame::AdminRequest {
            seq: 1,
            command: r#"{"command":"status"}"#.to_string(),
        },
    )
    .await
    .expect("frame should write");
    expect_link_closed(&mut link).await;
    active_connections_reaches(&server.server, 0).await;
    server.server.shutdown().await;
}
