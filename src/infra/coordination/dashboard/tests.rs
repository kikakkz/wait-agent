use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use askama::Template;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use tower::Service;

use super::{
    build_router, DashboardState, OperatorState, PendingLoginLink, Session, SessionStore,
    IDLE_TIMEOUT_SECS, SESSION_TTL_SECS,
};
use crate::infra::coordination::server::{now_unix, CoordinationStores};

const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "waitagent-dash-{name}-{}-{}",
        std::process::id(),
        std::thread::current()
            .name()
            .unwrap_or("test")
            .replace(':', "_")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn test_state(root: &std::path::Path, operator: OperatorState) -> Arc<DashboardState> {
    Arc::new(DashboardState {
        config: crate::infra::coordination::toml_store::CoordinationConfig {
            admin_email: Some("admin@qq.com".to_string()),
            ..crate::infra::coordination::toml_store::CoordinationConfig::default()
        },
        stores: Arc::new(CoordinationStores::open(root).unwrap()),
        admin_stdout: true,
        operator,
        node_id: "node-x".to_string(),
        relay_url: Some("http://127.0.0.1:7479/".to_string()),
        started: std::time::Instant::now(),
        sessions: super::SessionStore::new(),
        pending_links: super::PendingLinkStore::new(),
        login_cooldowns: std::sync::Mutex::new(std::collections::HashMap::new()),
        login_log: std::sync::Mutex::new(Vec::new()),
    })
}

fn peer(request: Request<Body>, ip: IpAddr) -> Request<Body> {
    let (mut parts, body) = request.into_parts();
    parts.extensions.insert(ConnectInfo(ip));
    Request::from_parts(parts, body)
}

async fn call(
    router: &Router,
    request: Request<Body>,
) -> (StatusCode, String, axum::http::HeaderMap) {
    let response = router.clone().call(request).await.expect("router call");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        headers,
    )
}

fn json_request(path: &str, body: serde_json::Value, ip: IpAddr) -> Request<Body> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::HOST, "dash.example")
        .header(header::USER_AGENT, "test-agent")
        .body(Body::from(body.to_string()))
        .unwrap();
    peer(request, ip)
}

async fn login(
    state: &Arc<DashboardState>,
    router: &Router,
    fingerprint: &str,
) -> (String, String) {
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
    let (status, _, _) = call(
        router,
        json_request(
            "/dashboard/api/login-request",
            serde_json::json!({"email": "admin@qq.com", "fingerprint": fingerprint}),
            ip,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let url = state
        .login_log
        .lock()
        .expect("poisoned")
        .last()
        .unwrap()
        .clone();
    let token = url.split("token=").nth(1).unwrap().to_string();
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/dashboard/api/login-redeem?token={token}"))
        .header(header::USER_AGENT, "test-agent")
        .header("x-login-fingerprint", fingerprint)
        .body(Body::empty())
        .unwrap();
    let (status, _, headers) = call(router, peer(request, ip)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let csrf = headers
        .get("x-csrf-token")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    (cookie, csrf)
}

fn authed(path: &str, cookie: &str, csrf: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::COOKIE, cookie);
    if let Some(csrf) = csrf {
        builder = builder.header("x-csrf-token", csrf);
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn login_request_mints_a_device_bound_link_and_cools_down() {
    let root = temp_root("login-request");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let router = build_router(state.clone());
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));

    let (status, body, _) = call(
        &router,
        json_request(
            "/dashboard/api/login-request",
            serde_json::json!({"email": "admin@qq.com", "fingerprint": "dev-1"}),
            ip,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"ok":true}"#);
    assert_eq!(state.login_log.lock().expect("poisoned").len(), 1);

    let (status, body, _) = call(
        &router,
        json_request(
            "/dashboard/api/login-request",
            serde_json::json!({"email": "admin@qq.com", "fingerprint": "dev-1"}),
            ip,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "cooldown: {body}");

    let (status, body, _) = call(
        &router,
        json_request(
            "/dashboard/api/login-request",
            serde_json::json!({"email": "someone-else@qq.com", "fingerprint": "dev-9"}),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "no enumeration: {body}");
    assert_eq!(
        state.login_log.lock().expect("poisoned").len(),
        1,
        "a non-admin mailbox mints no link"
    );
    crate::infra::best_effort::remove_dir_all(&root);
}

#[tokio::test]
async fn login_redeem_burns_the_link_and_fails_closed_on_device_mismatch() {
    let root = temp_root("redeem");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let router = build_router(state.clone());
    let (cookie, _) = login(&state, &router, "dev-1").await;

    let url = state.login_log.lock().expect("poisoned")[0].clone();
    let token = url.split("token=").nth(1).unwrap();
    let other_device = Request::builder()
        .method(Method::GET)
        .uri(format!("/dashboard/api/login-redeem?token={token}"))
        .header(header::USER_AGENT, "another-agent")
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = call(
        &router,
        peer(other_device, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "device binding: {body}");

    let same_device = Request::builder()
        .method(Method::GET)
        .uri(format!("/dashboard/api/login-redeem?token={token}"))
        .header(header::USER_AGENT, "test-agent")
        .header("x-login-fingerprint", "dev-1")
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = call(
        &router,
        peer(same_device, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7))),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "one-time link is burned: {body}"
    );

    let unknown = Request::builder()
        .method(Method::GET)
        .uri("/dashboard/api/login-redeem?token=deadbeef")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = call(&router, peer(unknown, PEER)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let overview = Request::builder()
        .uri("/dashboard/api/overview")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = call(&router, overview).await;
    assert_eq!(status, StatusCode::OK, "the redeemed session works: {body}");
    crate::infra::best_effort::remove_dir_all(&root);
}

#[tokio::test]
async fn expired_links_fail_closed() {
    let root = temp_root("expired");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let router = build_router(state.clone());
    let now = now_unix();
    state.pending_links.insert(
        "stale".to_string(),
        PendingLoginLink {
            email: "admin@qq.com".to_string(),
            created_at: now.saturating_sub(1200),
            expires_at: now.saturating_sub(60),
            ip: PEER,
            user_agent: "test-agent".to_string(),
            fingerprint: "dev-1".to_string(),
            used: false,
        },
    );
    let request = Request::builder()
        .method(Method::GET)
        .uri("/dashboard/api/login-redeem?token=stale")
        .header(header::USER_AGENT, "test-agent")
        .header("x-login-fingerprint", "dev-1")
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = call(&router, peer(request, PEER)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expired: {body}");
    crate::infra::best_effort::remove_dir_all(&root);
}

#[tokio::test]
async fn sessions_gate_reads_and_csrf_gates_mutations() {
    let root = temp_root("csrf");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let router = build_router(state.clone());
    let (cookie, csrf) = login(&state, &router, "dev-1").await;

    let anonymous = Request::builder()
        .uri("/dashboard/api/overview")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = call(&router, anonymous).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let overview = Request::builder()
        .uri("/dashboard/api/overview")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, body, headers) = call(&router, overview).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["node_id"], "node-x");
    assert!(headers.get("x-csrf-token").is_some());

    let (status, _, _) = call(&router, authed("/dashboard/api/heartbeat", &cookie, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "heartbeat without csrf");
    let (status, _, _) = call(
        &router,
        authed("/dashboard/api/heartbeat", &cookie, Some("wrong")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "wrong csrf");
    let (status, body, _) = call(
        &router,
        authed("/dashboard/api/heartbeat", &cookie, Some(&csrf)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    crate::infra::best_effort::remove_dir_all(&root);
}

#[tokio::test]
async fn idle_and_absolute_expiry_invalidate_sessions() {
    let store = SessionStore::new();
    let now = now_unix();
    store.insert(
        "idle".to_string(),
        session_at(now, now + SESSION_TTL_SECS, now - IDLE_TIMEOUT_SECS - 1),
    );
    store.insert(
        "expired".to_string(),
        session_at(now - SESSION_TTL_SECS - 10, now - 5, now),
    );
    store.insert(
        "live".to_string(),
        session_at(now, now + SESSION_TTL_SECS, now),
    );
    assert_eq!(store.len(), 3);
    store.sweep(now, IDLE_TIMEOUT_SECS);
    assert!(store.get("idle").is_none());
    assert!(store.get("expired").is_none());
    assert!(store.get("live").is_some());
}

#[tokio::test]
async fn approve_and_revoke_match_the_admin_alpn_semantics() {
    let root = temp_root("admin-parity");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let router = build_router(state.clone());
    let (cookie, csrf) = login(&state, &router, "dev-1").await;
    let node_key = iroh::SecretKey::generate();
    let node_id = node_key.public().to_string();
    let request = state
        .stores
        .requests
        .create(&node_id, std::time::Duration::from_secs(900), now_unix())
        .unwrap();

    let (status, body, _) = call(
        &router,
        authed(
            &format!("/dashboard/api/requests/{}/approve", request.id),
            &cookie,
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["ok"], true);
    let link = value["link"].as_str().unwrap();
    assert!(link.starts_with("waitagent://join?token="));
    assert_eq!(
        state
            .stores
            .requests
            .load(&request.id)
            .unwrap()
            .state
            .as_str(),
        "approved"
    );

    state
        .stores
        .registry
        .register(&node_id, now_unix())
        .unwrap();
    state.stores.presence.record(
        node_id.clone(),
        iroh::EndpointAddr::new(node_key.public()),
        None,
        now_unix(),
    );
    let (status, body, _) = call(
        &router,
        authed(
            &format!("/dashboard/api/nodes/{node_id}/revoke"),
            &cookie,
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!state.stores.registry.is_authorized(&node_id));
    assert!(state.stores.presence.get(&node_id).is_none());

    let refused = test_state(&root, OperatorState::Refused("not listed".to_string()));
    let refused_router = build_router(refused.clone());
    let (refused_cookie, refused_csrf) = login(&refused, &refused_router, "dev-2").await;
    let (status, body, _) = call(
        &refused_router,
        authed(
            &format!("/dashboard/api/requests/{}/approve", request.id),
            &refused_cookie,
            Some(&refused_csrf),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    crate::infra::best_effort::remove_dir_all(&root);
}

#[tokio::test]
async fn nodes_merges_registry_with_presence() {
    let root = temp_root("nodes");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let router = build_router(state.clone());
    let (cookie, _) = login(&state, &router, "dev-1").await;
    let node_key = iroh::SecretKey::generate();
    let node_id = node_key.public().to_string();
    state
        .stores
        .registry
        .register(&node_id, now_unix())
        .unwrap();
    state.stores.presence.record(
        node_id.clone(),
        iroh::EndpointAddr::new(node_key.public()),
        None,
        now_unix(),
    );

    let request = Request::builder()
        .uri("/dashboard/api/nodes")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = call(&router, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    let node = &value["nodes"][0];
    assert_eq!(node["node_id"], node_id);
    assert_eq!(node["online"], true);
    assert!(node["last_seen"].is_number());
    crate::infra::best_effort::remove_dir_all(&root);
}

#[tokio::test]
async fn dispatch_serves_relay_and_dashboard_on_one_port() {
    let root = temp_root("dispatch");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let router = build_router(state);
    let relay = crate::infra::coordination::server::embedded_relay_service_for_test();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        crate::infra::coordination::server::http_accept_loop_for_test(
            listener,
            Some(relay),
            router,
        )
        .await;
    });

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(b"GET /relay HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = Vec::new();
    use tokio::io::AsyncReadExt;
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 400") || text.starts_with("HTTP/1.0 400"),
        "the relay arm answers (missing upgrade headers): {text}"
    );

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            b"GET /dashboard/api/overview HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 401"),
        "the dashboard arm answers: {text}"
    );
    crate::infra::best_effort::remove_dir_all(&root);
}

#[tokio::test]
async fn pages_render_with_fixture_state() {
    let root = temp_root("pages-render");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let node_key = iroh::SecretKey::generate();
    let node_id = node_key.public().to_string();
    let request = state
        .stores
        .requests
        .create(&node_id, std::time::Duration::from_secs(900), now_unix())
        .unwrap();
    state
        .stores
        .registry
        .register(&node_id, now_unix())
        .unwrap();
    state.stores.presence.record(
        node_id.clone(),
        iroh::EndpointAddr::new(node_key.public()),
        None,
        now_unix(),
    );

    let overview = super::pages::OverviewTemplate {
        csrf: "csrf-1".to_string(),
        node_id: "node-x".to_string(),
        relay_url: "http://127.0.0.1:7479/".to_string(),
        registry_count: 1,
        online_count: 1,
        pending_count: 1,
        version: "test".to_string(),
    }
    .render()
    .unwrap();
    assert!(overview.contains("node-x"), "server identity renders");
    assert!(overview.contains("csrf-1"), "csrf meta renders");
    assert!(
        overview.contains("registry-count"),
        "counter mount points render"
    );

    let requests = super::pages::RequestsTemplate {
        csrf: "csrf-1".to_string(),
        rows: vec![super::pages::RequestRow {
            id: request.id.clone(),
            id_short: request.id.chars().take(8).collect(),
            node_id_short: node_id.chars().take(8).collect(),
            state: "pending".to_string(),
            created_at: request.created_at,
            age: "<1m".to_string(),
        }],
    }
    .render()
    .unwrap();
    assert!(
        requests.contains("data-approve=\""),
        "approve button renders"
    );
    assert!(requests.contains("pending"));

    let nodes = super::pages::NodesTemplate {
        csrf: "csrf-1".to_string(),
        rows: vec![super::pages::NodeRow {
            node_id: node_id.clone(),
            node_id_short: node_id.chars().take(8).collect(),
            online: true,
            last_seen: "<1m".to_string(),
            addr: "—".to_string(),
        }],
    }
    .render()
    .unwrap();
    assert!(nodes.contains("data-revoke=\""), "revoke button renders");
    assert!(nodes.contains("dot-on"), "online marker renders");

    let login = super::pages::LoginTemplate {
        csrf: String::new(),
    }
    .render()
    .unwrap();
    assert!(
        login.contains("login-request"),
        "login form posts to the API"
    );
    assert!(
        login.contains("X-Login-Fingerprint"),
        "redeem sends the fingerprint header"
    );
    assert!(
        login.contains("/dashboard/"),
        "success redirects to the dashboard"
    );
    crate::infra::best_effort::remove_dir_all(&root);
}

#[tokio::test]
async fn page_routes_sit_behind_the_session_gate() {
    let root = temp_root("pages-gate");
    let state = test_state(&root, OperatorState::Authorized("op".into()));
    let router = build_router(state.clone());
    let (cookie, csrf) = login(&state, &router, "dev-1").await;

    let anonymous = Request::builder()
        .uri("/dashboard/")
        .body(Body::empty())
        .unwrap();
    let (status, _, headers) = call(&router, anonymous).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "unauthenticated pages redirect"
    );
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/dashboard/login");

    let authed_overview = Request::builder()
        .uri("/dashboard/")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, body, headers) = call(&router, authed_overview).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html"),
        "pages render as HTML"
    );
    assert!(body.contains("node-x"), "SSR state renders");
    assert!(
        body.contains(&csrf),
        "the page carries the session csrf token"
    );

    for path in ["/dashboard/requests", "/dashboard/nodes"] {
        let authed = Request::builder()
            .uri(path)
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = call(&router, authed).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    let login_page = Request::builder()
        .uri("/dashboard/login")
        .body(Body::empty())
        .unwrap();
    let (status, body) = {
        let (status, body, _) = call(&router, login_page).await;
        (status, body)
    };
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Operator sign-in"));

    let emailed = state
        .login_log
        .lock()
        .expect("poisoned")
        .last()
        .unwrap()
        .clone();
    assert!(
        emailed.contains("/dashboard/login?token="),
        "emailed links target the login page: {emailed}"
    );
    crate::infra::best_effort::remove_dir_all(&root);
}

fn session_at(created_at: u64, expires_at: u64, last_heartbeat: u64) -> Session {
    Session {
        created_at,
        expires_at,
        last_heartbeat,
        ip: PEER,
        user_agent: String::new(),
        fingerprint: String::new(),
        csrf: "csrf".to_string(),
    }
}
