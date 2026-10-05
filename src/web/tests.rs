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
    let keys = WebAuthKeys::load_or_generate(&dir.join("web-auth.key"))
        .expect("web auth keys should generate");
    let auth = AuthState::new(keys, webui_config);
    let state = Arc::new(WebState::new(link.client, auth));

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let serve_state = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            build_router(serve_state).into_make_service_with_connect_info::<SocketAddr>(),
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
    let fp = fingerprint::compute("127.0.0.1", TEST_UA, &Probe::default());
    let jti = crate::web::auth::token::new_jti();
    let iat = crate::web::auth::token::now_unix();
    state.stores.mint_session(
        &jti,
        fp.clone(),
        Probe::default(),
        Duration::from_secs(crate::web::auth::token::SESSION_TTL_SECS),
    );
    state
        .keys
        .sign(&Claims {
            sub: "session".to_string(),
            jti,
            iat,
            exp: iat + crate::web::auth::token::SESSION_TTL_SECS,
            fp,
        })
        .expect("session token signs")
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
