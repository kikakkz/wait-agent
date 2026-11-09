//! Coordination dashboard backend (issue #260; see docs/coordination-design.md).

pub mod api;
pub mod auth;
pub mod pages;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::routing::{get, post};
use axum::Router;

use crate::infra::coordination::server::CoordinationStores;
use crate::infra::coordination::toml_store::CoordinationConfig;

pub const SESSION_COOKIE: &str = "waitagent_dashboard_session";
pub const SESSION_TTL_SECS: u64 = 12 * 60 * 60;
pub const IDLE_TIMEOUT_SECS: u64 = 90;
pub const LOGIN_LINK_TTL_SECS: u64 = 15 * 60;
pub const LOGIN_COOLDOWN_SECS: u64 = 60;
pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
/// One authenticated dashboard session, fully tracked server-side.
pub struct Session {
    pub created_at: u64,
    pub expires_at: u64,
    pub last_heartbeat: u64,
    pub ip: IpAddr,
    pub user_agent: String,
    pub fingerprint: String,
    pub csrf: String,
}

impl Session {
    /// Absolute TTL plus idle window, both against `now`.
    pub fn is_valid(&self, now: u64, idle_timeout_secs: u64) -> bool {
        self.expires_at > now && !self.is_idle(now, idle_timeout_secs)
    }

    fn is_idle(&self, now: u64, idle_timeout_secs: u64) -> bool {
        self.last_heartbeat.saturating_add(idle_timeout_secs) <= now
    }
}

#[derive(Debug, Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, token: String, session: Session) {
        self.sessions
            .lock()
            .expect("poisoned")
            .insert(token, session);
    }

    pub fn get(&self, token: &str) -> Option<Session> {
        self.sessions.lock().expect("poisoned").get(token).cloned()
    }

    pub fn touch(&self, token: &str, now: u64) {
        if let Some(session) = self.sessions.lock().expect("poisoned").get_mut(token) {
            session.last_heartbeat = now;
        }
    }

    /// Drops every session past its absolute TTL or idle window.
    pub fn sweep(&self, now: u64, idle_timeout_secs: u64) {
        self.sessions
            .lock()
            .expect("poisoned")
            .retain(|_, session| session.is_valid(now, idle_timeout_secs));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("poisoned").len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A minted but not yet redeemed magic-link login token, device-bound at request time.
pub struct PendingLoginLink {
    pub email: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub ip: IpAddr,
    pub user_agent: String,
    pub fingerprint: String,
    pub used: bool,
}

impl PendingLoginLink {
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

#[derive(Debug, Default)]
pub struct PendingLinkStore {
    links: Mutex<HashMap<String, PendingLoginLink>>,
}

impl PendingLinkStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, token: String, link: PendingLoginLink) {
        self.links.lock().expect("poisoned").insert(token, link);
    }

    pub fn get(&self, token: &str) -> Option<PendingLoginLink> {
        self.links.lock().expect("poisoned").get(token).cloned()
    }

    pub fn mark_used(&self, token: &str) {
        if let Some(link) = self.links.lock().expect("poisoned").get_mut(token) {
            link.used = true;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Whether the serve process may perform dashboard admin actions under its own key.
pub enum OperatorState {
    Authorized(String),
    Refused(String),
}

/// Shared dashboard state; nothing here survives a restart.
pub struct DashboardState {
    pub config: CoordinationConfig,
    pub stores: Arc<CoordinationStores>,
    pub admin_stdout: bool,
    pub operator: OperatorState,
    pub node_id: String,
    pub relay_url: Option<String>,
    pub started: Instant,
    pub sessions: SessionStore,
    pub pending_links: PendingLinkStore,
    pub login_cooldowns: Mutex<HashMap<IpAddr, u64>>,
    /// Every minted login link, newest last; the serve process prints these
    /// when `admin_stdout` is set, tests and the frontend slice read them here.
    pub login_log: Mutex<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverviewCounts {
    pub registry_count: usize,
    pub online_count: usize,
    pub pending_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// One registry node merged with its presence state.
pub struct RegistryNode {
    pub node_id: String,
    pub online: bool,
    pub last_seen: Option<u64>,
    pub relay_url: Option<String>,
    pub direct_addresses: Vec<String>,
}

impl DashboardState {
    pub fn overview_counts(&self, now: u64) -> OverviewCounts {
        let requests = self.stores.requests.list().unwrap_or_default();
        let pending = requests
            .iter()
            .filter(|request| {
                request.state == crate::infra::coordination::enrollment::RequestState::Pending
            })
            .count();
        OverviewCounts {
            registry_count: self
                .stores
                .registry
                .list()
                .map(|list| list.len())
                .unwrap_or(0),
            online_count: self
                .stores
                .presence
                .online(now, self.config.presence_timeout())
                .len(),
            pending_count: pending,
        }
    }

    /// Registry entries merged with online presence, sorted by node id.
    pub fn registry_nodes(&self, now: u64) -> Result<Vec<RegistryNode>, std::io::Error> {
        let online = self
            .stores
            .presence
            .online(now, self.config.presence_timeout());
        let mut nodes = self
            .stores
            .registry
            .list()
            .map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
            })?
            .into_iter()
            .map(|node_id| {
                let presence = online.iter().find(|entry| entry.node_id == node_id);
                RegistryNode {
                    node_id,
                    online: presence.is_some(),
                    last_seen: presence.as_ref().map(|entry| entry.last_seen),
                    relay_url: presence.and_then(|entry| {
                        entry.addr.addrs.iter().find_map(|addr| match addr {
                            iroh::TransportAddr::Relay(url) => Some(url.to_string()),
                            _ => None,
                        })
                    }),
                    direct_addresses: presence
                        .map(|entry| {
                            entry
                                .addr
                                .addrs
                                .iter()
                                .filter_map(|addr| match addr {
                                    iroh::TransportAddr::Ip(ip) => Some(ip.to_string()),
                                    _ => None,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            })
            .collect::<Vec<_>>();
        nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(nodes)
    }
}

pub fn build_router(state: Arc<DashboardState>) -> Router {
    Router::new()
        .route("/dashboard/api/login-request", post(api::login_request))
        .route("/dashboard/api/login-redeem", get(api::login_redeem))
        .route("/dashboard/api/heartbeat", post(api::heartbeat))
        .route("/dashboard/api/overview", get(api::overview))
        .route("/dashboard/api/requests", get(api::requests))
        .route("/dashboard/api/nodes", get(api::nodes))
        .route("/dashboard", get(pages::overview_page))
        .route("/dashboard/", get(pages::overview_page))
        .route("/dashboard/login", get(pages::login_page))
        .route("/dashboard/requests", get(pages::requests_page))
        .route("/dashboard/nodes", get(pages::nodes_page))
        .route(
            "/dashboard/api/requests/{id}/approve",
            post(api::approve_request),
        )
        .route("/dashboard/api/nodes/{id}/revoke", post(api::revoke_node))
        .route("/ping", get(api::relay_probe))
        .with_state(state)
}

/// Periodically drops expired sessions and stale login cooldowns.
pub fn spawn_sweep_task(state: Arc<DashboardState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
            let now = crate::infra::coordination::server::now_unix();
            state.sessions.sweep(now, IDLE_TIMEOUT_SECS);
            state
                .login_cooldowns
                .lock()
                .expect("poisoned")
                .retain(|_, at| at.saturating_add(LOGIN_COOLDOWN_SECS) > now);
        }
    })
}

#[cfg(test)]
mod tests;
