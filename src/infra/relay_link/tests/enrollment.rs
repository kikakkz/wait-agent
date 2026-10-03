//! Enrollment listener end-to-end tests: the token-authenticated join
//! session on `port + 1` (docs/relay-design.md 身份认证与入网). invite is
//! minted over the real admin socket, the joining node presents any client
//! certificate, and a valid token gets it whitelisted plus the relay
//! fingerprint in exchange. These tests share a single-thread runtime with
//! the relay: no blocking std IO here.

use super::admin::admin_request;
use super::*;

/// Mints an invite token over the admin socket and returns the raw token.
async fn invite_token(admin_addr: &crate::platform::remote_ipc::RemoteControlAddr) -> String {
    let body = admin_request(admin_addr, r#"{"command":"invite"}"#).await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("invite json");
    assert_eq!(response["ok"], true, "invite should succeed: {body}");
    let message = response["message"].as_str().expect("invite message");
    message
        .lines()
        .find_map(|line| line.strip_prefix("token: "))
        .expect("invite message carries the token")
        .to_string()
}

async fn enroll_once(
    server: &RunningServer,
    client: &TestNode,
    server_der: &[u8],
    token: &str,
) -> Frame {
    let mut link = open_node_link(server.server.enroll_local_addr(), client, server_der)
        .await
        .expect("enrollment handshake should succeed (any client cert)");
    write_frame(
        &mut link,
        &Frame::Enroll {
            token: token.to_string(),
        },
    )
    .await
    .expect("enroll frame should write");
    read_link_frame(&mut link).await
}

#[tokio::test]
async fn enroll_with_valid_token_whitelists_node_and_returns_relay_fingerprint() {
    let node = TestNode::generate();
    let mut server = start_test_server(&[]).await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();
    let relay_fingerprint = node_credentials::cert_fingerprint_from_der(&server_der)
        .expect("relay fingerprint should compute");

    let token = invite_token(&admin_addr).await;
    let response = enroll_once(&server, &node, &server_der, &token).await;
    assert_eq!(
        response,
        Frame::EnrollResponse {
            fingerprint: relay_fingerprint
        }
    );

    // The node is whitelisted on disk with its certificate PEM.
    let entry = server.config.authorized_nodes_dir.join(node.fingerprint());
    let pem = fs::read_to_string(&entry).expect("whitelist entry should exist");
    assert!(
        pem.starts_with("-----BEGIN CERTIFICATE-----"),
        "entry should hold the node certificate PEM"
    );

    // And can now register on the main listener.
    let mut link = register_node(&mut server, &node, &server_der).await;
    active_connections_reaches(&server.server, 1).await;
    write_frame(&mut link, &Frame::Unregister)
        .await
        .expect("unregister should write");
    server.server.shutdown().await;
}

#[tokio::test]
async fn one_time_token_redeems_once_then_reports_invalid() {
    let node = TestNode::generate();
    let server = start_test_server(&[]).await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();

    let token = invite_token(&admin_addr).await;
    let first = enroll_once(&server, &node, &server_der, &token).await;
    assert!(
        matches!(first, Frame::EnrollResponse { .. }),
        "first redeem should succeed, got {first:?}"
    );
    let second = enroll_once(&server, &node, &server_der, &token).await;
    assert!(
        matches!(
            second,
            Frame::Error { code, .. }
                if RelayErrorCode::from_wire(code) == Some(RelayErrorCode::TokenInvalid)
        ),
        "reused one-time token must be rejected, got {second:?}"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn unknown_token_reports_invalid_and_does_not_whitelist() {
    let node = TestNode::generate();
    let server = start_test_server(&[]).await;
    let server_der = server_cert_der(&server.config);

    let response = enroll_once(&server, &node, &server_der, "bogus-token").await;
    assert!(
        matches!(
            response,
            Frame::Error { code, .. }
                if RelayErrorCode::from_wire(code) == Some(RelayErrorCode::TokenInvalid)
        ),
        "unknown token must be rejected, got {response:?}"
    );
    assert!(
        !server
            .config
            .authorized_nodes_dir
            .join(node.fingerprint())
            .exists(),
        "a rejected token must not whitelist the node"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn expired_token_reports_expired() {
    let node = TestNode::generate();
    let server = start_test_server(&[]).await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();

    // ttl_secs 0 expires the token immediately.
    let body = admin_request(&admin_addr, r#"{"command":"invite","ttl_secs":0}"#).await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("invite json");
    assert_eq!(response["ok"], true, "invite should succeed: {body}");
    let message = response["message"].as_str().expect("invite message");
    let token = message
        .lines()
        .find_map(|line| line.strip_prefix("token: "))
        .expect("invite message carries the token");

    let response = enroll_once(&server, &node, &server_der, token).await;
    assert!(
        matches!(
            response,
            Frame::Error { code, .. }
                if RelayErrorCode::from_wire(code) == Some(RelayErrorCode::TokenExpired)
        ),
        "expired token must be reported as such, got {response:?}"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn deploy_token_redeems_multiple_times() {
    let first_node = TestNode::generate();
    let second_node = TestNode::generate();
    let server = start_test_server(&[]).await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();

    let body = admin_request(&admin_addr, r#"{"command":"invite","deploy":true}"#).await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("invite json");
    assert_eq!(response["ok"], true, "deploy invite should succeed: {body}");
    let message = response["message"].as_str().expect("invite message");
    assert!(
        message.contains("deploy token"),
        "message should name the deploy kind: {message}"
    );
    let token = message
        .lines()
        .find_map(|line| line.strip_prefix("token: "))
        .expect("invite message carries the token");

    for node in [&first_node, &second_node] {
        let response = enroll_once(&server, node, &server_der, token).await;
        assert!(
            matches!(response, Frame::EnrollResponse { .. }),
            "deploy token should redeem repeatedly, got {response:?}"
        );
    }
    server.server.shutdown().await;
}

#[tokio::test]
async fn first_frame_must_be_enroll() {
    let node = TestNode::generate();
    let server = start_test_server(&[]).await;
    let server_der = server_cert_der(&server.config);

    let mut link = open_node_link(server.server.enroll_local_addr(), &node, &server_der)
        .await
        .expect("enrollment handshake should succeed");
    write_frame(&mut link, &Frame::Heartbeat)
        .await
        .expect("frame should write");
    let response = read_link_frame(&mut link).await;
    assert!(
        matches!(
            response,
            Frame::Error { code, .. }
                if RelayErrorCode::from_wire(code) == Some(RelayErrorCode::TokenInvalid)
        ),
        "a non-Enroll first frame must be rejected, got {response:?}"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn admin_remove_revokes_whitelist_and_drops_live_link() {
    let node = TestNode::generate();
    let mut server = start_test_server_with(
        &[node.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();

    let mut link = register_node(&mut server, &node, &server_der).await;
    active_connections_reaches(&server.server, 1).await;

    let fingerprint = node.fingerprint();
    let body = admin_request(
        &admin_addr,
        &format!(r#"{{"command":"remove","fingerprint":"{fingerprint}"}}"#),
    )
    .await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("remove json");
    assert_eq!(response["ok"], true, "remove should succeed: {body}");
    assert!(
        response["message"]
            .as_str()
            .expect("remove message")
            .contains("live link dropped"),
        "the live link must be dropped: {body}"
    );
    assert!(
        !server
            .config
            .authorized_nodes_dir
            .join(&fingerprint)
            .exists(),
        "the whitelist entry must be gone"
    );
    // Revocation now tells the kicked link why before closing it; consume
    // the notice, then the link must be gone.
    let frame = read_link_frame(&mut link).await;
    assert!(
        matches!(
            frame,
            Frame::Error { stream_id: 0, code, .. }
                if RelayErrorCode::from_wire(code) == Some(RelayErrorCode::NodeRevoked)
        ),
        "the kicked link must be told it was revoked, got {frame:?}"
    );
    expect_link_closed(&mut link).await;
    active_connections_reaches(&server.server, 0).await;

    // A new handshake now fails at the transport layer.
    let error = connect_client(server.server.local_addr(), Some(&node), &server_der)
        .await
        .expect_err("a revoked node must fail new handshakes");
    assert!(!error.is_empty());
    server.server.shutdown().await;
}

#[tokio::test]
async fn admin_remove_notifies_kicked_link_before_close() {
    let node = TestNode::generate();
    let mut server = start_test_server_with(
        &[node.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();

    let mut link = register_node(&mut server, &node, &server_der).await;
    active_connections_reaches(&server.server, 1).await;

    let fingerprint = node.fingerprint();
    let body = admin_request(
        &admin_addr,
        &format!(r#"{{"command":"remove","fingerprint":"{fingerprint}"}}"#),
    )
    .await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("remove json");
    assert_eq!(response["ok"], true, "remove should succeed: {body}");
    assert!(
        response["message"]
            .as_str()
            .expect("remove message")
            .contains("live link dropped"),
        "the live link must be dropped: {body}"
    );
    assert!(
        !server
            .config
            .authorized_nodes_dir
            .join(&fingerprint)
            .exists(),
        "the whitelist entry must be gone"
    );

    // The kicked link learns why before it dies: the NODE_REVOKED error
    // (issue #36) precedes the teardown close.
    let frame = read_link_frame(&mut link).await;
    assert!(
        matches!(
            frame,
            Frame::Error { stream_id: 0, code, .. }
                if RelayErrorCode::from_wire(code) == Some(RelayErrorCode::NodeRevoked)
        ),
        "the kicked link must be told it was revoked, got {frame:?}"
    );
    expect_link_closed(&mut link).await;
    active_connections_reaches(&server.server, 0).await;
    server.server.shutdown().await;
}
