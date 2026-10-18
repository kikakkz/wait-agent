//! WebUI integration tests (issue #131 slices 1-3), modeled on the
//! `infra::relay_link` test harness: a real in-process relay server on
//! loopback (no docker), the web service enrolling through the standard
//! flow, a scripted stub SMTP server for the magic link, and a raw-HTTP
//! client with header/cookie control.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_capacity::RelayCapacityConfig;
use crate::infra::relay_client::RelayClientEvent;
use crate::infra::relay_connection_table::{RelayLifecycleConfig, RelayLifecycleEvent};
use crate::infra::relay_server::{start, RelayServeConfig, RelayServerHandle, TokenTtlConfig};
use crate::infra::relay_toml_store::RelayTomlConfig;
use crate::platform::remote_ipc::{RemoteControlAddr, RemoteControlAsyncStream};
use crate::web::auth::fingerprint::{self, Probe};
use crate::web::auth::routes::{AuthState, WebState};
use crate::web::auth::smtp::stub::StubSmtp;
use crate::web::auth::token::{Claims, WebAuthKeys};
use crate::web::config::{MailCustom, MailTls, WebuiConfig};
use crate::web::serve::{build_router, enroll_and_link, WebServeConfig};

const NO_DEADLOCK: Duration = Duration::from_secs(10);

/// The User-Agent every test client sends; fingerprints are computed
/// against it exactly like a browser's would be.
const TEST_UA: &str = "waitagent-web-test/1.0";

fn test_probe() -> Probe {
    Probe {
        platform: "Linux x86_64".to_string(),
        timezone: "Asia/Shanghai".to_string(),
        language: "en-US".to_string(),
        screen: "1920x1080".to_string(),
    }
}

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
        legacy_enroll_listener: true,
        web_app: std::sync::Arc::new(std::sync::OnceLock::new()),
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

/// The full web stack for auth tests: enrolled relay link + stub SMTP +
/// deployment config + token keys, served with ConnectInfo on loopback.
struct WebStack {
    relay: RunningRelay,
    addr: SocketAddr,
    state: Arc<WebState>,
    server: JoinHandle<()>,
    stub: StubSmtp,
    node_fingerprint: String,
    dir: PathBuf,
}

async fn start_web_stack(name: &str, admin_email: &str) -> WebStack {
    install_provider();
    let relay = start_test_relay().await;
    let fingerprint = relay_fingerprint(&relay);
    let relay_address = format!("127.0.0.1:{}", relay.server.local_addr().port());
    let dir = temp_dir(name);
    let config = web_config(&dir);
    RelayTomlConfig {
        address: relay_address,
        relay_fingerprint: fingerprint.clone(),
        heartbeat_interval_secs: None,
        use_proxy: false,
    }
    .save(&config.relay_toml_path)
    .expect("pre-existing relay.toml should save");

    let link = enroll_and_link(&config)
        .await
        .expect("web node should enroll and link");
    let node_fingerprint = link.node_fingerprint.clone();

    let stub = StubSmtp::start().await;
    let webui_config = WebuiConfig {
        admin_email: admin_email.to_string(),
        mail_auth_code: "test-auth-code".to_string(),
        public_base_url: "http://dash.test".to_string(),
        trusted_proxies: Vec::new(),
        mail_custom: Some(MailCustom {
            host: "127.0.0.1".to_string(),
            port: stub.port(),
            tls: MailTls::None,
            user: admin_email.to_string(),
        }),
    };
    // Ephemeral signing keys (issue #143): generated in memory per boot,
    // never persisted — see the no-key-file assertion below.
    let keys = WebAuthKeys::generate().expect("web auth keys should generate");
    let auth = AuthState::new(keys, webui_config);
    let state = Arc::new(WebState::new(link.client, auth));

    // Issue #209: the same router is published into the relay's single-port
    // ALPN demux, so every stack test can also reach it over TLS on the
    // relay port.
    let router = build_router(state.clone());
    let _ = relay.config.web_app.set(router.clone());

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("server should serve");
    });

    WebStack {
        relay,
        addr,
        state,
        server,
        stub,
        node_fingerprint,
        dir,
    }
}

/// One raw HTTP request returning status, headers (lowercased names), and body.
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    body: &str,
) -> (u16, Vec<(String, String)>, String) {
    http_ua(addr, method, path, TEST_UA, extra_headers, body).await
}

/// The same request with an explicit User-Agent (fingerprint drift tests).
async fn http_ua(
    addr: SocketAddr,
    method: &str,
    path: &str,
    ua: &str,
    extra_headers: &[(&str, &str)],
    body: &str,
) -> (u16, Vec<(String, String)>, String) {
    let mut stream = timeout(NO_DEADLOCK, tokio::net::TcpStream::connect(addr))
        .await
        .expect("connect within deadline")
        .expect("tcp connect");
    let mut request = format!("{method} {path} HTTP/1.1\r\nhost: {addr}\r\n");
    request.push_str(&format!("user-agent: {ua}\r\n"));
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        request.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    request.push_str("connection: close\r\n\r\n");
    request.push_str(body);
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
    let mut lines = text.lines();
    let status_line = lines.next().expect("a status line");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("a status code")
        .parse()
        .expect("a numeric status code");
    let mut headers = Vec::new();
    let mut body_out = String::new();
    let mut in_body = false;
    for line in lines {
        if in_body {
            body_out.push_str(line);
            body_out.push('\n');
            continue;
        }
        if line.is_empty() {
            in_body = true;
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_lowercase(), value.trim().to_string()));
        }
    }
    (code, headers, body_out)
}

async fn http_get(addr: SocketAddr, path: &str) -> (u16, Vec<(String, String)>, String) {
    http(addr, "GET", path, &[], "").await
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn form_escape(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b' ' => out.push('+'),
            b'&' | b'=' | b'%' | b'+' => out.push_str(&format!("%{byte:02X}")),
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'-' | b'_') => {
                out.push(byte as char)
            }
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn magic_token_from_mail(transcript_body: &str) -> String {
    transcript_body
        .split_whitespace()
        .find_map(|word| word.strip_prefix("http://dash.test/auth/magic?token="))
        .expect("the mail body carries the magic link")
        .trim()
        .to_string()
}

/// Mints a live session directly (bypasses mail) for tests that focus on
/// the dashboard rather than the login flow.
fn mint_test_session(state: &AuthState) -> String {
    mint_session_with_keys(state, &state.keys)
}

/// Mints a live session signed by an explicit keypair — `keys` other than
/// the state's simulate a token carried over from before a key rotation.
fn mint_session_with_keys(state: &AuthState, keys: &WebAuthKeys) -> String {
    let fp = fingerprint::compute("127.0.0.1", TEST_UA, &Probe::default());
    let jti = crate::web::auth::token::new_jti();
    let iat = crate::web::auth::token::now_unix();
    state.stores.mint_session(
        &jti,
        fp.clone(),
        Probe::default(),
        Duration::from_secs(crate::web::auth::token::SESSION_TTL_SECS),
    );
    keys.sign(&Claims {
        sub: "session".to_string(),
        jti,
        iat,
        exp: iat + crate::web::auth::token::SESSION_TTL_SECS,
        fp,
    })
    .expect("session token signs")
}

// --- slice 4: dashboard write ops -------------------------------------------

/// Extracts the session CSRF token the dashboard renders into its forms,
/// exactly like a browser submitting the form would.
fn csrf_from_dashboard(body: &str) -> String {
    body.split("name=\"csrf\"")
        .nth(1)
        .expect("a csrf input")
        .split("value=\"")
        .nth(1)
        .expect("a csrf value")
        .split('"')
        .next()
        .expect("a quoted value")
        .to_string()
}

#[tokio::test]
async fn dashboard_write_ops_require_the_session_csrf_token() {
    let stack = start_web_stack("write-csrf", "admin@example.com").await;
    let session = mint_test_session(&stack.state.auth);
    let cookie = format!("session={session}");

    // No CSRF token at all: refused, and the session survives the refusal.
    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/api/invite",
        &[
            ("cookie", &cookie),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        "deploy=on",
    )
    .await;
    assert_eq!(code, 403, "a write without CSRF must be refused");
    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/api/invite",
        &[
            ("cookie", &cookie),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        "csrf=wrong-token&deploy=on",
    )
    .await;
    assert_eq!(code, 403, "a wrong CSRF token must be refused");

    // No cookie at all: still 403.
    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/api/invite",
        &[("content-type", "application/x-www-form-urlencoded")],
        "csrf=anything",
    )
    .await;
    assert_eq!(code, 403);

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn invite_shows_the_token_once_and_audits() {
    let stack = start_web_stack("write-invite", "admin@example.com").await;
    let session = mint_test_session(&stack.state.auth);
    let cookie = format!("session={session}");
    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    let csrf = loop {
        let (code, _, dashboard) = http(stack.addr, "GET", "/", &[("cookie", &cookie)], "").await;
        if code == 200 {
            break csrf_from_dashboard(&dashboard);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dashboard should serve over the session (code {code})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let (code, _, body) = http(
        stack.addr,
        "POST",
        "/api/invite",
        &[
            ("cookie", &cookie),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!("csrf={csrf}&ttl_secs=3600"),
    )
    .await;
    assert_eq!(code, 200, "invite should answer with the dashboard: {body}");
    let token_line = body
        .lines()
        .find(|line| line.contains("token: "))
        .expect("the banner carries the raw token");
    let token = token_line
        .split("token: ")
        .nth(1)
        .expect("token value")
        .split_whitespace()
        .next()
        .expect("a bare token");
    assert_eq!(
        token.len(),
        43,
        "256-bit base64url token shown once: {token_line}"
    );

    // The audit trail recorded the operator and the action.
    let audit = crate::infra::error_log::ERROR_LOG
        .entries()
        .into_iter()
        .find(|(_, _, message)| {
            message.contains("[web-audit]") && message.contains("action=invite")
        })
        .expect("an audit entry for the invite");
    assert!(audit.2.contains("operator="), "{audit:?}");

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn remove_revokes_a_registered_node_after_prefix_confirmation() {
    let stack = start_web_stack("write-remove", "admin@example.com").await;
    let session = mint_test_session(&stack.state.auth);
    let cookie = format!("session={session}");
    // The relay link registers asynchronously; poll the dashboard until it
    // renders, then take the CSRF token from the forms.
    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    let csrf = loop {
        let (code, _, dashboard) = http(stack.addr, "GET", "/", &[("cookie", &cookie)], "").await;
        if code == 200 {
            break csrf_from_dashboard(&dashboard);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dashboard should serve over the session (code {code})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Enroll a second web node; both register on the relay.
    let dir = temp_dir("write-remove-second");
    let config = web_config(&dir);
    let relay_fingerprint = relay_fingerprint(&stack.relay);
    let relay_address = format!("127.0.0.1:{}", stack.relay.server.local_addr().port());
    RelayTomlConfig {
        address: relay_address,
        relay_fingerprint,
        heartbeat_interval_secs: None,
        use_proxy: false,
    }
    .save(&config.relay_toml_path)
    .expect("relay.toml saves");
    let second = enroll_and_link(&config).await.expect("second node enrolls");
    let second_fingerprint = second.node_fingerprint.clone();
    let admin_addr = stack.relay.server.admin_addr().clone();
    registered_nodes_reaches(&admin_addr, 2).await;
    assert!(stack
        .relay
        .config
        .authorized_nodes_dir
        .join(&second_fingerprint)
        .is_file());

    // Confirm with a unique prefix: the dashboard resolves it and revokes.
    let prefix = second_fingerprint[..20].to_string();
    let (code, _, body) = http(
        stack.addr,
        "POST",
        "/api/remove",
        &[
            ("cookie", &cookie),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!("csrf={csrf}&fingerprint={prefix}"),
    )
    .await;
    assert_eq!(code, 200, "remove should answer with the dashboard: {body}");
    assert!(body.contains("removed"), "{body}");

    // The whitelist entry is gone and the connection table shrinks back.
    assert!(
        !stack
            .relay
            .config
            .authorized_nodes_dir
            .join(&second_fingerprint)
            .is_file(),
        "remove must delete the whitelist entry"
    );
    registered_nodes_reaches(&admin_addr, 1).await;

    stack.server.abort();
    drop(second);
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn remove_revokes_an_enrolled_but_offline_node() {
    let stack = start_web_stack("write-remove-offline", "admin@example.com").await;
    let session = mint_test_session(&stack.state.auth);
    let cookie = format!("session={session}");
    let admin_addr = stack.relay.server.admin_addr().clone();
    registered_nodes_reaches(&admin_addr, 1).await;

    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    let csrf = loop {
        let (code, _, dashboard) = http(stack.addr, "GET", "/", &[("cookie", &cookie)], "").await;
        if code == 200 {
            break csrf_from_dashboard(&dashboard);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dashboard should serve over the session (code {code})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Enrolled but offline: the whitelist entry exists while the node has
    // no relay link, so the connection table (status.nodes) cannot resolve
    // it — the dashboard must fall back to the whitelist listing (#147).
    let offline_fingerprint = format!("e2e{:061x}", 1);
    fs::write(
        stack
            .relay
            .config
            .authorized_nodes_dir
            .join(&offline_fingerprint),
        b"pem",
    )
    .expect("offline whitelist entry writes");

    let prefix = offline_fingerprint[..20].to_string();
    let (code, _, body) = http(
        stack.addr,
        "POST",
        "/api/remove",
        &[
            ("cookie", &cookie),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!("csrf={csrf}&fingerprint={prefix}"),
    )
    .await;
    assert_eq!(code, 200, "remove should answer with the dashboard: {body}");
    assert!(body.contains("removed"), "{body}");
    assert!(
        body.contains("no live link"),
        "an offline remove reports the missing live link: {body}"
    );

    // The whitelist entry is gone; the live table (web node only) is
    // untouched because the target was never registered.
    assert!(
        !stack
            .relay
            .config
            .authorized_nodes_dir
            .join(&offline_fingerprint)
            .is_file(),
        "offline remove must delete the whitelist entry"
    );
    registered_nodes_reaches(&admin_addr, 1).await;

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn remove_rejects_an_ambiguous_prefix_across_enrolled_nodes() {
    let stack = start_web_stack("write-remove-ambiguous", "admin@example.com").await;
    let session = mint_test_session(&stack.state.auth);
    let cookie = format!("session={session}");
    let admin_addr = stack.relay.server.admin_addr().clone();
    registered_nodes_reaches(&admin_addr, 1).await;

    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    let csrf = loop {
        let (code, _, dashboard) = http(stack.addr, "GET", "/", &[("cookie", &cookie)], "").await;
        if code == 200 {
            break csrf_from_dashboard(&dashboard);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dashboard should serve over the session (code {code})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Two enrolled-but-offline nodes sharing their first 20 hex chars: the
    // union pool (connection table + whitelist) must reject the prefix.
    let shared = "e2e";
    let first = format!("{shared}{:061x}", 2);
    let second = format!("{shared}{:061x}", 3);
    for fingerprint in [&first, &second] {
        fs::write(
            stack.relay.config.authorized_nodes_dir.join(fingerprint),
            b"pem",
        )
        .expect("whitelist entry writes");
    }

    let prefix = first[..20].to_string();
    assert_eq!(prefix, second[..20], "scenario needs a shared prefix");
    let (code, _, body) = http(
        stack.addr,
        "POST",
        "/api/remove",
        &[
            ("cookie", &cookie),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!("csrf={csrf}&fingerprint={prefix}"),
    )
    .await;
    assert_eq!(code, 200, "the dashboard answers with the banner: {body}");
    assert!(body.contains("ambiguous"), "{body}");
    for fingerprint in [&first, &second] {
        assert!(
            stack
                .relay
                .config
                .authorized_nodes_dir
                .join(fingerprint)
                .is_file(),
            "an ambiguous prefix must remove nothing: {fingerprint}"
        );
    }

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

// --- slice 1 anchors -------------------------------------------------------

/// Acceptance anchor (b): after `web serve` enrollment, the relay status
/// snapshot (the `relay status` equivalent query) counts the special node.
#[tokio::test]
async fn web_node_enrolls_through_standard_flow_and_appears_in_relay_status() {
    let relay = start_test_relay().await;
    let admin_addr = relay.server.admin_addr().clone();
    let relay_address = format!("127.0.0.1:{}", relay.server.local_addr().port());
    let fingerprint = relay_fingerprint(&relay);

    let status = admin_status(&admin_addr).await;
    assert_eq!(status["registered_nodes"], 0);

    let dir = temp_dir("web");
    let config = web_config(&dir);
    RelayTomlConfig {
        address: relay_address.clone(),
        relay_fingerprint: fingerprint.clone(),
        heartbeat_interval_secs: None,
        use_proxy: false,
    }
    .save(&config.relay_toml_path)
    .expect("pre-existing relay.toml should save");

    let mut link = enroll_and_link(&config)
        .await
        .expect("web node should enroll and link");

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

    drop(link);
    registered_nodes_reaches(&admin_addr, 0).await;
    relay.server.shutdown().await;
}

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
        use_proxy: false,
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

// --- slice 2/3: dashboard behind auth --------------------------------------

/// The dashboard and heartbeat serve only with a valid session whose
/// fingerprint matches; the magic-link flow produces exactly that session.
#[tokio::test]
async fn magic_link_flow_end_to_end() {
    let stack = start_web_stack("magic-flow", "admin@example.com").await;
    let addr = stack.addr;

    // Unauthenticated dashboard redirects to the login page; healthz stays open.
    let (code, headers, _) = http_get(addr, "/").await;
    assert_eq!(code, 303, "unauthenticated / must redirect");
    assert_eq!(header(&headers, "location"), Some("/login"));
    let (code, _, _) = http_get(addr, "/healthz").await;
    assert_eq!(code, 200);

    // The login page renders.
    let (code, _, body) = http_get(addr, "/login").await;
    assert_eq!(code, 200);
    assert!(body.contains("magic link"), "{body}");

    // Request a magic link (the stub captures the mail).
    let form = format!(
        "email={}&platform={}&timezone={}&language={}&screen={}",
        form_escape("admin@example.com"),
        form_escape(&test_probe().platform),
        form_escape(&test_probe().timezone),
        form_escape(&test_probe().language),
        form_escape(&test_probe().screen),
    );
    let (code, _, body) = http(
        addr,
        "POST",
        "/auth/magic",
        &[("content-type", "application/x-www-form-urlencoded")],
        &form,
    )
    .await;
    assert_eq!(code, 200, "login form should answer 200: {body}");
    assert!(
        body.contains("on its way"),
        "a neutral success message shows: {body}"
    );
    let transcript = stack.stub.wait_for_data().await;
    transcript.assert_auth("admin@example.com", "test-auth-code");
    transcript.assert_mail_from("admin@example.com");
    transcript.assert_rcpt_to("admin@example.com");
    let magic = magic_token_from_mail(transcript.data_body().expect("body"));

    // Redeem on the same machine/UA → session cookie → dashboard.
    let (code, headers, _) =
        http(addr, "GET", &format!("/auth/magic?token={magic}"), &[], "").await;
    assert_eq!(code, 303, "redeem should redirect to /: {headers:?}");
    assert_eq!(header(&headers, "location"), Some("/"));
    let cookie = header(&headers, "set-cookie").expect("a session cookie");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    let session = cookie
        .split(';')
        .next()
        .expect("cookie value")
        .strip_prefix("session=")
        .expect("session name")
        .to_string();

    // The dashboard lists the web node over the authenticated session.
    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    let body = loop {
        let (code, _, body) = http(
            addr,
            "GET",
            "/",
            &[("cookie", &format!("session={session}"))],
            "",
        )
        .await;
        if code == 200 && body.contains(&stack.node_fingerprint) {
            break body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dashboard should serve over the session (code {code}): {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(body.contains("Connection table"), "{body}");

    // Heartbeat with the same probes keeps the session alive.
    let heartbeat_body = format!(
        r#"{{"platform":"{}","timezone":"{}","language":"{}","screen":"{}"}}"#,
        test_probe().platform,
        test_probe().timezone,
        test_probe().language,
        test_probe().screen
    );
    let (code, _, _) = http(
        addr,
        "POST",
        "/api/heartbeat",
        &[
            ("cookie", &format!("session={session}")),
            ("content-type", "application/json"),
        ],
        &heartbeat_body,
    )
    .await;
    assert_eq!(code, 200, "heartbeat should be accepted");

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn wrong_email_gets_the_same_answer_and_no_mail() {
    let stack = start_web_stack("wrong-email", "admin@example.com").await;
    let form = "email=someone%40else.example";
    let (code, _, body) = http(
        stack.addr,
        "POST",
        "/auth/magic",
        &[("content-type", "application/x-www-form-urlencoded")],
        form,
    )
    .await;
    assert_eq!(code, 200);
    assert!(
        body.contains("on its way"),
        "the answer must not reveal the address is wrong: {body}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        stack.stub.transcript().data.is_none(),
        "no mail may leave for a non-admin address"
    );
    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn fingerprint_mismatch_consumes_the_magic_link() {
    let stack = start_web_stack("fp-mismatch", "admin@example.com").await;
    let form = format!(
        "email={}&platform={}&timezone={}&language={}&screen={}",
        form_escape("admin@example.com"),
        form_escape(&test_probe().platform),
        form_escape(&test_probe().timezone),
        form_escape(&test_probe().language),
        form_escape(&test_probe().screen),
    );
    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/auth/magic",
        &[("content-type", "application/x-www-form-urlencoded")],
        &form,
    )
    .await;
    assert_eq!(code, 200);
    let transcript = stack.stub.wait_for_data().await;
    let magic = magic_token_from_mail(transcript.data_body().expect("body"));

    // Redeem with a different User-Agent (a different browser/machine):
    // the same-machine rule fires and the token is consumed.
    let (code, _, body) = http_ua(
        stack.addr,
        "GET",
        &format!("/auth/magic?token={magic}"),
        "another-agent/9.9",
        &[],
        "",
    )
    .await;
    assert_eq!(code, 403, "fingerprint mismatch must be 403: {body}");

    // Even the "right" client cannot redeem it anymore.
    let (code, _, _) = http(
        stack.addr,
        "GET",
        &format!("/auth/magic?token={magic}"),
        &[],
        "",
    )
    .await;
    assert_eq!(code, 403, "a consumed magic link never redeems");

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn magic_requests_are_rate_limited_per_ip() {
    let stack = start_web_stack("rate-limit", "admin@example.com").await;
    let form = format!("email={}", form_escape("admin@example.com"));
    for attempt in 1..=3 {
        let (code, _, _) = http(
            stack.addr,
            "POST",
            "/auth/magic",
            &[("content-type", "application/x-www-form-urlencoded")],
            &form,
        )
        .await;
        assert_eq!(code, 200, "attempt {attempt} should pass");
    }
    let (code, _, body) = http(
        stack.addr,
        "POST",
        "/auth/magic",
        &[("content-type", "application/x-www-form-urlencoded")],
        &form,
    )
    .await;
    assert_eq!(
        code, 429,
        "the 4th request within 10 minutes is refused: {body}"
    );
    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn session_with_a_mismatched_fingerprint_is_invalidated() {
    let stack = start_web_stack("session-fp", "admin@example.com").await;
    let session = mint_test_session(&stack.state.auth);
    let cookie = format!("session={session}");

    // Same fingerprint: the API answers.
    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/api/heartbeat",
        &[("cookie", &cookie), ("content-type", "application/json")],
        "{}",
    )
    .await;
    assert_eq!(code, 200, "default probe matches the minted session");

    // A different UA recomputes to a different fingerprint: 403 and the
    // session is revoked.
    let (code, _, _) = http_ua(
        stack.addr,
        "POST",
        "/api/heartbeat",
        "evil-agent/1.0",
        &[("cookie", &cookie), ("content-type", "application/json")],
        "{}",
    )
    .await;
    assert_eq!(code, 403, "fingerprint drift must be refused");

    // And the original client is now locked out too (session removed).
    let (code, headers, _) = http(stack.addr, "GET", "/", &[("cookie", &cookie)], "").await;
    assert_eq!(code, 303, "the revoked session redirects to login");
    assert_eq!(header(&headers, "location"), Some("/login"));

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

// --- slice 5: ephemeral signing keys (issue #143) ----------------------------

/// The signing key never touches disk: starting the whole web stack leaves
/// no key file behind, so there is nothing to back up or leak. (The old
/// default path under waitagent_home is asserted in the process e2e, where
/// WAITAGENT_HOME is a fresh directory.)
#[tokio::test]
async fn web_stack_persists_no_web_auth_key_file() {
    let stack = start_web_stack("key-file", "admin@example.com").await;
    assert!(
        !stack.dir.join("web-auth.key").is_file(),
        "no web-auth.key may be written under the web home"
    );

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

/// Restart rotation semantics: a session signed by the previous boot's key
/// is unauthenticated after the restart (redirect/403, never an error
/// page), and the standard magic-link flow re-authenticates against the
/// fresh key.
#[tokio::test]
async fn session_signed_by_a_rotated_key_is_unauthenticated_and_reauth_recovers() {
    let stack = start_web_stack("key-rotation", "admin@example.com").await;

    // A token minted under a DIFFERENT keypair (the retired pre-restart
    // key) fails verification: / redirects to /login, /api answers 403.
    let retired_keys = WebAuthKeys::generate().expect("retired keypair");
    let stale_session = mint_session_with_keys(&stack.state.auth, &retired_keys);
    let stale_cookie = format!("session={stale_session}");
    let (code, headers, _) = http(stack.addr, "GET", "/", &[("cookie", &stale_cookie)], "").await;
    assert_eq!(
        code, 303,
        "a pre-restart session is unauthenticated, not an error: {headers:?}"
    );
    assert_eq!(header(&headers, "location"), Some("/login"));
    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/api/heartbeat",
        &[
            ("cookie", &stale_cookie),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(code, 403, "a stale session must not reach the API");

    // Re-authentication closes the loop with the current key: request a
    // fresh magic link, redeem it, and the dashboard serves the session.
    let form = format!(
        "email={}&platform={}&timezone={}&language={}&screen={}",
        form_escape("admin@example.com"),
        form_escape(&test_probe().platform),
        form_escape(&test_probe().timezone),
        form_escape(&test_probe().language),
        form_escape(&test_probe().screen),
    );
    let (code, _, body) = http(
        stack.addr,
        "POST",
        "/auth/magic",
        &[("content-type", "application/x-www-form-urlencoded")],
        &form,
    )
    .await;
    assert_eq!(code, 200, "magic request should answer 200: {body}");
    let transcript = stack.stub.wait_for_data().await;
    let magic = magic_token_from_mail(transcript.data_body().expect("body"));
    let (code, headers, _) = http(
        stack.addr,
        "GET",
        &format!("/auth/magic?token={magic}"),
        &[],
        "",
    )
    .await;
    assert_eq!(code, 303, "redeem should redirect to /");
    let cookie = header(&headers, "set-cookie").expect("a fresh session cookie");
    let session = cookie
        .split(';')
        .next()
        .expect("cookie value")
        .strip_prefix("session=")
        .expect("session name")
        .to_string();
    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    loop {
        let (code, _, body) = http(
            stack.addr,
            "GET",
            "/",
            &[("cookie", &format!("session={session}"))],
            "",
        )
        .await;
        if code == 200 && body.contains(&stack.node_fingerprint) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "re-authenticated dashboard should serve (code {code}): {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

#[tokio::test]
async fn expired_magic_token_is_rejected() {
    let stack = start_web_stack("expired-magic", "admin@example.com").await;
    let fp = fingerprint::compute("127.0.0.1", TEST_UA, &test_probe());
    let jti = crate::web::auth::token::new_jti();
    let iat = crate::web::auth::token::now_unix() - 2 * crate::web::auth::token::MAGIC_TTL_SECS;
    let claims = Claims {
        sub: "magic".to_string(),
        jti,
        iat,
        exp: iat + crate::web::auth::token::MAGIC_TTL_SECS,
        fp: fp.clone(),
    };
    let token = stack.state.auth.keys.sign(&claims).expect("sign");
    stack.state.auth.stores.mint_magic(
        &claims.jti,
        fp,
        test_probe(),
        Duration::from_secs(crate::web::auth::token::MAGIC_TTL_SECS),
    );
    let (code, _, body) = http(
        stack.addr,
        "GET",
        &format!("/auth/magic?token={token}"),
        &[],
        "",
    )
    .await;
    assert_eq!(code, 403, "an expired magic link must be rejected: {body}");
    stack.server.abort();
    drop(stack.state);
    stack.relay.server.shutdown().await;
}

// --- issue #209: the dashboard on the relay port's ALPN-demuxed TLS -------

/// Test-only verifier: accept any server certificate (the relay serves a
/// self-signed one; browsers warn, the client joins do the same).
#[derive(Debug)]
struct AcceptAnyServerCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// One HTTPS GET against the relay port's web ALPN route, speaking HTTP/1.1
/// manually over rustls. Returns status, headers, and body.
async fn https_get(
    relay_port: u16,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, String) {
    install_provider();
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = timeout(
        NO_DEADLOCK,
        tokio::net::TcpStream::connect(("127.0.0.1", relay_port)),
    )
    .await
    .expect("tcp connect should finish")
    .expect("tcp connect should succeed");
    let server_name = rustls::pki_types::ServerName::try_from("127.0.0.1").expect("ip name");
    let mut tls = timeout(NO_DEADLOCK, connector.connect(server_name, tcp))
        .await
        .expect("tls handshake should finish")
        .expect("tls handshake should succeed");
    assert_eq!(
        tls.get_ref().1.alpn_protocol(),
        Some(b"http/1.1".as_slice()),
        "the web route negotiates http/1.1"
    );

    let mut request = format!(
        "GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{relay_port}\r\nuser-agent: {TEST_UA}\r\nconnection: close\r\n"
    );
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    tls.write_all(request.as_bytes())
        .await
        .expect("request should write");
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw)
        .await
        .expect("response should read");
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("a header block");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .expect("a status line")
        .split_whitespace()
        .nth(1)
        .expect("a status code")
        .parse::<u16>()
        .expect("a numeric status");
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').expect("a header line");
            (name.trim().to_lowercase(), value.trim().to_string())
        })
        .collect::<Vec<_>>();
    let decoded = decode_chunked(&headers, body);
    (status, headers, decoded)
}

/// hyper answers HTTP/1.1 without a content-length with chunked encoding;
/// decode it well enough for assertions.
fn decode_chunked(headers: &[(String, String)], body: &str) -> String {
    if header(headers, "transfer-encoding") != Some("chunked") {
        return body.to_string();
    }
    let bytes = body.as_bytes();
    let mut out = String::new();
    let mut position = 0usize;
    while position < bytes.len() {
        let line_end = bytes[position..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|offset| position + offset)
            .unwrap_or(bytes.len());
        let size_text = String::from_utf8_lossy(&bytes[position..line_end])
            .trim()
            .to_string();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("0"), 16)
            .expect("a chunk size");
        if size == 0 {
            break;
        }
        let chunk_start = line_end + 1;
        out.push_str(&String::from_utf8_lossy(
            &bytes[chunk_start..chunk_start + size],
        ));
        position = chunk_start + size + 1; // skip the trailing newline
    }
    out
}

/// Issue #209: the dashboard serves on the relay port itself — TLS with the
/// web ALPN, session cookies flagged Secure, auth semantics identical to
/// the standalone listener.
#[tokio::test]
async fn single_port_tls_serves_the_dashboard_with_secure_cookies() {
    let stack = start_web_stack("single-port-tls", "admin@example.com").await;
    let relay_port = stack.relay.server.local_addr().port();

    // healthz over the relay port's TLS web route.
    let (code, _, _) = https_get(relay_port, "/healthz", &[]).await;
    assert_eq!(code, 200, "healthz over the single-port TLS demux");

    // Auth middleware runs unchanged (ConnectInfo injected per connection).
    let (code, headers, _) = https_get(relay_port, "/", &[]).await;
    assert_eq!(code, 303, "unauthenticated / must redirect over TLS");
    assert_eq!(header(&headers, "location"), Some("/login"));

    // Mint a magic link over the plain listener, redeem it over TLS: the
    // session cookie must carry Secure on the TLS path.
    let form = format!(
        "email={}&platform={}&timezone={}&language={}&screen={}",
        form_escape("admin@example.com"),
        form_escape(&test_probe().platform),
        form_escape(&test_probe().timezone),
        form_escape(&test_probe().language),
        form_escape(&test_probe().screen),
    );
    let (code, _, body) = http(
        stack.addr,
        "POST",
        "/auth/magic",
        &[("content-type", "application/x-www-form-urlencoded")],
        &form,
    )
    .await;
    assert_eq!(code, 200, "login form should answer 200: {body}");
    let transcript = stack.stub.wait_for_data().await;
    let magic = magic_token_from_mail(transcript.data_body().expect("body"));
    let (code, headers, _) =
        https_get(relay_port, &format!("/auth/magic?token={magic}"), &[]).await;
    assert_eq!(code, 303, "redeem over TLS should redirect: {headers:?}");
    let cookie = header(&headers, "set-cookie").expect("a session cookie");
    assert!(
        cookie.contains("Secure"),
        "TLS-served session cookies are Secure: {cookie}"
    );
    let session = cookie.split(';').next().expect("cookie value").to_string();

    // The session works over TLS too.
    let (code, body) = {
        let (code, _, body) = https_get(relay_port, "/", &[("cookie", &session)]).await;
        (code, body)
    };
    assert_eq!(code, 200, "dashboard over TLS: {body}");
    assert!(body.contains(&stack.node_fingerprint), "{body}");

    drop(stack);
}

// --- issue #208: self-service enrollment via admin-email magic link -------

use crate::infra::relay_join::join_relay;
use crate::web::auth::routes::join_request;
use crate::web::auth::token::ENROLL_MAGIC_SUB;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};

/// The enrollment link out of a captured request mail: the `--link` URL is
/// the whitespace word carrying the enroll path.
fn enroll_link_from_mail(transcript_body: &str) -> String {
    transcript_body
        .split_whitespace()
        .find(|word| word.starts_with("http://dash.test/auth/enroll?token="))
        .expect("the mail body carries the enrollment link")
        .to_string()
}

#[tokio::test]
async fn join_request_mails_a_device_bound_enrollment_link() {
    let stack = start_web_stack("join-request", "admin@example.com").await;
    let device_fp = node_credentials::load_cert_fingerprint(&stack.dir.join("web-node.crt"))
        .expect("web node fingerprint");

    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/auth/join-request",
        &[("content-type", "application/json")],
        &serde_json::json!({"fp": device_fp}).to_string(),
    )
    .await;
    assert_eq!(code, 200, "join-request should answer 200");

    let transcript = stack.stub.wait_for_data().await;
    // Subject distinguishes enrollment mail from dashboard login mail; it
    // rides inside the DATA payload.
    let body = transcript.data_body().expect("mail body");
    let subject = body
        .lines()
        .find_map(|line| line.strip_prefix("Subject: "))
        .expect("a Subject header");
    assert!(
        subject.contains("enrollment request") && subject.contains(&device_fp[..8]),
        "the subject names the flow and the device: {subject}"
    );
    assert!(
        body.contains("--link"),
        "the body carries the join command: {body}"
    );
    let link = enroll_link_from_mail(body);
    let token = link
        .strip_prefix("http://dash.test/auth/enroll?token=")
        .expect("the link embeds the token");
    let claims = stack
        .state
        .auth
        .keys
        .verify(token, ENROLL_MAGIC_SUB)
        .expect("the mailed token is an enroll-magic JWT");
    assert_eq!(
        claims.fp, device_fp,
        "the link is bound to the requesting device"
    );
}

#[tokio::test]
async fn join_request_is_rate_limited_per_ip() {
    let stack = start_web_stack("join-request-limit", "admin@example.com").await;
    let device_fp = node_credentials::load_cert_fingerprint(&stack.dir.join("web-node.crt"))
        .expect("web node fingerprint");
    let body = serde_json::json!({"fp": device_fp}).to_string();
    for attempt in 1..=3 {
        let (code, _, _) = http(
            stack.addr,
            "POST",
            "/auth/join-request",
            &[("content-type", "application/json")],
            &body,
        )
        .await;
        assert_eq!(code, 200, "request {attempt} should pass");
    }
    let (code, _, answer) = http(
        stack.addr,
        "POST",
        "/auth/join-request",
        &[("content-type", "application/json")],
        &body,
    )
    .await;
    assert_eq!(
        code, 429,
        "the 4th request inside the window is refused: {answer}"
    );
}

#[tokio::test]
async fn join_request_rejects_a_malformed_fingerprint() {
    let stack = start_web_stack("join-request-bad-fp", "admin@example.com").await;
    let (code, _, answer) = http(
        stack.addr,
        "POST",
        "/auth/join-request",
        &[("content-type", "application/json")],
        &serde_json::json!({"fp": "not-hex"}).to_string(),
    )
    .await;
    assert_eq!(code, 400, "a non-hex fp must fail: {answer}");
}

/// The full self-service flow: request → mail → redeem (enroll token) →
/// the standard Enroll protocol enrolls and whitelists the device.
#[tokio::test]
async fn enroll_redeem_mints_a_one_time_token_that_enrolls_the_device() {
    let stack = start_web_stack("enroll-redeem", "admin@example.com").await;
    let relay_address = format!("127.0.0.1:{}", stack.relay.server.local_addr().port());
    let relay_fp = relay_fingerprint(&stack.relay);

    // A fresh applicant identity on the "requesting device".
    let dir = temp_dir("enroll-redeem-device");
    let credentials = NodeCredentialPaths {
        key_path: dir.join("node.key"),
        cert_path: dir.join("node.crt"),
    };
    let device_fp = node_credentials::ensure_credentials(&credentials).expect("device identity");

    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/auth/join-request",
        &[("content-type", "application/json")],
        &serde_json::json!({"fp": device_fp}).to_string(),
    )
    .await;
    assert_eq!(code, 200);
    let transcript = stack.stub.wait_for_data().await;
    let link = enroll_link_from_mail(transcript.data_body().expect("body"));
    let magic = link
        .strip_prefix("http://dash.test/auth/enroll?token=")
        .expect("token in the link")
        .to_string();

    // Redeem: one-time enroll token, machine-readable answer.
    let (code, _, answer) = http(
        stack.addr,
        "POST",
        "/auth/enroll",
        &[("content-type", "application/json")],
        &serde_json::json!({"token": magic, "fp": device_fp}).to_string(),
    )
    .await;
    assert_eq!(code, 200, "redeem should answer 200: {answer}");
    let enroll_token = serde_json::from_str::<serde_json::Value>(&answer).expect("json answer")
        ["token"]
        .as_str()
        .expect("an enroll token")
        .to_string();

    // The standard enrollment protocol (#209 dual-path negotiation).
    let toml_path = dir.join("relay.toml");
    let outcome = join_relay(
        &relay_address,
        &enroll_token,
        &credentials,
        &toml_path,
        None,
    )
    .await
    .expect("the enroll token must redeem through the standard flow");
    assert_eq!(outcome.relay_fingerprint, relay_fp);
    assert!(
        stack
            .relay
            .config
            .authorized_nodes_dir
            .join(&device_fp)
            .is_file(),
        "the self-service enrollment must whitelist the device"
    );

    // A second redeem of the same link is refused (one-time jti).
    let (code, _, answer) = http(
        stack.addr,
        "POST",
        "/auth/enroll",
        &[("content-type", "application/json")],
        &serde_json::json!({"token": magic, "fp": device_fp}).to_string(),
    )
    .await;
    assert_eq!(code, 403, "the link redeems exactly once: {answer}");
}

#[tokio::test]
async fn enroll_redeem_rejects_other_devices_and_dashboard_tokens() {
    let stack = start_web_stack("enroll-redeem-guards", "admin@example.com").await;
    let device_fp = node_credentials::load_cert_fingerprint(&stack.dir.join("web-node.crt"))
        .expect("web node fingerprint");
    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/auth/join-request",
        &[("content-type", "application/json")],
        &serde_json::json!({"fp": device_fp}).to_string(),
    )
    .await;
    assert_eq!(code, 200);
    let transcript = stack.stub.wait_for_data().await;
    let link = enroll_link_from_mail(transcript.data_body().expect("body"));
    let magic = link
        .strip_prefix("http://dash.test/auth/enroll?token=")
        .expect("the token in the link")
        .to_string();

    // A different device presenting the same link is refused — and the
    // refusal does NOT consume the link (the fp check precedes redemption),
    // so the legitimate device can still use it afterwards.
    let other = "ab".repeat(32);
    let (code, _, answer) = http(
        stack.addr,
        "POST",
        "/auth/enroll",
        &[("content-type", "application/json")],
        &serde_json::json!({"token": magic, "fp": other}).to_string(),
    )
    .await;
    assert_eq!(code, 403, "fp mismatch must refuse: {answer}");
    let (code, _, answer) = http(
        stack.addr,
        "POST",
        "/auth/enroll",
        &[("content-type", "application/json")],
        &serde_json::json!({"token": magic, "fp": device_fp}).to_string(),
    )
    .await;
    assert_eq!(
        code, 200,
        "a wrong-device attempt leaves the link usable for the requesting device: {answer}"
    );

    // The legitimate redemption consumed the jti: no third use.
    let (code, _, answer) = http(
        stack.addr,
        "POST",
        "/auth/enroll",
        &[("content-type", "application/json")],
        &serde_json::json!({"token": magic, "fp": device_fp}).to_string(),
    )
    .await;
    assert_eq!(code, 403, "the link redeems exactly once: {answer}");

    // A dashboard login token (sub = "magic") redeems nothing here.
    let iat = crate::web::auth::token::now_unix();
    let dashboard_token = stack
        .state
        .auth
        .keys
        .sign(&crate::web::auth::token::Claims {
            sub: "magic".to_string(),
            jti: crate::web::auth::token::new_jti(),
            iat,
            exp: iat + crate::web::auth::token::MAGIC_TTL_SECS,
            fp: device_fp.clone(),
        })
        .expect("sign");
    let (code, _, _) = http(
        stack.addr,
        "POST",
        "/auth/enroll",
        &[("content-type", "application/json")],
        &serde_json::json!({"token": dashboard_token, "fp": device_fp}).to_string(),
    )
    .await;
    assert_eq!(code, 403, "cross-subject tokens are refused");
}

/// The mail-failure gate calls the handler directly (no listener needed):
/// a relay whose SMTP is unreachable answers 502 with the `relay invite`
/// fallback spelled out.
#[tokio::test]
async fn join_request_mail_failure_names_the_relay_invite_fallback() {
    install_provider();
    let dir = temp_dir("join-request-dead-mail");
    let credentials = NodeCredentialPaths {
        key_path: dir.join("node.key"),
        cert_path: dir.join("node.crt"),
    };
    let _ = node_credentials::ensure_credentials(&credentials);
    let (event_tx, _events) = mpsc::channel(16);
    let client = crate::infra::relay_client::RelayClient::spawn(
        crate::infra::relay_client::RelayClientConfig::from_relay_toml(
            RelayTomlConfig {
                address: "127.0.0.1:1".to_string(),
                relay_fingerprint: "ab".repeat(32),
                heartbeat_interval_secs: None,
                use_proxy: false,
            },
            credentials,
        ),
        event_tx,
    );
    let config = WebuiConfig {
        admin_email: "admin@example.com".to_string(),
        mail_auth_code: "code".to_string(),
        public_base_url: "http://dash.test".to_string(),
        trusted_proxies: Vec::new(),
        mail_custom: Some(MailCustom {
            host: "127.0.0.1".to_string(),
            port: 1,
            tls: MailTls::None,
            user: "admin@example.com".to_string(),
        }),
    };
    let state = Arc::new(WebState::new(
        client,
        AuthState::new(WebAuthKeys::generate().expect("keys"), config),
    ));
    let fp = "cd".repeat(32);
    let answer = join_request(
        State(state),
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))),
        HeaderMap::new(),
        axum::body::Bytes::from(serde_json::json!({"fp": fp}).to_string()),
    )
    .await;
    assert_eq!(answer.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(answer.into_body(), 4096)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("relay invite"),
        "the error names the operator path: {text}"
    );
}
