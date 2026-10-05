//! Node-channel relay administration (docs/relay-design.md 管理通道): the
//! read-only half of the "远端控制流" — a registered node asks the relay
//! for the status snapshot over its authenticated link, and the relay
//! answers on the same link. Command names reuse the local admin socket
//! semantics (`relay_admin`); only `status` is accepted in this slice —
//! invite/remove/shutdown stay local-only until the write-operations slice.
//!
//! The wire envelope mirrors the local admin protocol: one JSON request
//! `{"command": ...}` and one JSON response, `{"ok": true, ...status}` or
//! `{"ok": false, "error": ...}`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::infra::relay_admin::{parse_relay_admin_command, RelayAdminCommand};
use crate::infra::relay_capacity::{RelayCapacityConfig, RelayUsage, SharedUsageMeter};
use crate::infra::relay_connection_table::RelayConnectionTable;
use crate::infra::relay_routing::RoutingTable;

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
/// protocol: `ok` plus exactly one of `status` / `error`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAdminResponse {
    /// Whether the request succeeded.
    pub ok: bool,
    /// The status payload when `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<RemoteAdminStatus>,
    /// The human-readable rejection when not `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A parsed node-channel admin command. Write commands parse successfully
/// but arrive as [`RemoteAdminCommand::Unsupported`] so the caller can give
/// an actionable read-only rejection instead of "unknown command".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteAdminCommand {
    /// Read the status snapshot.
    Status,
    /// A recognized command this channel does not serve (yet).
    Unsupported(String),
}

/// Parses one request body, reusing the local admin command grammar.
pub(crate) fn parse_remote_admin_command(body: &str) -> Result<RemoteAdminCommand, String> {
    match parse_relay_admin_command(body)? {
        RelayAdminCommand::Status => Ok(RemoteAdminCommand::Status),
        RelayAdminCommand::Invite { .. } | RelayAdminCommand::Remove { .. } => {
            Ok(RemoteAdminCommand::Unsupported(
                "write commands are not supported over the node channel".to_string(),
            ))
        }
        RelayAdminCommand::Shutdown => Ok(RemoteAdminCommand::Unsupported(
            "shutdown is not supported over the node channel; use the local admin socket"
                .to_string(),
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
    table: &Arc<RelayConnectionTable>,
    routing: &Arc<RoutingTable>,
    listen: SocketAddr,
    capacity: &RelayCapacityConfig,
    meter: &SharedUsageMeter,
    started_at: Instant,
) -> String {
    match parse_remote_admin_command(command_body) {
        Ok(command) => handle_remote_admin_request(
            command, table, routing, listen, capacity, meter, started_at,
        ),
        Err(message) => serde_json::to_string(&RemoteAdminResponse {
            ok: false,
            status: None,
            error: Some(message),
        })
        .unwrap_or_else(|error| {
            format!(r#"{{"ok":false,"error":"status encode failed: {error}"}}"#)
        }),
    }
}

/// Answers one parsed request against a freshly built snapshot. Pure apart
/// from the snapshot build; the JSON encode failure path is defensive
/// (serializing this shape cannot fail today).
pub(crate) fn handle_remote_admin_request(
    command: RemoteAdminCommand,
    table: &Arc<RelayConnectionTable>,
    routing: &Arc<RoutingTable>,
    listen: SocketAddr,
    capacity: &RelayCapacityConfig,
    meter: &SharedUsageMeter,
    started_at: Instant,
) -> String {
    let response = match command {
        RemoteAdminCommand::Status => RemoteAdminResponse {
            ok: true,
            status: Some(build_status_snapshot(
                table, routing, listen, capacity, meter, started_at,
            )),
            error: None,
        },
        RemoteAdminCommand::Unsupported(reason) => RemoteAdminResponse {
            ok: false,
            status: None,
            error: Some(reason),
        },
    };
    serde_json::to_string(&response).unwrap_or_else(|error| {
        format!(r#"{{"ok":false,"error":"status encode failed: {error}"}}"#)
    })
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
    fn write_commands_parse_as_unsupported() {
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"invite"}"#),
            Ok(RemoteAdminCommand::Unsupported(
                "write commands are not supported over the node channel".to_string()
            ))
        );
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"remove","fingerprint":"ab"}"#),
            Ok(RemoteAdminCommand::Unsupported(
                "write commands are not supported over the node channel".to_string()
            ))
        );
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"shutdown"}"#),
            Ok(RemoteAdminCommand::Unsupported(
                "shutdown is not supported over the node channel; use the local admin socket"
                    .to_string()
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
            error: Some("nope".to_string()),
        };
        let body = serde_json::to_string(&response).expect("envelope serializes");
        assert!(body.contains(r#""ok":false"#), "{body}");
        assert!(!body.contains("\"status\""), "{body}");
    }
}
