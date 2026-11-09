//! Dashboard API handlers (issue #260; see docs/coordination-design.md).

use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;

use crate::infra::coordination::dashboard::auth::{
    authenticate, device_binding_matches, mint_token, new_session, require_csrf, session_cookie,
};
use crate::infra::coordination::dashboard::{
    DashboardState, OperatorState, PendingLoginLink, LOGIN_COOLDOWN_SECS, LOGIN_LINK_TTL_SECS,
};
use crate::infra::coordination::server::now_unix;
use crate::infra::coordination::toml_store;

type SharedState = State<Arc<DashboardState>>;
type ApiResult = Result<Response<Body>, Response<Body>>;

fn json(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    response
}

fn ok(value: serde_json::Value) -> Response<Body> {
    json(StatusCode::OK, value)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    json(
        status,
        serde_json::json!({"ok": false, "error": message.into()}),
    )
}

fn unauthorized(message: &str) -> Response<Body> {
    json(
        StatusCode::UNAUTHORIZED,
        serde_json::json!({"ok": false, "error": message}),
    )
}

fn admin_refused(state: &DashboardState) -> Option<Response<Body>> {
    match &state.operator {
        OperatorState::Authorized(_) => None,
        OperatorState::Refused(message) => Some(error(StatusCode::CONFLICT, message.clone())),
    }
}

fn csrf_headers(session: &crate::infra::coordination::dashboard::Session) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-csrf-token", session.csrf.parse().unwrap());
    headers
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub fingerprint: String,
}

pub async fn login_request(
    State(state): SharedState,
    ConnectInfo(ip): ConnectInfo<IpAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> ApiResult {
    let Some(admin_email) = state.config.admin_email.clone() else {
        return Ok(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "dashboard login needs admin_email in coordination.toml",
        ));
    };
    let now = now_unix();
    let cooldown = state
        .login_cooldowns
        .lock()
        .expect("poisoned")
        .get(&ip)
        .copied()
        .unwrap_or_default();
    if cooldown.saturating_add(LOGIN_COOLDOWN_SECS) > now {
        return Ok(error(StatusCode::TOO_MANY_REQUESTS, "login cooldown"));
    }
    state
        .login_cooldowns
        .lock()
        .expect("poisoned")
        .insert(ip, now);

    if body.email.trim() == admin_email {
        let token = mint_token();
        let link = PendingLoginLink {
            email: admin_email.clone(),
            created_at: now,
            expires_at: now.saturating_add(LOGIN_LINK_TTL_SECS),
            ip,
            user_agent: user_agent(&headers),
            fingerprint: body.fingerprint.clone(),
            used: false,
        };
        let link_expires_at = link.expires_at;
        state.pending_links.insert(token.clone(), link);
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("127.0.0.1");
        let url = format!("http://{host}/dashboard/api/login-redeem?token={token}");
        state.login_log.lock().expect("poisoned").push(url.clone());
        if state.admin_stdout {
            println!("login_link: {url}");
        } else {
            let message = toml_store::dashboard_login_mail(&admin_email, &url, link_expires_at);
            let send = async {
                let preset = toml_store::smtp_endpoint(&admin_email)?;
                crate::web::auth::smtp::send(
                    preset.host,
                    preset.port,
                    preset.tls,
                    &admin_email,
                    state.config.mail_auth_code.as_deref().unwrap_or_default(),
                    &message,
                )
                .await
                .map_err(|error| error.to_string())
            };
            if let Err(failure) = send.await {
                eprintln!("dashboard login mail failed: {failure}");
            }
        }
    }
    Ok(ok(serde_json::json!({"ok": true})))
}

#[derive(Debug, Deserialize)]
pub struct RedeemQuery {
    pub token: String,
}

pub async fn login_redeem(
    State(state): SharedState,
    ConnectInfo(ip): ConnectInfo<IpAddr>,
    headers: HeaderMap,
    Query(query): Query<RedeemQuery>,
) -> ApiResult {
    let Some(link) = state.pending_links.get(&query.token) else {
        return Ok(error(StatusCode::FORBIDDEN, "unknown login token"));
    };
    let fingerprint = headers
        .get("x-login-fingerprint")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !device_binding_matches(&link, ip, &user_agent(&headers), fingerprint) {
        return Ok(error(
            StatusCode::FORBIDDEN,
            "login token is bound to another device",
        ));
    }
    let now = now_unix();
    if link.used {
        return Ok(error(StatusCode::FORBIDDEN, "login token already used"));
    }
    if link.is_expired(now) {
        return Ok(error(StatusCode::FORBIDDEN, "login token expired"));
    }
    state.pending_links.mark_used(&query.token);
    let session = new_session(ip, user_agent(&headers), fingerprint.to_string());
    state.sessions.insert(query.token.clone(), session.clone());
    let mut response = json(StatusCode::SEE_OTHER, serde_json::json!({"ok": true}));
    *response.headers_mut() = csrf_headers(&session);
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&query.token).parse().unwrap(),
    );
    response
        .headers_mut()
        .insert(header::LOCATION, "/dashboard/".parse().unwrap());
    Ok(response)
}

pub async fn heartbeat(State(state): SharedState, headers: HeaderMap) -> ApiResult {
    let session =
        authenticate(&state, &headers).map_err(|_| unauthorized("authentication required"))?;
    require_csrf(&session.session, &headers)
        .map_err(|_| error(StatusCode::FORBIDDEN, "csrf token mismatch"))?;
    state.sessions.touch(&session.token, now_unix());
    Ok(ok(serde_json::json!({"ok": true})))
}

pub async fn overview(State(state): SharedState, headers: HeaderMap) -> ApiResult {
    let session =
        authenticate(&state, &headers).map_err(|_| unauthorized("authentication required"))?;
    let now = now_unix();
    let requests = state.stores.requests.list().unwrap_or_default();
    let pending = requests
        .iter()
        .filter(|request| {
            request.state == crate::infra::coordination::enrollment::RequestState::Pending
        })
        .count();
    let registry_count = state
        .stores
        .registry
        .list()
        .map(|list| list.len())
        .unwrap_or(0);
    let online_count = state
        .stores
        .presence
        .online(now, state.config.presence_timeout())
        .len();
    let mut response = ok(serde_json::json!({
        "ok": true,
        "node_id": state.node_id,
        "relay_url": state.relay_url,
        "registry_count": registry_count,
        "online_count": online_count,
        "pending_count": pending,
        "uptime_secs": state.started.elapsed().as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
    }));
    response
        .headers_mut()
        .extend(csrf_headers(&session.session));
    Ok(response)
}

pub async fn requests(State(state): SharedState, headers: HeaderMap) -> ApiResult {
    let session =
        authenticate(&state, &headers).map_err(|_| unauthorized("authentication required"))?;
    let requests = state
        .stores
        .requests
        .list()
        .map_err(|failure| error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()))?;
    let mut response = ok(serde_json::json!({
        "ok": true,
        "requests": requests.iter().map(request_json).collect::<Vec<_>>(),
    }));
    response
        .headers_mut()
        .extend(csrf_headers(&session.session));
    Ok(response)
}

pub async fn nodes(State(state): SharedState, headers: HeaderMap) -> ApiResult {
    let session =
        authenticate(&state, &headers).map_err(|_| unauthorized("authentication required"))?;
    let now = now_unix();
    let online = state
        .stores
        .presence
        .online(now, state.config.presence_timeout());
    let registry = state
        .stores
        .registry
        .list()
        .map_err(|failure| error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()))?;
    let nodes = registry
        .iter()
        .map(|node_id| {
            let presence = online.iter().find(|entry| entry.node_id == *node_id);
            serde_json::json!({
                "node_id": node_id,
                "online": presence.is_some(),
                "last_seen": presence.as_ref().map(|entry| entry.last_seen),
                "addr": presence
                    .as_ref()
                    .map(|entry| serde_json::from_str::<serde_json::Value>(&crate::infra::mesh_probe::format_node_addr(&entry.addr)).unwrap_or_default()),
            })
        })
        .collect::<Vec<_>>();
    let mut response = ok(serde_json::json!({"ok": true, "nodes": nodes}));
    response
        .headers_mut()
        .extend(csrf_headers(&session.session));
    Ok(response)
}

pub async fn approve_request(
    State(state): SharedState,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    let session =
        authenticate(&state, &headers).map_err(|_| unauthorized("authentication required"))?;
    require_csrf(&session.session, &headers)
        .map_err(|_| error(StatusCode::FORBIDDEN, "csrf token mismatch"))?;
    if let Some(refused) = admin_refused(&state) {
        return Ok(refused);
    }
    let now = now_unix();
    match state.stores.requests.approve(&id, now) {
        Ok(request) => {
            let mut response = ok(serde_json::json!({
                "ok": true,
                "request_id": request.id,
                "link": toml_store::magic_link(&request),
            }));
            response
                .headers_mut()
                .extend(csrf_headers(&session.session));
            Ok(response)
        }
        Err(failure) => Ok(error(StatusCode::BAD_REQUEST, failure.to_string())),
    }
}

pub async fn revoke_node(
    State(state): SharedState,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    let session =
        authenticate(&state, &headers).map_err(|_| unauthorized("authentication required"))?;
    require_csrf(&session.session, &headers)
        .map_err(|_| error(StatusCode::FORBIDDEN, "csrf token mismatch"))?;
    if let Some(refused) = admin_refused(&state) {
        return Ok(refused);
    }
    if !state.stores.registry.is_authorized(&id) {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            format!("node {id} is not registered"),
        ));
    }
    state.stores.presence.remove(&id);
    match state.stores.registry.revoke(&id) {
        Ok(_) => {
            let mut response = ok(serde_json::json!({"ok": true, "node_id": id}));
            response
                .headers_mut()
                .extend(csrf_headers(&session.session));
            Ok(response)
        }
        Err(failure) => Ok(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            failure.to_string(),
        )),
    }
}

/// The relay latency probe used by net_report; any 2xx with an empty body.
pub async fn relay_probe() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    response
}

fn user_agent(headers: &HeaderMap) -> String {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn request_json(
    request: &crate::infra::coordination::enrollment::JoinRequest,
) -> serde_json::Value {
    serde_json::json!({
        "id": request.id,
        "node_id": request.node_id,
        "state": request.state.as_str(),
        "created_at": request.created_at,
        "expires_at": request.expires_at,
        "joined_at": request.joined_at,
    })
}
