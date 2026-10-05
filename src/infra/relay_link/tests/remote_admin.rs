//! Node-channel admin integration (docs/relay-design.md 管理通道): a
//! registered node sends `Frame::AdminRequest` over its authenticated link
//! and the relay answers with the status snapshot on `Frame::AdminResponse`.
//! Read-only in this slice: write commands get a structured rejection, and
//! an admin request never substitutes for `Register`.

use super::*;

use crate::infra::relay_join::join_relay;
use crate::infra::relay_mux::frame::{write_frame, Frame};
use crate::infra::relay_routing::error_code::RelayErrorCode;

fn join_paths(dir: &std::path::Path) -> (NodeCredentialPaths, std::path::PathBuf) {
    (
        NodeCredentialPaths {
            key_path: dir.join("node.key"),
            cert_path: dir.join("node.crt"),
        },
        dir.join("relay.toml"),
    )
}

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
async fn shutdown_stays_local_only_over_the_channel() {
    let client = TestNode::generate();
    let mut server = start_test_server(&[client.fingerprint()]).await;
    let server_der = server_cert_der(&server.config);
    let mut link = register_node(&mut server, &client, &server_der).await;

    write_frame(
        &mut link,
        &Frame::AdminRequest {
            seq: 1,
            command: r#"{"command":"shutdown"}"#.to_string(),
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
        "shutdown must stay local-only: {body}"
    );
    assert!(
        response["error"]
            .as_str()
            .expect("error message")
            .contains("local admin socket"),
        "{body}"
    );
    // The relay is still serving.
    active_connections_reaches(&server.server, 1).await;

    server.server.shutdown().await;
}

#[tokio::test]
async fn invite_over_the_channel_mints_a_token_that_join_redeems() {
    let admin_node = TestNode::generate();
    let joining_node = TestNode::generate();
    let mut server = start_test_server(&[admin_node.fingerprint()]).await;
    let server_der = server_cert_der(&server.config);
    let mut link = register_node(&mut server, &admin_node, &server_der).await;

    // The dashboard's invite form asks for a one-time token over the link.
    write_frame(
        &mut link,
        &Frame::AdminRequest {
            seq: 11,
            command: r#"{"command":"invite"}"#.to_string(),
        },
    )
    .await
    .expect("invite request should write");
    let frame = read_link_frame(&mut link).await;
    let Frame::AdminResponse { seq, body } = frame else {
        panic!("expected AdminResponse, got {frame:?}");
    };
    assert_eq!(seq, 11);
    let response: serde_json::Value = serde_json::from_str(&body).expect("invite json");
    assert_eq!(response["ok"], true, "invite over the node channel: {body}");
    let message = response["message"].as_str().expect("a message");
    let token = message
        .lines()
        .find_map(|line| line.strip_prefix("token: "))
        .expect("the message carries the raw token");

    // The minted token redeems through the standard enrollment session.
    let dir = temp_dir("remote-admin-join");
    let (credentials, toml_path) = join_paths(&dir);
    joining_node.write_pem_files(&credentials);
    let address = format!("127.0.0.1:{}", server.server.local_addr().port());
    join_relay(&address, token, &credentials, &toml_path)
        .await
        .expect("the node-channel token must redeem via join");
    assert!(
        server
            .config
            .authorized_nodes_dir
            .join(joining_node.fingerprint())
            .is_file(),
        "invite over the channel must whitelist the joining node"
    );

    server.server.shutdown().await;
}

#[tokio::test]
async fn remove_over_the_channel_revokes_whitelist_entry_and_live_link() {
    let admin_node = TestNode::generate();
    let victim = TestNode::generate();
    let mut server = start_test_server(&[admin_node.fingerprint(), victim.fingerprint()]).await;
    let server_der = server_cert_der(&server.config);
    let mut admin_link = register_node(&mut server, &admin_node, &server_der).await;
    let mut victim_link = register_node(&mut server, &victim, &server_der).await;
    active_connections_reaches(&server.server, 2).await;

    // The dashboard resolves prefixes against the status snapshot and sends
    // the full fingerprint (same semantics as the local admin remove).
    let fingerprint = victim.fingerprint();
    write_frame(
        &mut admin_link,
        &Frame::AdminRequest {
            seq: 21,
            command: format!(r#"{{"command":"remove","fingerprint":"{fingerprint}"}}"#),
        },
    )
    .await
    .expect("remove request should write");
    let frame = read_link_frame(&mut admin_link).await;
    let Frame::AdminResponse { seq, body } = frame else {
        panic!("expected AdminResponse, got {frame:?}");
    };
    assert_eq!(seq, 21);
    let response: serde_json::Value = serde_json::from_str(&body).expect("remove json");
    assert_eq!(response["ok"], true, "remove over the node channel: {body}");
    assert!(
        response["message"]
            .as_str()
            .expect("message")
            .contains("removed"),
        "{body}"
    );

    // The whitelist entry is gone; the live link is told why, then closed.
    assert!(
        !server
            .config
            .authorized_nodes_dir
            .join(victim.fingerprint())
            .is_file(),
        "remove must delete the whitelist entry"
    );
    let frame = read_link_frame(&mut victim_link).await;
    assert!(
        matches!(
            frame,
            Frame::Error { stream_id: 0, code, .. }
                if RelayErrorCode::from_wire(code) == Some(RelayErrorCode::NodeRevoked)
        ),
        "the revoked link must be told why: {frame:?}"
    );
    expect_link_closed(&mut victim_link).await;
    active_connections_reaches(&server.server, 1).await;

    // A removed node fails new handshakes (the same whitelist gate).
    let error = connect_client(server.server.local_addr(), Some(&victim), &server_der)
        .await
        .expect_err("a removed node must fail new handshakes");
    assert!(!error.is_empty());

    drop(admin_link);
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
