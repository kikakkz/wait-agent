//! Node-channel relay administration (docs/relay-design.md 管理通道): the
//! "远端控制流" — a registered node manages the relay over its authenticated
//! link. Read path since slice 2 (`status`); write paths (`invite`/`remove`)
//! opened in slice 4 with the exact `relay_admin` semantics (mint +
//! persist; revoke whitelist entry + drop the live link). `shutdown` stays
//! local-only forever: an emergency stop must not be reachable from a
//! network-facing channel.
//!
//! The wire envelope mirrors the local admin protocol: one JSON request
//! `{"command": ...}` and one JSON response, `{"ok": true, ...}` or
//! `{"ok": false, "error": ...}`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::infra::relay_admin::{
    handle_invite, handle_remove, parse_relay_admin_command, RelayAdminCommand,
};
use crate::infra::relay_capacity::{RelayCapacityConfig, RelayUsage, SharedUsageMeter};
use crate::infra::relay_connection_table::RelayConnectionTable;
use crate::infra::relay_enrollment::EnrollmentTokenStore;
use crate::infra::relay_presence::PresenceHub;
use crate::infra::relay_routing::RoutingTable;
use crate::infra::relay_server::TokenTtlConfig;

/// One entry of the relay connection table as exposed over the node channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAdminNodeEntry {
    /// The node's certificate fingerprint (its node id).
    pub node_id: String,
    /// Milliseconds since the node's last frame.
    pub idle_ms: u128,
}

/// The relay status snapshot, answered to a `status` request. Field sources
/// are identical to the local admin socket's status (same tables, same
/// meter); `uptime_ms` is node-channel-only because the dashboard renders
/// it and the local protocol predates it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAdminStatus {
    /// Address the relay listener bound to.
    pub listen: SocketAddr,
    /// Milliseconds since the relay started.
    pub uptime_ms: u64,
    /// Registered (online) nodes in the connection table.
    pub registered_nodes: usize,
    /// The connection table: registered means online.
    pub nodes: Vec<RemoteAdminNodeEntry>,
    /// Configured capacity ceilings.
    pub capacity: RelayCapacityConfig,
    /// Current usage meters.
    pub usage: RelayUsage,
}

/// The node-channel admin response envelope, same shape as the local admin
/// protocol: `ok` plus at most one of `status` / `message` / `error`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAdminResponse {
    /// Whether the request succeeded.
    pub ok: bool,
    /// The status payload of a successful `status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<RemoteAdminStatus>,
    /// The human-readable result of a successful write command (same text
    /// the local admin socket prints, e.g. the invite answer carrying the
    /// raw token on its `token: ` line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The human-readable rejection when not `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Everything a link needs to answer one node-channel admin request:
/// the read-side snapshot sources plus the write-side stores (`invite`
/// mints+persist tokens, `remove` revokes the whitelist and drops live
/// links). Constructed per request in the link loop from the server-wide
/// arcs — no state lives here.
pub(crate) struct RemoteAdminContext<'a> {
    pub(crate) table: &'a Arc<RelayConnectionTable>,
    pub(crate) routing: &'a Arc<RoutingTable>,
    pub(crate) listen: SocketAddr,
    pub(crate) capacity: &'a RelayCapacityConfig,
    pub(crate) meter: &'a SharedUsageMeter,
    pub(crate) started_at: Instant,
    pub(crate) tokens: &'a Arc<EnrollmentTokenStore>,
    pub(crate) tokens_path: &'a Path,
    pub(crate) token_ttls: TokenTtlConfig,
    pub(crate) whitelist_dir: &'a Path,
    pub(crate) presence: &'a Arc<PresenceHub>,
}

/// A parsed node-channel admin command. `invite`/`remove` execute with the
/// local admin semantics; only `shutdown` is refused (it stays on the local
/// admin socket by design).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteAdminCommand {
    /// Read the status snapshot.
    Status,
    /// Mint an enrollment token (one-time by default).
    Invite { ttl_secs: Option<u64>, deploy: bool },
    /// Revoke a whitelisted node and drop its live link.
    Remove { fingerprint: String },
    /// A recognized command this channel does not serve.
    Unsupported(&'static str),
}

/// Parses one request body, reusing the local admin command grammar.
pub(crate) fn parse_remote_admin_command(body: &str) -> Result<RemoteAdminCommand, String> {
    match parse_relay_admin_command(body)? {
        RelayAdminCommand::Status => Ok(RemoteAdminCommand::Status),
        RelayAdminCommand::Invite { ttl_secs, deploy } => {
            Ok(RemoteAdminCommand::Invite { ttl_secs, deploy })
        }
        RelayAdminCommand::Remove { fingerprint } => Ok(RemoteAdminCommand::Remove { fingerprint }),
        RelayAdminCommand::Shutdown => Ok(RemoteAdminCommand::Unsupported(
            "shutdown is not supported over the node channel; use the local admin socket",
        )),
    }
}

/// Builds the status snapshot from the same state the local admin socket
/// reads. `started_at` is captured by `relay_server::start`.
pub(crate) fn build_status_snapshot(
    table: &Arc<RelayConnectionTable>,
    routing: &Arc<RoutingTable>,
    listen: SocketAddr,
    capacity: &RelayCapacityConfig,
    meter: &SharedUsageMeter,
    started_at: Instant,
) -> RemoteAdminStatus {
    RemoteAdminStatus {
        listen,
        uptime_ms: started_at.elapsed().as_millis() as u64,
        registered_nodes: table.len(),
        nodes: table
            .snapshot()
            .into_iter()
            .map(|(node_id, _connection_id, idle_ms)| RemoteAdminNodeEntry { node_id, idle_ms })
            .collect(),
        capacity: capacity.clone(),
        usage: RelayUsage {
            registered_nodes: table.len(),
            active_streams: routing.stream_count(),
            forwarded_bytes_per_sec: meter.bytes_this_window(),
        },
    }
}

/// Answers one raw request body: parse, dispatch, encode. The link loop's
/// single call point for an `AdminRequest` frame.
pub(crate) fn remote_admin_response(
    command_body: &str,
    context: &RemoteAdminContext<'_>,
) -> String {
    match parse_remote_admin_command(command_body) {
        Ok(command) => handle_remote_admin_request(command, context),
        Err(message) => encode(&RemoteAdminResponse {
            ok: false,
            status: None,
            message: None,
            error: Some(message),
        }),
    }
}

/// Answers one parsed request. Write commands reuse the local admin
/// handlers verbatim, so the node channel cannot drift from the local
/// semantics; their `message` text is the same text the CLI prints.
pub(crate) fn handle_remote_admin_request(
    command: RemoteAdminCommand,
    context: &RemoteAdminContext<'_>,
) -> String {
    let response = match command {
        RemoteAdminCommand::Status => RemoteAdminResponse {
            ok: true,
            status: Some(build_status_snapshot(
                context.table,
                context.routing,
                context.listen,
                context.capacity,
                context.meter,
                context.started_at,
            )),
            message: None,
            error: None,
        },
        RemoteAdminCommand::Invite { ttl_secs, deploy } => {
            let local = handle_invite(
                ttl_secs,
                deploy,
                context.tokens,
                context.tokens_path,
                &context.token_ttls,
            );
            message_envelope(local)
        }
        RemoteAdminCommand::Remove { fingerprint } => {
            let local = handle_remove(
                &fingerprint,
                context.whitelist_dir,
                context.table,
                context.presence,
            );
            message_envelope(local)
        }
        RemoteAdminCommand::Unsupported(reason) => RemoteAdminResponse {
            ok: false,
            status: None,
            message: None,
            error: Some(reason.to_string()),
        },
    };
    encode(&response)
}

/// Maps a local admin response onto the node-channel envelope: the
/// human-readable `message` survives, the machine payload (listen/nodes)
/// stays local-only.
fn message_envelope(local: crate::infra::relay_admin::RelayAdminResponse) -> RemoteAdminResponse {
    RemoteAdminResponse {
        ok: local.ok,
        status: None,
        message: local.message,
        error: local.error,
    }
}

fn encode(response: &RemoteAdminResponse) -> String {
    serde_json::to_string(response)
        .unwrap_or_else(|error| format!(r#"{{"ok":false,"error":"encode failed: {error}"}}"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_rejects_garbage() {
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"status"}"#),
            Ok(RemoteAdminCommand::Status)
        );
        let error = parse_remote_admin_command("{not json").expect_err("garbage fails");
        assert!(error.contains("invalid admin request"), "{error}");
    }

    #[test]
    fn write_commands_parse_to_executable_variants() {
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"invite"}"#),
            Ok(RemoteAdminCommand::Invite {
                ttl_secs: None,
                deploy: false
            })
        );
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"invite","ttl_secs":3600,"deploy":true}"#),
            Ok(RemoteAdminCommand::Invite {
                ttl_secs: Some(3600),
                deploy: true
            })
        );
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"remove","fingerprint":"ab"}"#),
            Ok(RemoteAdminCommand::Remove {
                fingerprint: "ab".to_string()
            })
        );
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"shutdown"}"#),
            Ok(RemoteAdminCommand::Unsupported(
                "shutdown is not supported over the node channel; use the local admin socket"
            ))
        );
        let unknown = parse_remote_admin_command(r#"{"command":"bogus"}"#).expect_err("");
        assert!(unknown.contains("unknown admin command"), "{unknown}");
    }

    #[test]
    fn status_response_envelope_serializes_status_only() {
        let snapshot = RemoteAdminStatus {
            listen: SocketAddr::from(([127, 0, 0, 1], 7475)),
            uptime_ms: 12_345,
            registered_nodes: 1,
            nodes: vec![RemoteAdminNodeEntry {
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
        };
        let response = RemoteAdminResponse {
            ok: true,
            status: Some(snapshot.clone()),
            message: None,
            error: None,
        };
        let body = serde_json::to_string(&response).expect("envelope serializes");
        assert!(body.contains(r#""ok":true"#), "{body}");
        assert!(body.contains(r#""uptime_ms":12345"#), "{body}");
        assert!(!body.contains("\"error\""), "{body}");
        let decoded: RemoteAdminResponse = serde_json::from_str(&body).expect("envelope parses");
        assert_eq!(decoded.status, Some(snapshot));
    }

    #[test]
    fn unsupported_response_envelope_serializes_error_only() {
        let response = RemoteAdminResponse {
            ok: false,
            status: None,
            message: None,
            error: Some("nope".to_string()),
        };
        let body = serde_json::to_string(&response).expect("envelope serializes");
        assert!(body.contains(r#""ok":false"#), "{body}");
        assert!(!body.contains("\"status\""), "{body}");
    }
}
