//! WebUI service slice-1 integration tests (issue #131), modeled on the
//! `infra::relay_link` test harness: a real in-process relay server on
//! loopback (no docker), the web service enrolling through the standard
//! admin-invite -> enrollment session -> relay-client flow, and a raw-HTTP
//! `GET /healthz` against the axum app.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_capacity::RelayCapacityConfig;
use crate::infra::relay_client::RelayClientEvent;
use crate::infra::relay_connection_table::{RelayLifecycleConfig, RelayLifecycleEvent};
use crate::infra::relay_server::{start, RelayServeConfig, RelayServerHandle, TokenTtlConfig};
use crate::infra::relay_toml_store::RelayTomlConfig;
use crate::platform::remote_ipc::{RemoteControlAddr, RemoteControlAsyncStream};
use crate::web::serve::{build_router, enroll_and_link, WebServeConfig};

const NO_DEADLOCK: Duration = Duration::from_secs(10);

fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "waitagent-web-{name}-{}-{}",
        std::process::id(),
        std::thread::current()
            .name()
            .unwrap_or("test")
            .replace(":", "_")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir should create");
    dir
}

struct RunningRelay {
    server: RelayServerHandle,
    #[allow(dead_code)]
    events: mpsc::Receiver<RelayLifecycleEvent>,
    config: RelayServeConfig,
}

async fn start_test_relay() -> RunningRelay {
    install_provider();
    let dir = temp_dir("relay");
    let whitelist_dir = dir.join("authorized_nodes");
    fs::create_dir_all(&whitelist_dir).expect("whitelist dir should create");
    let config = RelayServeConfig {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        authorized_nodes_dir: whitelist_dir,
        credentials: NodeCredentialPaths {
            key_path: dir.join("relay.key"),
            cert_path: dir.join("relay.crt"),
        },
        lifecycle: RelayLifecycleConfig::default(),
        capacity: RelayCapacityConfig::default(),
        tokens_path: dir.join("relay-enroll-tokens.json"),
        admin_socket: None,
        token_ttls: TokenTtlConfig::default(),
    };
    let started = start(config.clone()).await.expect("relay should start");
    RunningRelay {
        server: started.server,
        events: started.events,
        config,
    }
}

fn relay_fingerprint(relay: &RunningRelay) -> String {
    let pem = fs::read_to_string(&relay.config.credentials.cert_path).expect("relay cert readable");
    let mut reader = pem.as_bytes();
    let cert = rustls_pemfile::certs(&mut reader)
        .next()
        .expect("one cert")
        .expect("cert parses");
    node_credentials::cert_fingerprint_from_der(cert.as_ref()).expect("relay fingerprint computes")
}

async fn admin_request(addr: &RemoteControlAddr, request: &str) -> String {
    let mut stream = RemoteControlAsyncStream::connect(addr)
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

async fn admin_status(addr: &RemoteControlAddr) -> serde_json::Value {
    let body = admin_request(addr, r#"{"command":"status"}"#).await;
    serde_json::from_str(&body).expect("status should be json")
}

async fn registered_nodes_reaches(addr: &RemoteControlAddr, expected: u64) {
    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    loop {
        let status = admin_status(addr).await;
        if status["registered_nodes"].as_u64() == Some(expected) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "registered_nodes should reach {expected}, got {}",
            status["registered_nodes"]
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn web_config(dir: &Path) -> WebServeConfig {
    WebServeConfig {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        credentials: NodeCredentialPaths {
            key_path: dir.join("web-node.key"),
            cert_path: dir.join("web-node.crt"),
        },
        relay_toml_path: dir.join("relay.toml"),
    }
}

/// Acceptance anchor (a): the axum skeleton answers `GET /healthz` with 200.
#[tokio::test]
async fn healthz_returns_200_ok() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, build_router())
            .await
            .expect("server should serve");
    });

    let code = http_get_status(addr, "/healthz").await;
    assert_eq!(code, 200, "/healthz must answer 200");
    server.abort();
}

/// Acceptance anchor (b): after `web serve` enrollment, the relay status
/// snapshot (the `relay status` equivalent query) counts the special node.
#[tokio::test]
async fn web_node_enrolls_through_standard_flow_and_appears_in_relay_status() {
    let relay = start_test_relay().await;
    let admin_addr = relay.server.admin_addr().clone();
    let relay_address = format!("127.0.0.1:{}", relay.server.local_addr().port());
    let fingerprint = relay_fingerprint(&relay);

    // Baseline: the connection table starts empty so the +1 below is
    // attributable to the web node.
    let status = admin_status(&admin_addr).await;
    assert_eq!(status["registered_nodes"], 0);

    let dir = temp_dir("web");
    let config = web_config(&dir);
    // The machine joined the relay before `web serve` starts: relay.toml
    // pins address and fingerprint exactly as `relay join` wrote them.
    RelayTomlConfig {
        address: relay_address.clone(),
        relay_fingerprint: fingerprint.clone(),
        heartbeat_interval_secs: None,
    }
    .save(&config.relay_toml_path)
    .expect("pre-existing relay.toml should save");

    let mut link = enroll_and_link(&config)
        .await
        .expect("web node should enroll and link");

    // Standard enrollment artifacts: the whitelist gained this node's
    // certificate and the relay pin landed in relay.toml.
    assert!(
        relay
            .config
            .authorized_nodes_dir
            .join(&link.node_fingerprint)
            .is_file(),
        "enrollment must whitelist the web node certificate"
    );
    let pinned = RelayTomlConfig::load(&config.relay_toml_path)
        .expect("relay.toml should load")
        .expect("relay.toml should exist");
    assert_eq!(pinned.address, relay_address);
    assert_eq!(pinned.relay_fingerprint, fingerprint);

    // The persistent link registered itself over the standard node<->relay
    // protocol (Connecting precedes Connected on every attempt).
    loop {
        match timeout(NO_DEADLOCK, link.events.recv())
            .await
            .expect("a link event should arrive")
            .expect("the event stream should stay open")
        {
            RelayClientEvent::Connected {
                relay_address: seen,
            } => {
                assert_eq!(seen, relay_address);
                break;
            }
            RelayClientEvent::Connecting { .. } => {}
            other => panic!("expected Connecting/Connected, got {other:?}"),
        }
    }
    registered_nodes_reaches(&admin_addr, 1).await;
    let status = admin_status(&admin_addr).await;
    let node_ids: Vec<&str> = status["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|node| node["node_id"].as_str().expect("node id"))
        .collect();
    assert_eq!(
        node_ids,
        vec![link.node_fingerprint.as_str()],
        "the relay status snapshot must list the special node"
    );

    // Tearing the link down unregisters it again: the entry was ours.
    drop(link);
    registered_nodes_reaches(&admin_addr, 0).await;
    relay.server.shutdown().await;
}

/// `relay join` documents that it rewrites only the enrollment result;
/// a service re-enrolling on every boot must not drop an operator's
/// heartbeat override from relay.toml.
#[tokio::test]
async fn web_enroll_preserves_operator_heartbeat_override() {
    let relay = start_test_relay().await;
    let fingerprint = relay_fingerprint(&relay);
    let relay_address = format!("127.0.0.1:{}", relay.server.local_addr().port());

    let dir = temp_dir("web-hb");
    let config = web_config(&dir);
    RelayTomlConfig {
        address: relay_address,
        relay_fingerprint: fingerprint,
        heartbeat_interval_secs: Some(42),
    }
    .save(&config.relay_toml_path)
    .expect("pre-existing relay.toml should save");

    let link = enroll_and_link(&config)
        .await
        .expect("web node should enroll against the pinned relay");
    let pinned = RelayTomlConfig::load(&config.relay_toml_path)
        .expect("relay.toml should load")
        .expect("relay.toml should exist");
    assert_eq!(
        pinned.heartbeat_interval_secs,
        Some(42),
        "re-enrollment must keep the operator's heartbeat override"
    );

    drop(link);
    relay.server.shutdown().await;
}

/// Without a pinned relay there is nothing to enroll against: the error
/// names the fix instead of silently doing nothing.
#[tokio::test]
async fn web_enroll_requires_a_pinned_relay() {
    install_provider();
    let dir = temp_dir("web-missing-pin");
    let config = web_config(&dir);
    match enroll_and_link(&config).await {
        Err(error) => assert!(
            error.to_string().contains("relay join"),
            "the error names the join command: {error}"
        ),
        Ok(_) => panic!("a missing relay.toml must fail enrollment"),
    }
}

async fn http_get_status(addr: SocketAddr, path: &str) -> u16 {
    let mut stream = timeout(NO_DEADLOCK, tokio::net::TcpStream::connect(addr))
        .await
        .expect("connect within deadline")
        .expect("tcp connect");
    let request = format!("GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("request should write");
    let mut response = Vec::new();
    timeout(NO_DEADLOCK, stream.read_to_end(&mut response))
        .await
        .expect("read within deadline")
        .expect("response should read");
    let text = String::from_utf8_lossy(&response);
    text.lines()
        .next()
        .expect("a status line")
        .split_whitespace()
        .nth(1)
        .expect("a status code")
        .parse()
        .expect("a numeric status code")
}
