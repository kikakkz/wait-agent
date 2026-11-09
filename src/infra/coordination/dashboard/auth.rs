//! Session and magic-link authentication for the dashboard (issue #260; see docs/coordination-design.md).

use std::net::IpAddr;

use axum::http::{HeaderMap, StatusCode};

use crate::infra::coordination::dashboard::{
    DashboardState, PendingLoginLink, Session, IDLE_TIMEOUT_SECS, SESSION_COOKIE, SESSION_TTL_SECS,
};
use crate::infra::coordination::server::now_unix;

pub fn mint_token() -> String {
    crate::infra::coordination::enrollment::mint_token()
}

pub fn session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/")
}

pub fn parse_session_cookie(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_string())
    })
}

/// An authenticated session resolved from the cookie store.
#[derive(Debug, Clone)]
pub struct AdminSession {
    pub token: String,
    pub session: Session,
}

/// Resolves the session token cookie to a live session; every gate
/// (existence, absolute TTL, idle window) fails closed.
pub fn authenticate(
    state: &DashboardState,
    headers: &HeaderMap,
) -> Result<AdminSession, StatusCode> {
    let token = parse_session_cookie(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let session = state
        .sessions
        .get(&token)
        .filter(|session| session.is_valid(now_unix(), IDLE_TIMEOUT_SECS))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    Ok(AdminSession { token, session })
}

/// Mutations require the per-session CSRF token in a header.
pub fn require_csrf(session: &Session, headers: &HeaderMap) -> Result<(), StatusCode> {
    match headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
    {
        Some(presented) if presented == session.csrf => Ok(()),
        _ => Err(StatusCode::FORBIDDEN),
    }
}

pub fn new_session(ip: IpAddr, user_agent: String, fingerprint: String) -> Session {
    let now = now_unix();
    Session {
        created_at: now,
        expires_at: now.saturating_add(SESSION_TTL_SECS),
        last_heartbeat: now,
        ip,
        user_agent,
        fingerprint,
        csrf: mint_token(),
    }
}

/// The device binding every login link carries: exact IP, exact User-Agent,
/// and the client fingerprint, all pinned at request time.
pub fn device_binding_matches(
    link: &PendingLoginLink,
    ip: IpAddr,
    user_agent: &str,
    fingerprint: &str,
) -> bool {
    link.ip == ip && link.user_agent == user_agent && link.fingerprint == fingerprint
}
