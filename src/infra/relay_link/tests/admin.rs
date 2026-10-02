//! Admin socket integration: status queries the connection table through
//! the owner-control socket, shutdown stops the relay gracefully, and the
//! TLS listener port is closed afterwards. The client uses the async stream:
//! these tests share a single-thread runtime with the relay, and blocking
//! std IO here would starve it.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

pub(super) async fn admin_request(
    addr: &crate::platform::remote_ipc::RemoteControlAddr,
    request: &str,
) -> String {
    let mut stream = crate::platform::remote_ipc::RemoteControlAsyncStream::connect(addr)
        .await
        .expect("admin socket should accept connections");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("request should write");
    stream
        .shutdown()
        .await
        .expect("write side should shut down");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("response should read");
    response
}

#[tokio::test]
async fn admin_status_lists_registered_nodes_and_shutdown_stops_relay() {
    let client_a = TestNode::generate();
    let client_b = TestNode::generate();
    let mut server = start_test_server_with(
        &[client_a.fingerprint(), client_b.fingerprint()],
        RelayLifecycleConfig::fast_for_tests(),
    )
    .await;
    let server_der = server_cert_der(&server.config);
    let admin_addr = server.server.admin_addr().clone();

    // Empty table first.
    let body = admin_request(&admin_addr, r#"{"command":"status"}"#).await;
    let status: serde_json::Value = serde_json::from_str(&body).expect("status should be json");
    assert_eq!(status["ok"], true);
    assert_eq!(status["registered_nodes"], 0);

    let _link_a = register_node(&mut server, &client_a, &server_der).await;
    let _link_b = register_node(&mut server, &client_b, &server_der).await;
    active_connections_reaches(&server.server, 2).await;

    let body = admin_request(&admin_addr, r#"{"command":"status"}"#).await;
    let status: serde_json::Value = serde_json::from_str(&body).expect("status should be json");
    assert_eq!(status["ok"], true);
    assert_eq!(status["registered_nodes"], 2);
    let node_ids: Vec<&str> = status["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|node| node["node_id"].as_str().expect("node id"))
        .collect();
    let mut expected = vec![client_a.fingerprint(), client_b.fingerprint()];
    expected.sort();
    let mut actual = node_ids
        .iter()
        .map(|id| (*id).to_string())
        .collect::<Vec<_>>();
    actual.sort();
    assert_eq!(actual, expected, "admin sees the registered fingerprints");

    // Graceful shutdown through the admin socket.
    let body = admin_request(&admin_addr, r#"{"command":"shutdown"}"#).await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("shutdown json");
    assert_eq!(response["ok"], true);

    let listen_addr = server.server.local_addr();
    timeout(NO_DEADLOCK, server.server.wait_until_stopped())
        .await
        .expect("relay should stop after admin shutdown");
    let connect = timeout(NO_DEADLOCK, tokio::net::TcpStream::connect(listen_addr)).await;
    assert!(
        matches!(connect, Ok(Err(_))),
        "TLS listener must close after admin shutdown, got: {connect:?}"
    );
}

#[tokio::test]
async fn admin_unknown_command_gets_a_structured_error() {
    let client = TestNode::generate();
    let server = start_test_server(&[client.fingerprint()]).await;
    let admin_addr = server.server.admin_addr().clone();

    let body = admin_request(&admin_addr, r#"{"command":"bogus"}"#).await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("error json");
    assert_eq!(response["ok"], false);
    assert!(response["error"]
        .as_str()
        .expect("error message")
        .contains("unknown admin command"));
    server.server.shutdown().await;
}
