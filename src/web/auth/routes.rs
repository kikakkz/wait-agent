//! The auth HTTP surface (issue #131 slice 3): the login page, the
//! magic-link request/redeem pair, the session middleware, and the
//! heartbeat endpoint. All session state lives in memory (`AuthStores`);
//! the JWT keys sign and verify only — liveness (one-time magic jtis,
//! heartbeat-evicted session jtis) is the stores' job.
//!
//! Flow: `POST /auth/magic` (rate-limited, admin-email-only, sends the
//! link) → `GET /auth/magic?token=..` (verifies the JWT, consumes the jti,
//! binds the fingerprint, sets the session cookie, redirects to `/`) →
//! every `/` and `/api/*` request passes `auth_middleware` (JWT + jti +
//! fingerprint constant-time compare + `last_seen` refresh); the dashboard
//! page's inline script POSTs `/api/heartbeat` every 30s.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::header::{COOKIE, SET_COOKIE, USER_AGENT};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::infra::error_log::ERROR_LOG;
use crate::infra::relay_client::RelayClientHandle;
use crate::web::auth::fingerprint::{self, Probe};
use crate::web::auth::mail::{self, SmtpEndpoint};
use crate::web::auth::smtp::{self, MailMessage};
use crate::web::auth::store::{AuthStores, MagicRedeem, SessionTouch};
use crate::web::auth::token::{self, Claims, WebAuthKeys, MAGIC_TTL_SECS, SESSION_TTL_SECS};
use crate::web::config::WebuiConfig;

/// The dashboard page's heartbeat cadence (v3: 30s). The templates inline
/// the same value in their `setInterval` calls; the constant exists so the
/// parameter has a name in code.
#[allow(dead_code)]
pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// Upper bound when the middleware buffers a heartbeat body.
const HEARTBEAT_BODY_LIMIT: usize = 4096;

/// Everything the auth routes and middleware need, alongside the enrolled
/// relay client the dashboard queries.
pub struct WebState {
    pub client: RelayClientHandle,
    pub auth: AuthState,
}

impl WebState {
    pub fn new(client: RelayClientHandle, auth: AuthState) -> Self {
        Self { client, auth }
    }
}

/// The auth-only half of [`WebState`].
pub struct AuthState {
    pub keys: WebAuthKeys,
    pub stores: AuthStores,
    pub config: WebuiConfig,
}

impl AuthState {
    pub fn new(keys: WebAuthKeys, config: WebuiConfig) -> Self {
        Self {
            keys,
            stores: AuthStores::new(),
            config,
        }
    }
}

/// `GET /login`: the email form (probe fields are filled by the inline
/// script before submit).
pub(crate) async fn login_page() -> Response {
    HtmlLoginTemplate {
        message: None,
        error: false,
    }
    .into_response()
}

/// `POST /auth/magic`: rate-limit → admin-email check → mint → mail.
/// Wrong addresses get the same page as right ones (no oracle, no
/// third-party bombing); SMTP failures are user-visible AND logged.
pub(crate) async fn login_form(
    State(state): State<Arc<WebState>>,
    connect: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = match std::str::from_utf8(&body) {
        Ok(body) => body,
        Err(error) => {
            ERROR_LOG.log_debug(format!("[web] login form is not utf-8: {error}"));
            return (StatusCode::BAD_REQUEST, "bad form encoding\n").into_response();
        }
    };
    let form = parse_form(body);
    let email = form.get("email").cloned().unwrap_or_default();
    let ip = request_ip(&connect, &headers, &state.auth.config.trusted_proxies);

    if !state.auth.stores.allow_magic_request(ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "rate limited: at most 3 magic-link requests per 10 minutes\n",
        )
            .into_response();
    }

    if email
        .trim()
        .eq_ignore_ascii_case(&state.auth.config.admin_email)
    {
        let probe = probe_from_form(&form);
        let user_agent = user_agent(&headers);
        let fp = fingerprint::compute(&ip.to_string(), &user_agent, &probe);
        let jti = token::new_jti();
        let iat = token::now_unix();
        let claims = Claims {
            sub: "magic".to_string(),
            jti: jti.clone(),
            iat,
            exp: iat + MAGIC_TTL_SECS,
            fp: fp.clone(),
        };
        let signed = match state.auth.keys.sign(&claims) {
            Ok(signed) => signed,
            Err(error) => {
                ERROR_LOG.log_error(format!("[web] magic token sign failed: {error}"));
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error minting the magic link\n",
                )
                    .into_response();
            }
        };
        state
            .auth
            .stores
            .mint_magic(&jti, fp, probe, Duration::from_secs(MAGIC_TTL_SECS));

        let link = format!(
            "{}/auth/magic?token={signed}",
            state.auth.config.public_base_url
        );
        match send_magic_mail(&state.auth.config, &link).await {
            Ok(()) => {
                ERROR_LOG.log_debug(format!("[web] magic link mailed to {email}"));
            }
            Err(error) => {
                // The token stays minted but unmailed; it expires unused.
                // The user must see the failure (and it lands in error_log).
                ERROR_LOG.log_error(format!("[web] magic mail to {email} failed: {error}"));
                return mail_error_page(&format!("could not send the magic email: {error}"));
            }
        }
    }

    HtmlLoginTemplate {
        message: Some(
            "If this address is the administrator, a magic link is on its way.".to_string(),
        ),
        error: false,
    }
    .into_response()
}

/// `GET /auth/magic?token=...`: verify → consume → bind → session cookie →
/// redirect to `/`. Any fingerprint drift (other machine/browser/network,
/// per the literal same-machine rule) invalidates the token on the spot.
pub(crate) async fn magic_link(
    State(state): State<Arc<WebState>>,
    connect: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(token) = query.get("token") else {
        return (StatusCode::BAD_REQUEST, "missing token\n").into_response();
    };
    let claims = match state.auth.keys.verify(token, "magic") {
        Ok(claims) => claims,
        Err(error) => {
            ERROR_LOG.log_debug(format!("[web] magic token rejected: {error}"));
            return (StatusCode::FORBIDDEN, "magic link is invalid or expired\n").into_response();
        }
    };
    let MagicRedeem::Ok { fp, probe } = state.auth.stores.redeem_magic(&claims.jti) else {
        return (
            StatusCode::FORBIDDEN,
            "magic link is unknown, already used, or expired\n",
        )
            .into_response();
    };
    let ip = request_ip(&connect, &headers, &state.auth.config.trusted_proxies);
    let fp_now = fingerprint::compute(&ip.to_string(), &user_agent(&headers), &probe);
    if !fingerprint::constant_time_eq(&fp_now, &claims.fp) {
        ERROR_LOG.log_warn(format!(
            "[web] magic link for {} consumed by a mismatched fingerprint",
            claims.jti
        ));
        return (
            StatusCode::FORBIDDEN,
            "magic link was requested from a different machine or network\n",
        )
            .into_response();
    }

    let jti = token::new_jti();
    let iat = token::now_unix();
    let session = Claims {
        sub: "session".to_string(),
        jti: jti.clone(),
        iat,
        exp: iat + SESSION_TTL_SECS,
        fp: fp.clone(),
    };
    let signed = match state.auth.keys.sign(&session) {
        Ok(signed) => signed,
        Err(error) => {
            ERROR_LOG.log_error(format!("[web] session token sign failed: {error}"));
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error creating the session\n",
            )
                .into_response();
        }
    };
    state
        .auth
        .stores
        .mint_session(&jti, fp, probe, Duration::from_secs(SESSION_TTL_SECS));

    let cookie = format!("session={signed}; HttpOnly; SameSite=Strict; Path=/");
    let mut response = Redirect::to("/").into_response();
    if let Ok(value) = cookie.parse() {
        response.headers_mut().append(SET_COOKIE, value);
    }
    response
}

/// `POST /api/heartbeat`: the middleware already refreshed the session;
/// this just acknowledges (the dashboard script polls it every 30s).
pub(crate) async fn heartbeat() -> Response {
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

/// The session guard for `/` and `/api/*`. Public: `/healthz`, `/login`,
/// `/auth/magic`. Failures are 403 under `/api` and a redirect to the
/// login page everywhere else (browser-friendly).
pub(crate) async fn auth_middleware(
    State(state): State<Arc<WebState>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    if is_public_path(&path) {
        return next.run(request).await;
    }
    let Some(peer) = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0)
    else {
        ERROR_LOG.log_error("[web] request without ConnectInfo; refusing".to_string());
        return reject(&path);
    };
    let headers = request.headers().clone();

    // Heartbeat probes ride in the JSON body; buffer it, parse the probe,
    // and rebuild the request untouched for the handler.
    let (request, body_probe) = if path == "/api/heartbeat" {
        let (parts, body) = request.into_parts();
        match to_bytes(body, HEARTBEAT_BODY_LIMIT).await {
            Ok(bytes) => {
                let probe = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .map(|value| {
                        Probe::from_fields(
                            value["platform"].as_str(),
                            value["timezone"].as_str(),
                            value["language"].as_str(),
                            value["screen"].as_str(),
                        )
                    });
                (Request::from_parts(parts, Body::from(bytes)), probe)
            }
            Err(error) => {
                ERROR_LOG.log_debug(format!("[web] heartbeat body unreadable: {error}"));
                return reject(&path);
            }
        }
    } else {
        (request, None)
    };

    let Some(cookie) = session_cookie(&headers) else {
        return reject(&path);
    };
    let claims = match state.auth.keys.verify(&cookie, "session") {
        Ok(claims) => claims,
        Err(_) => return reject(&path),
    };
    let SessionTouch::Active { fp, probe } = state.auth.stores.touch_session(&claims.jti) else {
        return reject(&path);
    };
    let ip = request_ip(
        &ConnectInfo(peer),
        &headers,
        &state.auth.config.trusted_proxies,
    );
    let probe = body_probe.unwrap_or(probe);
    let fp_now = fingerprint::compute(&ip.to_string(), &user_agent(&headers), &probe);
    if !fingerprint::constant_time_eq(&fp_now, &fp) {
        ERROR_LOG.log_warn(format!(
            "[web] session {} used from a mismatched fingerprint; invalidating",
            claims.jti
        ));
        state.auth.stores.remove_session(&claims.jti);
        return reject(&path);
    }
    next.run(request).await
}

fn is_public_path(path: &str) -> bool {
    matches!(path, "/healthz" | "/login" | "/auth/magic")
}

fn reject(path: &str) -> Response {
    if path.starts_with("/api/") {
        (StatusCode::FORBIDDEN, "forbidden\n").into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        if let Some((name, value)) = pair.trim().split_once('=') {
            if name == "session" {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn user_agent(headers: &HeaderMap) -> String {
    headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn request_ip(
    connect: &ConnectInfo<SocketAddr>,
    headers: &HeaderMap,
    trusted: &[std::net::IpAddr],
) -> std::net::IpAddr {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    fingerprint::client_ip(connect.0.ip(), forwarded, trusted)
}

fn probe_from_form(form: &HashMap<String, String>) -> Probe {
    Probe::from_fields(
        form.get("platform").map(String::as_str),
        form.get("timezone").map(String::as_str),
        form.get("language").map(String::as_str),
        form.get("screen").map(String::as_str),
    )
}

async fn send_magic_mail(config: &WebuiConfig, link: &str) -> Result<(), String> {
    let SmtpEndpoint {
        host,
        port,
        tls,
        user,
    } = mail::smtp_endpoint(config)?;
    let message = MailMessage {
        from: config.admin_email.clone(),
        to: config.admin_email.clone(),
        subject: "waitagent dashboard magic link".to_string(),
        body: format!(
            "A login was requested for the waitagent dashboard.\n\nOpen this link within 10 minutes on the same machine and network:\n\n{link}\n\nIf this was not you, ignore this message.\n"
        ),
    };
    smtp::send(&host, port, tls, &user, &config.mail_auth_code, &message)
        .await
        .map_err(|error| error.to_string())
}

fn mail_error_page(message: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        HtmlLoginTemplate {
            message: Some(message.to_string()),
            error: true,
        }
        .render()
        .unwrap_or_else(|error| format!("login page render failed: {error}")),
    )
        .into_response()
}

/// The login page template (askama): dark/terminal style per
/// docs/ui-design.md §7, tiny inline probe script, no frameworks.
#[derive(Template)]
#[template(path = "login.html")]
struct HtmlLoginTemplate {
    message: Option<String>,
    error: bool,
}

impl IntoResponse for HtmlLoginTemplate {
    fn into_response(self) -> Response {
        match self.render() {
            Ok(html) => axum::response::Html(html).into_response(),
            Err(error) => {
                ERROR_LOG.log_error(format!("[web] login template render failed: {error}"));
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
            }
        }
    }
}

/// Parses an `application/x-www-form-urlencoded` body (no new dependency;
/// the auth form carries five small fields).
fn parse_form(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in body.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        out.insert(form_unescape(name), form_unescape(value));
    }
    out
}

fn form_unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|digits| u8::from_str_radix(digits, 16).ok())
                {
                    Some(decoded) => {
                        out.push(decoded);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_parsing_decodes_percent_and_plus() {
        let form = parse_form(
            "email=Admin%40example.com&platform=Linux+x86_64&timezone=Asia%2FShanghai&empty=&dangling",
        );
        assert_eq!(
            form.get("email").map(String::as_str),
            Some("Admin@example.com")
        );
        assert_eq!(
            form.get("platform").map(String::as_str),
            Some("Linux x86_64")
        );
        assert_eq!(
            form.get("timezone").map(String::as_str),
            Some("Asia/Shanghai")
        );
        assert_eq!(form.get("empty").map(String::as_str), Some(""));
        assert!(!form.contains_key("dangling"));
    }

    #[test]
    fn session_cookie_picks_the_session_pair() {
        let mut headers = HeaderMap::new();
        assert!(session_cookie(&headers).is_none());
        headers.insert(
            COOKIE,
            "other=1; session=abc.def.ghi; theme=dark".parse().unwrap(),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("abc.def.ghi"));
    }
}
