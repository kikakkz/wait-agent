//! Relay client integration tests: the node-side persistent link (issue #32,
//! step 1) against the real relay server over loopback TLS — registration,
//! heartbeat liveness, reconnect/re-register after the server restarts, and
//! retry behavior on a wrong fingerprint pin.

use super::admin::admin_request;
use super::*;
use crate::infra::relay_client::{
    RelayClient, RelayClientConfig, RelayClientEvent, RelayRetryPolicy,
};
use crate::infra::relay_toml_store::RelayTomlConfig;
use crate::platform::remote_ipc::RemoteControlAddr;

pub(super) fn fast_retry() -> RelayRetryPolicy {
    RelayRetryPolicy {
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(50),
    }
}

pub(super) fn relay_client_config(
    addr: SocketAddr,
    relay_fingerprint: String,
    credentials: NodeCredentialPaths,
    heartbeat_interval: Duration,
) -> RelayClientConfig {
    RelayClientConfig {
        relay: RelayTomlConfig {
            address: addr.to_string(),
            relay_fingerprint,
            heartbeat_interval_secs: None,
        },
        credentials,
        heartbeat_interval,
        retry: fast_retry(),
    }
}

pub(super) fn node_credentials_in(dir: &std::path::Path, node: &TestNode) -> NodeCredentialPaths {
    let credentials = NodeCredentialPaths {
        key_path: dir.join("node.key"),
        cert_path: dir.join("node.crt"),
    };
    node.write_pem_files(&credentials);
    credentials
}

pub(super) fn relay_fingerprint(server: &RunningServer) -> String {
    let server_der = server_cert_der(&server.config);
    node_credentials::cert_fingerprint_from_der(&server_der).expect("relay fingerprint")
}

pub(super) async fn next_client_event(
    rx: &mut mpsc::Receiver<RelayClientEvent>,
) -> RelayClientEvent {
    timeout(NO_DEADLOCK, rx.recv())
        .await
        .expect("client event should arrive within the deadline")
        .expect("client event stream should stay open")
}

/// Skips `Connecting`/`Disconnected` retry cycles until `Connected` arrives;
/// a client that never connects fails the deadline in `next_client_event`.
pub(super) async fn expect_connected(
    rx: &mut mpsc::Receiver<RelayClientEvent>,
) -> RelayClientEvent {
    loop {
        match next_client_event(rx).await {
            event @ RelayClientEvent::Connected { .. } => return event,
            RelayClientEvent::Connecting { .. }
            | RelayClientEvent::Disconnected { .. }
            | RelayClientEvent::Presence { .. } => continue,
        }
    }
}

pub(super) async fn expect_disconnected(rx: &mut mpsc::Receiver<RelayClientEvent>) -> String {
    loop {
        match next_client_event(rx).await {
            RelayClientEvent::Disconnected { reason, .. } => return reason,
            RelayClientEvent::Connecting { .. } | RelayClientEvent::Presence { .. } => continue,
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }
}

/// After `cancel` the client thread has exited, so the event stream must
/// close; a trailing `Disconnected` queued before the stop is also fine.
pub(super) async fn expect_clean_stop(mut rx: mpsc::Receiver<RelayClientEvent>) {
    loop {
        match timeout(NO_DEADLOCK, rx.recv()).await {
            Ok(Some(RelayClientEvent::Disconnected { .. })) => continue,
            Ok(Some(other)) => panic!("unexpected event after cancel: {other:?}"),
            Ok(None) => return,
            Err(_) => panic!("event stream should close after cancel"),
        }
    }
}

async fn admin_lists_node(admin_addr: &RemoteControlAddr, fingerprint: &str) -> bool {
    let body = admin_request(admin_addr, r#"{"command":"status"}"#).await;
    let status: serde_json::Value = serde_json::from_str(&body).expect("status should be json");
    status["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .any(|node| node["node_id"].as_str() == Some(fingerprint))
        })
        .unwrap_or(false)
}

pub(super) async fn wait_admin_lists_node(admin_addr: &RemoteControlAddr, fingerprint: &str) {
    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    while !admin_lists_node(admin_addr, fingerprint).await {
        assert!(
            std::time::Instant::now() < deadline,
            "node {fingerprint} should appear in relay admin status"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_admin_lists_node(admin_addr: &RemoteControlAddr, fingerprint: &str) {
    assert!(
        admin_lists_node(admin_addr, fingerprint).await,
        "node {fingerprint} should be registered"
    );
}

async fn expect_evicted(events: &mut mpsc::Receiver<RelayLifecycleEvent>, fingerprint: &str) {
    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "silent node {fingerprint} should be evicted"
        );
        match timeout(Duration::from_millis(500), events.recv()).await {
            Ok(Some(RelayLifecycleEvent::EvictedOffline { node_id })) if node_id == fingerprint => {
                return;
            }
            Ok(Some(_)) => continue,
            other => panic!("expected EvictedOffline for {fingerprint}, got {other:?}"),
        }
    }
}

pub(super) fn long_lived_lifecycle() -> RelayLifecycleConfig {
    RelayLifecycleConfig {
        offline_after: Duration::from_secs(300),
        ..RelayLifecycleConfig::default()
    }
}

#[tokio::test]
async fn client_registers_and_stays_connected() {
    install_provider();
    let node = TestNode::generate();
    let server = start_test_server_with(&[node.fingerprint()], long_lived_lifecycle()).await;
    let relay_fingerprint = relay_fingerprint(&server);
    let admin_addr = server.server.admin_addr().clone();
    let dir = temp_dir("relay-client-basic");
    let credentials = node_credentials_in(&dir, &node);

    let (event_tx, mut event_rx) = mpsc::channel(16);
    let handle = RelayClient::spawn(
        relay_client_config(
            server.server.local_addr(),
            relay_fingerprint,
            credentials,
            Duration::from_millis(50),
        ),
        event_tx,
    );

    let connected = expect_connected(&mut event_rx).await;
    assert!(
        matches!(connected, RelayClientEvent::Connected { ref relay_address } if relay_address == &server.server.local_addr().to_string()),
        "Connected must carry the relay address"
    );
    wait_admin_lists_node(&admin_addr, &node.fingerprint()).await;

    // Heartbeats keep the registration alive past the keepalive probe.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_admin_lists_node(&admin_addr, &node.fingerprint()).await;

    handle.cancel();
    expect_clean_stop(event_rx).await;
    server.server.shutdown().await;
}

#[tokio::test]
async fn client_reconnects_and_reregisters_after_server_restart() {
    install_provider();
    let node = TestNode::generate();
    let server = start_test_server_with(&[node.fingerprint()], long_lived_lifecycle()).await;
    let relay_fingerprint = relay_fingerprint(&server);
    let admin_addr = server.server.admin_addr().clone();
    let first_addr = server.server.local_addr();
    let dir = temp_dir("relay-client-restart");
    let credentials = node_credentials_in(&dir, &node);

    let (event_tx, mut event_rx) = mpsc::channel(16);
    let handle = RelayClient::spawn(
        relay_client_config(
            first_addr,
            relay_fingerprint,
            credentials,
            Duration::from_millis(50),
        ),
        event_tx,
    );
    expect_connected(&mut event_rx).await;
    wait_admin_lists_node(&admin_addr, &node.fingerprint()).await;

    // Simulate the process restart: retire the live link (a plain graceful
    // shutdown leaves link tasks alive — they observe only table retirement,
    // not the shutdown watch), stop the listener, then bring a fresh server
    // up on the same address with the same relay identity.
    let body = admin_request(
        &admin_addr,
        &format!(
            r#"{{"command":"remove","fingerprint":"{}"}}"#,
            node.fingerprint()
        ),
    )
    .await;
    let removed: serde_json::Value = serde_json::from_str(&body).expect("remove json");
    assert_eq!(removed["ok"], true);
    expect_disconnected(&mut event_rx).await;
    server.server.shutdown().await;

    // Re-admit before the restarted relay accepts handshakes.
    fs::write(
        server.config.authorized_nodes_dir.join(node.fingerprint()),
        b"",
    )
    .expect("whitelist entry should re-admit the node");
    let restart_config = RelayServeConfig {
        listen: first_addr,
        ..server.config.clone()
    };
    let restarted = start(restart_config)
        .await
        .expect("restarted relay should bind the same address");

    expect_connected(&mut event_rx).await;
    wait_admin_lists_node(&admin_addr, &node.fingerprint()).await;

    handle.cancel();
    expect_clean_stop(event_rx).await;
    restarted.server.shutdown().await;
}

#[tokio::test]
async fn client_with_wrong_fingerprint_disconnects_and_retries() {
    install_provider();
    let node = TestNode::generate();
    let server = start_test_server_with(&[node.fingerprint()], long_lived_lifecycle()).await;
    let dir = temp_dir("relay-client-wrong-pin");
    let credentials = node_credentials_in(&dir, &node);

    let (event_tx, mut event_rx) = mpsc::channel(16);
    let handle = RelayClient::spawn(
        relay_client_config(
            server.server.local_addr(),
            "deadbeef".to_string(),
            credentials,
            Duration::from_millis(50),
        ),
        event_tx,
    );

    // The pin mismatch fails the TLS handshake; the client reports it and
    // keeps retrying — the link is infrastructure and never gives up.
    let reason = expect_disconnected(&mut event_rx).await;
    assert!(
        reason.contains("handshake"),
        "pin mismatch must fail the handshake: {reason}"
    );
    match next_client_event(&mut event_rx).await {
        RelayClientEvent::Connecting { .. } => {}
        other => panic!("expected a retry Connecting after disconnect, got {other:?}"),
    }

    handle.cancel();
    expect_clean_stop(event_rx).await;
    server.server.shutdown().await;
}

#[tokio::test]
async fn client_heartbeats_survive_eviction_window_but_silence_does_not() {
    install_provider();
    let node = TestNode::generate();
    let silent = TestNode::generate();
    let lifecycle = RelayLifecycleConfig {
        offline_after: Duration::from_millis(350),
        sweep_interval: Duration::from_millis(50),
        register_timeout: Duration::from_secs(2),
    };
    let mut server =
        start_test_server_with(&[node.fingerprint(), silent.fingerprint()], lifecycle).await;
    let server_der = server_cert_der(&server.config);
    let relay_fingerprint =
        node_credentials::cert_fingerprint_from_der(&server_der).expect("relay fingerprint");
    let admin_addr = server.server.admin_addr().clone();
    let dir = temp_dir("relay-client-eviction");
    let credentials = node_credentials_in(&dir, &node);

    let (event_tx, mut event_rx) = mpsc::channel(16);
    let handle = RelayClient::spawn(
        relay_client_config(
            server.server.local_addr(),
            relay_fingerprint,
            credentials,
            Duration::from_millis(100),
        ),
        event_tx,
    );
    expect_connected(&mut event_rx).await;
    wait_admin_lists_node(&admin_addr, &node.fingerprint()).await;

    // Control: a registered link that never heartbeats is evicted well inside
    // the window the client survives.
    let mut silent_link = register_node(&mut server, &silent, &server_der).await;
    expect_evicted(&mut server.events, &silent.fingerprint()).await;
    expect_link_closed(&mut silent_link).await;

    // Past the eviction window plus sweep slack the heartbeat-driven client
    // must still be registered.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_admin_lists_node(&admin_addr, &node.fingerprint()).await;

    handle.cancel();
    expect_clean_stop(event_rx).await;
    server.server.shutdown().await;
}
