//! Relay local admin socket: the owner-only local channel for bootstrap
//! and emergency management (docs/relay-design.md 管理通道). Follows the
//! owner-control pattern in `platform::remote_ipc`: UDS under the temp dir
//! on Unix, TCP loopback with a derived port on Windows.
//!
//! Protocol: one JSON request per connection — the client writes the
//! request and half-closes; the server reads to EOF, answers once, and
//! closes (the exact idiom of the remote runtime owner control channel).
//! `status` queries the connection table; `shutdown` stops the relay
//! gracefully; `invite` mints enrollment tokens and `remove` revokes a
//! whitelisted node (whitelist entry + live link).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::relay_capacity::{RelayCapacityConfig, RelayUsage, SharedUsageMeter};
use crate::infra::relay_connection_table::RelayConnectionTable;
use crate::infra::relay_enrollment::{
    remove_authorized_node, EnrollmentTokenStore, DEFAULT_DEPLOY_TTL, DEFAULT_INVITE_TTL,
};
use crate::infra::relay_presence::PresenceHub;
use crate::infra::relay_routing::RoutingTable;
use crate::platform::remote_ipc::{
    cleanup_remote_listener, RemoteControlAddr, RemoteControlAsyncListener,
};

/// Default port offset for the Windows loopback admin listener (the
/// ingress/owner sockets use 10_000/10_001).
#[cfg(windows)]
const WINDOWS_ADMIN_PORT_OFFSET: u16 = 20_000;

/// Returns the admin socket address for a relay listening on `listen`.
/// Computed from the resolved listen address so ephemeral (port 0) relays
/// get a stable unique address.
pub fn relay_admin_addr(listen: SocketAddr) -> RemoteControlAddr {
    #[cfg(unix)]
    {
        let sanitized: String = listen
            .to_string()
            .chars()
            .map(|ch| match ch {
                'a'..='z' | 'A'..='Z' | '0'..='9' => ch,
                _ => '_',
            })
            .collect();
        RemoteControlAddr::Unix(
            std::env::temp_dir().join(format!("waitagent-relay-admin-{sanitized}.sock")),
        )
    }
    #[cfg(windows)]
    {
        RemoteControlAddr::Tcp(SocketAddr::from((
            [127, 0, 0, 1],
            listen.port().saturating_add(WINDOWS_ADMIN_PORT_OFFSET),
        )))
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct RelayAdminRequest {
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) ttl_secs: Option<u64>,
    #[serde(default)]
    pub(crate) deploy: Option<bool>,
    #[serde(default)]
    pub(crate) fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayAdminCommand {
    Status,
    Shutdown,
    /// Mint an enrollment token; `deploy` selects a reusable deploy token
    /// over a one-time invite token.
    Invite {
        ttl_secs: Option<u64>,
        deploy: bool,
    },
    /// Revoke a whitelisted node and drop its live link.
    Remove {
        fingerprint: String,
    },
}

/// One registered node as exposed over the admin socket.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct AdminNodeEntry {
    pub(crate) node_id: String,
    pub(crate) idle_ms: u128,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct RelayAdminStatus {
    pub(crate) listen: SocketAddr,
    pub(crate) registered_nodes: usize,
    pub(crate) nodes: Vec<AdminNodeEntry>,
    pub(crate) capacity: RelayCapacityConfig,
    pub(crate) usage: RelayUsage,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct RelayAdminResponse {
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) listen: Option<SocketAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) registered_nodes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) nodes: Option<Vec<AdminNodeEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) capacity: Option<RelayCapacityConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<RelayUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl RelayAdminResponse {
    fn status(snapshot: RelayAdminStatus) -> Self {
        Self {
            ok: true,
            listen: Some(snapshot.listen),
            registered_nodes: Some(snapshot.registered_nodes),
            nodes: Some(snapshot.nodes),
            capacity: Some(snapshot.capacity),
            usage: Some(snapshot.usage),
            message: None,
            error: None,
        }
    }

    fn shutdown() -> Self {
        Self {
            ok: true,
            listen: None,
            registered_nodes: None,
            nodes: None,
            capacity: None,
            usage: None,
            message: Some("shutting down".to_string()),
            error: None,
        }
    }

    /// A success answer whose payload is a human-readable `message` (the
    /// invite/remove responses).
    fn message_ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            listen: None,
            registered_nodes: None,
            nodes: None,
            capacity: None,
            usage: None,
            message: Some(message.into()),
            error: None,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            listen: None,
            registered_nodes: None,
            nodes: None,
            capacity: None,
            usage: None,
            message: None,
            error: Some(message.into()),
        }
    }
}

/// Parses one admin request body. Pure; unit-tested.
pub(crate) fn parse_relay_admin_command(body: &str) -> Result<RelayAdminCommand, String> {
    let request: RelayAdminRequest =
        serde_json::from_str(body).map_err(|error| format!("invalid admin request: {error}"))?;
    match request.command.as_str() {
        "status" => Ok(RelayAdminCommand::Status),
        "shutdown" => Ok(RelayAdminCommand::Shutdown),
        "invite" => Ok(RelayAdminCommand::Invite {
            ttl_secs: request.ttl_secs,
            deploy: request.deploy.unwrap_or(false),
        }),
        "remove" => {
            let fingerprint = request
                .fingerprint
                .ok_or_else(|| "remove requires a `fingerprint` field".to_string())?;
            Ok(RelayAdminCommand::Remove { fingerprint })
        }
        other => Err(format!("unknown admin command: {other:?}")),
    }
}

/// Builds the response for a parsed command against a table snapshot. Pure;
/// the shutdown side effect lives in [`run_admin_listener`], and the
/// enrollment commands (invite/remove) are answered by the impure
/// [`handle_invite`] / [`handle_remove`] wrappers in the dispatch loop.
pub(crate) fn handle_relay_admin_command(
    command: RelayAdminCommand,
    snapshot: RelayAdminStatus,
) -> RelayAdminResponse {
    match command {
        RelayAdminCommand::Status => RelayAdminResponse::status(snapshot),
        RelayAdminCommand::Shutdown => RelayAdminResponse::shutdown(),
        // Enrollment commands never reach the pure handler; the dispatch
        // loop routes them to the impure wrappers. Flag the routing bug.
        RelayAdminCommand::Invite { .. } | RelayAdminCommand::Remove { .. } => {
            RelayAdminResponse::error("internal error: enrollment command reached the pure handler")
        }
    }
}

/// `invite`: mints the enrollment token, persists the store (best-effort —
/// a persist failure is logged, never fatal), and answers with the raw
/// token plus its unix expiry.
fn handle_invite(
    ttl_secs: Option<u64>,
    deploy: bool,
    tokens: &Arc<EnrollmentTokenStore>,
    tokens_path: &Path,
) -> RelayAdminResponse {
    let (ttl, one_time) = match (deploy, ttl_secs) {
        (true, Some(secs)) => (Duration::from_secs(secs), false),
        (true, None) => (DEFAULT_DEPLOY_TTL, false),
        (false, Some(secs)) => (Duration::from_secs(secs), true),
        (false, None) => (DEFAULT_INVITE_TTL, true),
    };
    let minted = tokens.mint(one_time, ttl);
    if let Err(error) = tokens.persist(tokens_path) {
        ERROR_LOG.log_error(format!("[relay-admin] token persist failed: {error}"));
    }
    let kind = if minted.one_time {
        "one-time invite token"
    } else {
        "deploy token (reusable until expiry)"
    };
    RelayAdminResponse::message_ok(format!(
        "token: {}\nexpires_at: {}\nkind: {kind}",
        minted.token, minted.expires_at_unix
    ))
}

/// `remove`: revokes the whitelist entry (when present) and drops the live
/// link (when registered). Reports both halves so the operator sees whether
/// the node was whitelisted, online, or both.
fn handle_remove(
    fingerprint: &str,
    whitelist_dir: &Path,
    table: &Arc<RelayConnectionTable>,
    presence: &Arc<PresenceHub>,
) -> RelayAdminResponse {
    match remove_authorized_node(whitelist_dir, fingerprint) {
        Ok(removed_from_whitelist) => {
            let retired = table.retire(&fingerprint.to_lowercase());
            if retired {
                presence.publish(&fingerprint.to_lowercase(), false);
            }
            let detail = match (removed_from_whitelist, retired) {
                (true, true) => "whitelist entry removed; live link dropped",
                (true, false) => "whitelist entry removed; no live link",
                (false, true) => "no whitelist entry; live link dropped",
                (false, false) => "no whitelist entry; no live link",
            };
            RelayAdminResponse::message_ok(format!("removed: {fingerprint} ({detail})"))
        }
        Err(error) => RelayAdminResponse::error(format!("remove {fingerprint}: {error}")),
    }
}

/// Shared state the admin listener needs for one status answer.
#[derive(Clone)]
pub(crate) struct RelayAdminContext {
    pub(crate) table: Arc<RelayConnectionTable>,
    pub(crate) routing: Arc<RoutingTable>,
    pub(crate) listen: SocketAddr,
    pub(crate) capacity: RelayCapacityConfig,
    pub(crate) meter: SharedUsageMeter,
    pub(crate) tokens: Arc<EnrollmentTokenStore>,
    pub(crate) tokens_path: PathBuf,
    pub(crate) whitelist_dir: PathBuf,
    pub(crate) presence: Arc<PresenceHub>,
}

/// Accept loop for the admin socket. `shutdown_tx` is the server-wide
/// shutdown channel: the `shutdown` command fires it after answering, and
/// the loop also exits when it observes the signal, cleaning up the socket
/// file on its way out.
pub(crate) async fn run_admin_listener(
    listener: RemoteControlAsyncListener,
    addr: RemoteControlAddr,
    context: RelayAdminContext,
    shutdown_tx: watch::Sender<bool>,
) {
    let mut shutdown_rx = shutdown_tx.subscribe();
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            accepted = listener.accept() => {
                let Ok((mut stream, _)) = accepted else {
                    ERROR_LOG.log_error("[relay-admin] accept failed; stopping".to_string());
                    break;
                };
                let context = context.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    let RelayAdminContext {
                        table,
                        routing,
                        listen,
                        capacity,
                        meter,
                        tokens,
                        tokens_path,
                        whitelist_dir,
                        presence,
                    } = context;
                    let mut bytes = Vec::new();
                    if let Err(error) = stream.read_to_end(&mut bytes).await {
                        ERROR_LOG.log_error(format!("[relay-admin] read failed: {error}"));
                        return;
                    }
                    let body = String::from_utf8_lossy(&bytes);
                    let response = match parse_relay_admin_command(body.trim()) {
                        Ok(RelayAdminCommand::Status) => {
                            let snapshot = RelayAdminStatus {
                                listen,
                                registered_nodes: table.len(),
                                nodes: table
                                    .snapshot()
                                    .into_iter()
                                    .map(|(node_id, _connection_id, idle_ms)| AdminNodeEntry {
                                        node_id,
                                        idle_ms,
                                    })
                                    .collect(),
                                capacity: capacity.clone(),
                                usage: RelayUsage {
                                    registered_nodes: table.len(),
                                    active_streams: routing.stream_count(),
                                    forwarded_bytes_per_sec: meter.bytes_this_window(),
                                },
                            };
                            handle_relay_admin_command(RelayAdminCommand::Status, snapshot)
                        }
                        Ok(command @ RelayAdminCommand::Shutdown) => {
                            let response = handle_relay_admin_command(
                                command,
                                RelayAdminStatus {
                                    listen,
                                    registered_nodes: 0,
                                    nodes: Vec::new(),
                                    capacity: capacity.clone(),
                                    usage: RelayUsage {
                                        registered_nodes: 0,
                                        active_streams: 0,
                                        forwarded_bytes_per_sec: 0,
                                    },
                                },
                            );
                            // Answer first, then stop the relay.
                            let _ = shutdown_tx.send(true);
                            response
                        }
                        Ok(RelayAdminCommand::Invite { ttl_secs, deploy }) => {
                            handle_invite(ttl_secs, deploy, &tokens, &tokens_path)
                        }
                        Ok(RelayAdminCommand::Remove { fingerprint }) => {
                            handle_remove(&fingerprint, &whitelist_dir, &table, &presence)
                        }
                        Err(message) => RelayAdminResponse::error(message),
                    };
                    let body = match serde_json::to_string(&response) {
                        Ok(body) => body,
                        Err(error) => {
                            ERROR_LOG.log_error(format!("[relay-admin] encode failed: {error}"));
                            return;
                        }
                    };
                    if stream.write_all(body.as_bytes()).await.is_err()
                        || stream.flush().await.is_err()
                    {
                        ERROR_LOG.log_error("[relay-admin] write failed".to_string());
                    }
                });
            }
        }
    }
    cleanup_remote_listener(&addr);
}

/// CLI guidance when the admin socket cannot be reached. Pure; unit-tested.
pub(crate) fn relay_not_running_guidance(addr: &RemoteControlAddr, listen_text: &str) -> String {
    let serve = if listen_text == format!("0.0.0.0:{DEFAULT_RELAY_LISTEN_PORT}") {
        "waitagent relay serve".to_string()
    } else {
        format!("waitagent relay serve --listen {listen_text}")
    };
    format!(
        "cannot reach the relay admin socket at {addr} ({listen_text}): the relay does not appear to be running.\nStart it with: {serve}"
    )
}

use crate::infra::relay_server::DEFAULT_RELAY_LISTEN_PORT;

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> RelayAdminStatus {
        RelayAdminStatus {
            listen: SocketAddr::from(([127, 0, 0, 1], 7475)),
            registered_nodes: 1,
            nodes: vec![AdminNodeEntry {
                node_id: "node-a".to_string(),
                idle_ms: 12,
            }],
            capacity: RelayCapacityConfig {
                max_nodes: 8,
                max_streams: 16,
                max_throughput_bytes_per_sec: 1024,
            },
            usage: RelayUsage {
                registered_nodes: 1,
                active_streams: 2,
                forwarded_bytes_per_sec: 512,
            },
        }
    }

    #[test]
    fn parses_status_and_shutdown_commands() {
        assert_eq!(
            parse_relay_admin_command(r#"{"command":"status"}"#),
            Ok(RelayAdminCommand::Status)
        );
        assert_eq!(
            parse_relay_admin_command(r#"{"command":"shutdown"}"#),
            Ok(RelayAdminCommand::Shutdown)
        );
    }

    #[test]
    fn parses_invite_command_with_options() {
        assert_eq!(
            parse_relay_admin_command(r#"{"command":"invite"}"#),
            Ok(RelayAdminCommand::Invite {
                ttl_secs: None,
                deploy: false
            })
        );
        assert_eq!(
            parse_relay_admin_command(r#"{"command":"invite","ttl_secs":3600,"deploy":true}"#),
            Ok(RelayAdminCommand::Invite {
                ttl_secs: Some(3600),
                deploy: true
            })
        );
    }

    #[test]
    fn parses_remove_command_with_fingerprint() {
        assert_eq!(
            parse_relay_admin_command(r#"{"command":"remove","fingerprint":"7c857a105486f46d"}"#),
            Ok(RelayAdminCommand::Remove {
                fingerprint: "7c857a105486f46d".to_string()
            })
        );
        let missing = parse_relay_admin_command(r#"{"command":"remove"}"#).unwrap_err();
        assert!(missing.contains("fingerprint"), "{missing}");
    }

    #[test]
    fn parse_rejects_unknown_and_malformed_requests() {
        let unknown = parse_relay_admin_command(r#"{"command":"bogus"}"#).unwrap_err();
        assert!(unknown.contains("unknown admin command"));
        let malformed = parse_relay_admin_command("{not json").unwrap_err();
        assert!(malformed.contains("invalid admin request"));
    }

    #[test]
    fn invite_response_carries_token_and_expiry() {
        let tokens = Arc::new(EnrollmentTokenStore::new());
        let path = std::env::temp_dir().join(format!(
            "waitagent-admin-invite-{}-{}.json",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let response = handle_invite(None, false, &tokens, &path);
        assert!(response.ok);
        let message = response.message.expect("invite answers with a message");
        let token = message
            .lines()
            .find_map(|line| line.strip_prefix("token: "))
            .expect("message carries the raw token");
        assert_eq!(token.len(), 43, "256-bit base64url-no-pad token");
        assert!(
            message.lines().any(|line| line.starts_with("expires_at: ")),
            "message carries the unix expiry: {message}"
        );

        // The minted token redeems against the same store.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        assert_eq!(
            tokens.redeem(token, now),
            crate::infra::relay_enrollment::RedeemOutcome::EnrollOk { one_time: true }
        );
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn remove_response_reports_whitelist_and_link_outcome() {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-admin-remove-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("whitelist dir");
        std::fs::write(dir.join("deadbeef"), b"pem").expect("entry");

        let table = Arc::new(RelayConnectionTable::default());
        let response = handle_remove("DeadBeef", &dir, &table, &Arc::new(PresenceHub::new()));
        assert!(response.ok);
        assert!(response
            .message
            .expect("remove answers with a message")
            .contains("whitelist entry removed"));
        assert!(
            std::fs::metadata(dir.join("deadbeef")).is_err(),
            "entry gone"
        );

        // Second remove: nothing left.
        let response = handle_remove("deadbeef", &dir, &table, &Arc::new(PresenceHub::new()));
        assert!(response.ok);
        assert!(response
            .message
            .expect("message")
            .contains("no whitelist entry; no live link"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_response_carries_the_snapshot() {
        let response = handle_relay_admin_command(RelayAdminCommand::Status, snapshot());
        assert!(response.ok);
        assert_eq!(
            response.listen,
            Some(SocketAddr::from(([127, 0, 0, 1], 7475)))
        );
        assert_eq!(response.registered_nodes, Some(1));
        assert_eq!(
            response.nodes,
            Some(vec![AdminNodeEntry {
                node_id: "node-a".to_string(),
                idle_ms: 12,
            }])
        );
    }

    #[test]
    fn status_response_carries_capacity_and_usage() {
        let snapshot = snapshot();
        let response = handle_relay_admin_command(RelayAdminCommand::Status, snapshot.clone());
        assert_eq!(response.capacity, Some(snapshot.capacity));
        assert_eq!(response.usage, Some(snapshot.usage));
    }

    #[test]
    fn shutdown_response_is_ok_with_a_message() {
        let response = handle_relay_admin_command(RelayAdminCommand::Shutdown, snapshot());
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("shutting down"));
        assert!(response.nodes.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn admin_addr_is_a_sanitized_unix_socket_in_temp_dir() {
        let addr = relay_admin_addr(SocketAddr::from(([127, 0, 0, 1], 7475)));
        let path = addr.unix_path().expect("unix addr");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "waitagent-relay-admin-127_0_0_1_7475.sock");
    }

    #[test]
    fn guidance_names_the_default_serve_command() {
        let addr = relay_admin_addr(SocketAddr::from(([127, 0, 0, 1], 7475)));
        let guidance = relay_not_running_guidance(&addr, "0.0.0.0:7475");
        assert!(
            guidance.contains("waitagent relay serve"),
            "guidance should name the start command: {guidance}"
        );
        assert!(!guidance.contains("--listen"), "{guidance}");
    }

    #[test]
    fn guidance_repeats_a_custom_listen_flag() {
        let addr = relay_admin_addr(SocketAddr::from(([127, 0, 0, 1], 9999)));
        let guidance = relay_not_running_guidance(&addr, "0.0.0.0:9999");
        assert!(
            guidance.contains("waitagent relay serve --listen 0.0.0.0:9999"),
            "{guidance}"
        );
    }
}
