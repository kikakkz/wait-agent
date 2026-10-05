//! `waitagent web serve`: the WebUI service runtime (issue #131, slice 1).
//!
//! Two halves, both thin over existing facilities:
//!
//! * Enrollment: the service owns node credentials generated under
//!   `waitagent_home()` (distinct file names from the interactive node's, so
//!   the two identities never clash). At startup it mints a one-time invite
//!   over the relay's local admin socket — the same channel `relay invite`
//!   uses — and runs the standard `relay join` enrollment session against
//!   the pinned relay, then keeps a persistent link with the stock
//!   [`RelayClient`] (register/heartbeat/backoff-reconnect). Every relay
//!   interaction travels the loopback node<->relay wire protocol; nothing
//!   reaches into the relay process.
//! * HTTP: an axum skeleton with a single `GET /healthz` route. Handlers
//!   stay non-blocking and share no mutable state (domain-web); later
//!   slices add the dashboard data plane and authentication on top.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::host::ssh::remote_host_home::waitagent_home;
use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_admin::relay_admin_addr;
use crate::infra::relay_client::{
    RelayClient, RelayClientConfig, RelayClientEvent, RelayClientHandle,
};
use crate::infra::relay_join::{join_relay, parse_relay_address};
use crate::infra::relay_toml_store::{RelayTomlConfig, RelayTomlStoreError};
use crate::platform::remote_ipc::{RemoteControlAddr, RemoteControlAsyncStream};
use crate::web::auth::routes::{
    auth_middleware, heartbeat, login_form, login_page, magic_link, AuthState, WebState,
};
use crate::web::auth::token::WebAuthKeys;
use crate::web::config::{WebuiConfig, WebuiConfigError};
use crate::web::dashboard::{dashboard, invite, remove};

/// Default web listen port. The default bind is `0.0.0.0` (issue #131 v2:
/// public deployment; the magic-link auth from slice 3 protects the
/// dashboard, and `web serve` prints an exposure warning for non-loopback
/// binds).
pub const DEFAULT_WEB_LISTEN_PORT: u16 = 8788;

/// Capacity of the relay-client lifecycle event queue.
const EVENT_QUEUE: usize = 16;

/// What `web serve` needs to run: where to listen, which node identity to
/// enroll, and where the pinned-relay config lives.
#[derive(Debug, Clone)]
pub struct WebServeConfig {
    /// HTTP listen address.
    pub listen: SocketAddr,
    /// The web service's own node credentials, generated on first use.
    pub credentials: NodeCredentialPaths,
    /// Path of the pinned relay config (`relay.toml`).
    pub relay_toml_path: PathBuf,
}

impl WebServeConfig {
    /// The CLI default: the v2 default bind (0.0.0.0, public deployment with
    /// magic-link auth), credentials and relay pin under `waitagent_home()`.
    pub fn from_waitagent_home() -> Self {
        let home = waitagent_home();
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], DEFAULT_WEB_LISTEN_PORT)),
            credentials: NodeCredentialPaths {
                key_path: home.join("web-node.key"),
                cert_path: home.join("web-node.crt"),
            },
            relay_toml_path: RelayTomlConfig::default_path(),
        }
    }
}

/// A web node enrolled and linked against the pinned relay.
pub struct WebRelayLink {
    /// The pinned relay config the link registered with.
    pub relay: RelayTomlConfig,
    /// This web node's certificate fingerprint (its node id on the relay).
    pub node_fingerprint: String,
    /// Lifecycle events of the persistent link (`Connected`, ...). Kept
    /// alive so `RelayClient`'s event queue never fills up.
    #[allow(dead_code)]
    // Read by the integration tests; the CLI runtime holds the receiver
    // alive without draining it, same as `relay serve` drops the relay
    // lifecycle receiver.
    pub events: mpsc::Receiver<RelayClientEvent>,
    /// The persistent relay client; dropping it (or
    /// [`RelayClientHandle::cancel`]) stops the link and joins the client
    /// thread.
    #[allow(dead_code)]
    // Held for its Drop side effect; the CLI runtime cancels it on shutdown.
    pub client: RelayClientHandle,
}

/// Errors of the web service runtime.
#[derive(Debug, Error)]
pub enum WebServeError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("web node credentials error: {0}")]
    Credentials(#[from] node_credentials::NodeCredentialsError),
    #[error("relay.toml error: {0}")]
    RelayToml(#[from] RelayTomlStoreError),
    #[error(
        "this machine has not joined a relay ({0} is missing); run `waitagent relay join <address> <token>` first"
    )]
    RelayTomlMissing(PathBuf),
    #[error("invalid relay address {0:?}: {1}")]
    RelayAddress(String, String),
    #[error("relay admin error: {0}")]
    Admin(String),
    #[error("relay enrollment (join) error: {0}")]
    Join(#[from] crate::infra::relay_join::RelayJoinError),
    #[error("webui config error: {0}")]
    WebuiConfig(#[from] WebuiConfigError),
    #[error("web auth keys error: {0}")]
    AuthKey(#[from] crate::web::auth::token::AuthKeyError),
}

/// Enrolls this web service into the pinned relay and opens the persistent
/// link. Idempotent by construction: every startup mints a fresh one-time
/// invite and re-runs the standard enrollment session, which refreshes the
/// whitelist entry for this node's certificate.
pub async fn enroll_and_link(config: &WebServeConfig) -> Result<WebRelayLink, WebServeError> {
    let node_fingerprint = node_credentials::ensure_credentials(&config.credentials)?;
    let pinned = RelayTomlConfig::load(&config.relay_toml_path)?
        .ok_or_else(|| WebServeError::RelayTomlMissing(config.relay_toml_path.clone()))?;
    let previous_heartbeat = pinned.heartbeat_interval_secs;

    let token = admin_invite_token(&pinned.address).await?;
    join_relay(
        &pinned.address,
        &token,
        &config.credentials,
        &config.relay_toml_path,
    )
    .await?;

    // `join_relay` rewrites the pin with its enrollment defaults; an
    // operator's heartbeat override survives a service restart.
    let mut relay = RelayTomlConfig::load(&config.relay_toml_path)?
        .ok_or_else(|| WebServeError::RelayTomlMissing(config.relay_toml_path.clone()))?;
    if previous_heartbeat.is_some() && relay.heartbeat_interval_secs.is_none() {
        relay.heartbeat_interval_secs = previous_heartbeat;
        relay.save(&config.relay_toml_path)?;
    }

    let client_config =
        RelayClientConfig::from_relay_toml(relay.clone(), config.credentials.clone());
    let (event_tx, events) = mpsc::channel(EVENT_QUEUE);
    let client = RelayClient::spawn(client_config, event_tx);
    Ok(WebRelayLink {
        relay,
        node_fingerprint,
        events,
        client,
    })
}

/// The axum application: `GET /healthz` answers 200 (open, the slice-1 e2e
/// anchor), `GET /` renders the dashboard, and `/login` + `/auth/magic` run
/// the magic-link flow; the session middleware guards `/` and `/api/*`
/// (slices 2-3). Everything else 404s from the default fallback.
pub fn build_router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(dashboard))
        .route("/login", get(login_page))
        .route("/auth/magic", get(magic_link).post(login_form))
        .route("/api/heartbeat", post(heartbeat))
        .route("/api/invite", post(invite))
        .route("/api/remove", post(remove))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Enroll-then-serve, driven by the CLI: loads the WebUI deployment config
/// and the token keys, links the relay, binds the HTTP listener, and serves
/// until Ctrl-C.
pub async fn run(config: &WebServeConfig) -> Result<(), WebServeError> {
    let webui_config = WebuiConfig::load(&WebuiConfig::default_path())?;
    let keys = WebAuthKeys::load_or_generate(&waitagent_home().join("web-auth.key"))?;
    let link = enroll_and_link(config).await?;
    let listener = TcpListener::bind(config.listen).await?;
    if !config.listen.ip().is_loopback() {
        println!(
            "WARNING: the dashboard listens on {} and is reachable from the network.",
            listener.local_addr()?
        );
        println!(
            "WARNING: access requires the admin mailbox magic link, but bind \
             non-loopback only when you mean to expose this machine."
        );
    }
    println!("web listening on {}", listener.local_addr()?);
    println!("web node fingerprint: {}", link.node_fingerprint);
    println!("relay: {}", link.relay.address);

    let auth = AuthState::new(keys, webui_config);
    let state = Arc::new(WebState::new(link.client, auth));
    axum::serve(
        listener,
        build_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Mints a one-time enrollment token over the relay's local admin socket,
/// the same request `relay invite` sends. The admin address derives from
/// the relay listen address, which the web service cannot know directly, so
/// it tries the address pinned in relay.toml first and the default
/// `0.0.0.0:<port>` listen shape second — both are loopback-local.
async fn admin_invite_token(relay_address: &str) -> Result<String, WebServeError> {
    let mut attempts = Vec::new();
    for candidate in admin_candidates(relay_address)? {
        match admin_request(&candidate, r#"{"command":"invite"}"#).await {
            Ok(body) => match parse_invite_token(&body) {
                Ok(token) => return Ok(token),
                Err(error) => attempts.push(format!("{candidate}: {error}")),
            },
            Err(error) => attempts.push(format!("{candidate}: {error}")),
        }
    }
    Err(WebServeError::Admin(format!(
        "could not mint an enrollment token via the relay admin socket ({}); is the relay running on this machine?",
        attempts.join("; ")
    )))
}

/// Candidate admin socket addresses for a pinned relay address: the
/// relay.toml address itself (covers `--listen` relays joined by that same
/// address) and the default serve shape `0.0.0.0:<port>` (covers a default
/// `relay serve`). Named hosts only get the default shape — slice 1
/// enrolls against a loopback relay.
fn admin_candidates(relay_address: &str) -> Result<Vec<RemoteControlAddr>, WebServeError> {
    let (host, port) = parse_relay_address(relay_address).map_err(|error| {
        WebServeError::RelayAddress(relay_address.to_string(), error.to_string())
    })?;
    let mut candidates = Vec::new();
    if let Ok(ip) = host.parse::<IpAddr>() {
        candidates.push(relay_admin_addr(SocketAddr::new(ip, port)));
    }
    let default_shape = relay_admin_addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port));
    if !candidates.contains(&default_shape) {
        candidates.push(default_shape);
    }
    Ok(candidates)
}

/// One admin-socket request/response round trip. Mirrors
/// `command::relay_serve::admin_request`, but async-native: that one builds
/// its own current-thread runtime, which cannot be nested inside the web
/// service runtime.
async fn admin_request(addr: &RemoteControlAddr, request: &str) -> Result<String, String> {
    let mut stream = RemoteControlAsyncStream::connect(addr)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    stream.shutdown().await.map_err(|error| error.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .map_err(|error| error.to_string())?;
    Ok(response)
}

/// Extracts the raw token from an invite response body. Pure; the invite
/// answer is a JSON envelope whose `message` carries `token: <raw>` on its
/// own line.
fn parse_invite_token(body: &str) -> Result<String, String> {
    let response: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("invalid admin response: {error}"))?;
    if response["ok"] != true {
        return Err(format!(
            "admin rejected the invite request: {}",
            response["error"]
        ));
    }
    let message = response["message"]
        .as_str()
        .ok_or_else(|| "invite response lacks a message".to_string())?;
    message
        .lines()
        .find_map(|line| line.strip_prefix("token: "))
        .map(str::to_string)
        .ok_or_else(|| "invite response carries no token".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_token_extracts_from_message_lines() {
        let body = r#"{"ok":true,"message":"token: abc-def\nexpires_at: 123\nkind: one-time invite token"}"#;
        assert_eq!(parse_invite_token(body).expect("token"), "abc-def");
    }

    #[test]
    fn invite_token_rejects_error_envelopes_and_garbage() {
        let error_body = r#"{"ok":false,"error":"unknown admin command: \"bogus\""}"#;
        assert!(parse_invite_token(error_body)
            .expect_err("an error envelope has no token")
            .contains("admin rejected"));
        let no_token = r#"{"ok":true,"message":"expires_at: 123"}"#;
        assert!(parse_invite_token(no_token)
            .expect_err("a token-less message has no token")
            .contains("no token"));
        assert!(parse_invite_token("not json")
            .expect_err("garbage is not an envelope")
            .contains("invalid admin response"));
    }

    #[cfg(unix)]
    #[test]
    fn admin_candidates_cover_toml_and_default_listen_shapes() {
        let names = |addrs: &[RemoteControlAddr]| {
            addrs
                .iter()
                .map(|addr| {
                    addr.unix_path()
                        .expect("unix admin addr")
                        .file_name()
                        .expect("file name")
                        .to_str()
                        .expect("utf-8")
                        .to_string()
                })
                .collect::<Vec<_>>()
        };

        let loopback = admin_candidates("127.0.0.1:7475").expect("candidates");
        assert_eq!(
            names(&loopback),
            vec![
                "waitagent-relay-admin-127_0_0_1_7475.sock".to_string(),
                "waitagent-relay-admin-0_0_0_0_7475.sock".to_string(),
            ]
        );

        // A default-shape relay produces a single (deduplicated) candidate.
        let default_shape = admin_candidates("0.0.0.0:7475").expect("candidates");
        assert_eq!(
            names(&default_shape),
            vec!["waitagent-relay-admin-0_0_0_0_7475.sock".to_string()]
        );

        // Named hosts cannot be turned into a listen address; only the
        // default shape applies.
        let named = admin_candidates("relay.example:7475").expect("candidates");
        assert_eq!(
            names(&named),
            vec!["waitagent-relay-admin-0_0_0_0_7475.sock".to_string()]
        );
    }
}
