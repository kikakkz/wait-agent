//! Node-channel relay administration (docs/relay-design.md 管理通道): the
//! "远端控制流" — a registered node manages the relay over its authenticated
//! link. Read paths since slice 2 (`status`) plus the `list-whitelist`
//! directory listing (issue #147, so the dashboard can resolve a remove
//! prefix against enrolled-but-offline nodes); the slice-3 fingerprint
//! auto-discovery pair (`announce` / `resolve-node`, issue #156) lets a node
//! publish its host labels and look a peer's fingerprint up by host label
//! or address; write paths (`invite`/`remove`)
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
use crate::infra::relay_connection_table::{LabelResolve, RelayConnectionTable};
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

/// A unique `resolve-node` match (issue #156 slice 3): the registered node
/// whose announced host labels answer the query, with the labels it
/// announced so the caller can show what matched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAdminNodeMatch {
    /// The matched node's certificate fingerprint (its node id) — the
    /// `tls_pin_sha256` a relay-via dial pins.
    pub node_id: String,
    /// The host labels the node announced for itself.
    pub labels: Vec<String>,
}

/// Caps for the slice-3 `announce` labels (issue #156): enough for a
/// hostname plus a handful of addresses, bounded so one link cannot bloat
/// the in-memory table.
pub(crate) const MAX_ANNOUNCE_LABELS: usize = 8;
/// Maximum byte length of one announced label.
pub(crate) const MAX_LABEL_LEN: usize = 64;

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
/// protocol: `ok` plus at most one of `status` / `whitelist` / `node` /
/// `message` / `error`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAdminResponse {
    /// Whether the request succeeded.
    pub ok: bool,
    /// The status payload of a successful `status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<RemoteAdminStatus>,
    /// The whitelist fingerprints of a successful `list-whitelist`: the
    /// lowercase file names of the relay's authorized_nodes directory
    /// (enrolled nodes, online or not — issue #147).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whitelist: Option<Vec<String>>,
    /// The unique node match of a successful `resolve-node` (issue #156
    /// slice 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<RemoteAdminNodeMatch>,
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
    /// The requesting link's registered identity: `announce` may only label
    /// this link's own table entry, never another node's (issue #156
    /// slice 3). Same ownership guard as `touch`/`remove_if_current`.
    pub(crate) node_id: String,
    pub(crate) connection_id: u64,
}

/// A parsed node-channel admin command. `invite`/`remove` execute with the
/// local admin semantics; only `shutdown` is refused (it stays on the local
/// admin socket by design).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteAdminCommand {
    /// Read the status snapshot.
    Status,
    /// List the enrolled (whitelisted) node fingerprints, online or not
    /// (issue #147: the dashboard resolves remove prefixes against this).
    ListWhitelist,
    /// Publish this link's host labels for fingerprint auto-discovery
    /// (issue #156 slice 3). Labels the requesting link's own table entry
    /// only.
    Announce { labels: Vec<String> },
    /// Look up the uniquely-registered node whose announced labels match the
    /// query (issue #156 slice 3). Read-only.
    ResolveNode { label: String },
    /// Mint an enrollment token (one-time by default).
    Invite { ttl_secs: Option<u64>, deploy: bool },
    /// Revoke a whitelisted node and drop its live link.
    Remove { fingerprint: String },
    /// A recognized command this channel does not serve.
    Unsupported(&'static str),
}

/// Parses one request body. The node-channel-only reads (`list-whitelist`,
/// `resolve-node`) and the self-labeling write (`announce`) are recognized
/// here before falling back to the shared local admin grammar, which stays
/// unchanged (issue #147, issue #156 slice 3).
pub(crate) fn parse_remote_admin_command(body: &str) -> Result<RemoteAdminCommand, String> {
    if let Ok(request) = serde_json::from_str::<crate::infra::relay_admin::RelayAdminRequest>(body)
    {
        match request.command.as_str() {
            "list-whitelist" => return Ok(RemoteAdminCommand::ListWhitelist),
            "announce" => {
                let labels = request
                    .labels
                    .ok_or_else(|| "announce requires a `labels` array".to_string())?;
                return parse_announce_labels(labels)
                    .map(|labels| RemoteAdminCommand::Announce { labels });
            }
            "resolve-node" => {
                let label = request
                    .label
                    .filter(|label| !label.trim().is_empty())
                    .ok_or_else(|| "resolve-node requires a `label` field".to_string())?;
                return Ok(RemoteAdminCommand::ResolveNode { label });
            }
            _ => {}
        }
    }
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

/// Validates an `announce` label list: bounded count and per-label length,
/// non-empty after trim, deduplicated, stored trimmed.
fn parse_announce_labels(labels: Vec<String>) -> Result<Vec<String>, String> {
    if labels.is_empty() {
        return Err("announce requires a non-empty `labels` array".to_string());
    }
    if labels.len() > MAX_ANNOUNCE_LABELS {
        return Err(format!(
            "announce accepts at most {MAX_ANNOUNCE_LABELS} labels, got {}",
            labels.len()
        ));
    }
    let mut parsed = Vec::with_capacity(labels.len());
    for label in labels {
        let label = label.trim();
        if label.is_empty() {
            return Err("announce labels must be non-empty".to_string());
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(format!(
                "announce labels are at most {MAX_LABEL_LEN} bytes, got {}",
                label.len()
            ));
        }
        if !parsed.iter().any(|existing| existing == label) {
            parsed.push(label.to_string());
        }
    }
    Ok(parsed)
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
            whitelist: None,
            node: None,
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
            whitelist: None,
            node: None,
            message: None,
            error: None,
        },
        RemoteAdminCommand::ListWhitelist => {
            match crate::infra::relay_server::authorized_node_fingerprints(context.whitelist_dir) {
                Ok(mut fingerprints) => {
                    // Sorted so the answer is deterministic for the dashboard
                    // and for tests; the directory scan order is unspecified.
                    fingerprints.sort();
                    RemoteAdminResponse {
                        ok: true,
                        status: None,
                        whitelist: Some(fingerprints),
                        node: None,
                        message: None,
                        error: None,
                    }
                }
                Err(error) => RemoteAdminResponse {
                    ok: false,
                    status: None,
                    whitelist: None,
                    node: None,
                    message: None,
                    error: Some(format!("list-whitelist: {error}")),
                },
            }
        }
        RemoteAdminCommand::Announce { labels } => {
            // Self-description scoped to the requesting link: the table
            // drops the write when this connection id no longer owns the
            // entry (replacement/re-register raced the request).
            context
                .table
                .set_labels(&context.node_id, context.connection_id, labels.clone());
            RemoteAdminResponse {
                ok: true,
                status: None,
                whitelist: None,
                node: None,
                message: Some(format!("announced: {} label(s)", labels.len())),
                error: None,
            }
        }
        RemoteAdminCommand::ResolveNode { label } => match context.table.resolve_label(&label) {
            LabelResolve::Unique { node_id, labels } => RemoteAdminResponse {
                ok: true,
                status: None,
                whitelist: None,
                node: Some(RemoteAdminNodeMatch { node_id, labels }),
                message: None,
                error: None,
            },
            LabelResolve::NoMatch => RemoteAdminResponse {
                ok: false,
                status: None,
                whitelist: None,
                node: None,
                message: None,
                error: Some(format!("no registered node matches label {label:?}")),
            },
            LabelResolve::Ambiguous(count) => RemoteAdminResponse {
                ok: false,
                status: None,
                whitelist: None,
                node: None,
                message: None,
                error: Some(format!(
                    "label {label:?} matches {count} registered nodes; refine the host label"
                )),
            },
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
            whitelist: None,
            node: None,
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
        whitelist: None,
        node: None,
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
    fn parses_list_whitelist_command() {
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"list-whitelist"}"#),
            Ok(RemoteAdminCommand::ListWhitelist)
        );
        // The node channel recognizes it; the shared local grammar (and so
        // the local admin socket) still rejects it as unknown.
        let local = parse_relay_admin_command(r#"{"command":"list-whitelist"}"#).unwrap_err();
        assert!(local.contains("unknown admin command"), "{local}");
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
            whitelist: None,
            node: None,
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
            whitelist: None,
            node: None,
            message: None,
            error: Some("nope".to_string()),
        };
        let body = serde_json::to_string(&response).expect("envelope serializes");
        assert!(body.contains(r#""ok":false"#), "{body}");
        assert!(!body.contains("\"status\""), "{body}");
    }

    /// Owns everything a [`RemoteAdminContext`] borrows for the
    /// `list-whitelist`/`announce`/`resolve-node` tests (only `table` and
    /// `whitelist_dir` matter there).
    struct TestContext {
        table: Arc<RelayConnectionTable>,
        routing: Arc<RoutingTable>,
        meter: crate::infra::relay_capacity::SharedUsageMeter,
        tokens: Arc<EnrollmentTokenStore>,
        presence: Arc<PresenceHub>,
    }

    impl TestContext {
        fn new() -> Self {
            Self {
                table: Arc::new(RelayConnectionTable::default()),
                routing: Arc::new(RoutingTable::default()),
                meter: Arc::new(crate::infra::relay_capacity::UsageMeter::new()),
                tokens: Arc::new(EnrollmentTokenStore::new()),
                presence: Arc::new(PresenceHub::new()),
            }
        }

        fn context<'a>(&'a self, whitelist_dir: &'a std::path::Path) -> RemoteAdminContext<'a> {
            self.context_with_link(whitelist_dir, "fp-self", 1)
        }

        fn context_with_link<'a>(
            &'a self,
            whitelist_dir: &'a std::path::Path,
            node_id: &str,
            connection_id: u64,
        ) -> RemoteAdminContext<'a> {
            RemoteAdminContext {
                table: &self.table,
                routing: &self.routing,
                listen: SocketAddr::from(([127, 0, 0, 1], 7475)),
                capacity: &RelayCapacityConfig {
                    max_nodes: 8,
                    max_streams: 16,
                    max_throughput_bytes_per_sec: 1024,
                },
                meter: &self.meter,
                started_at: Instant::now(),
                tokens: &self.tokens,
                tokens_path: std::path::Path::new("unused-tokens.json"),
                token_ttls: TokenTtlConfig::default(),
                whitelist_dir,
                presence: &self.presence,
                node_id: node_id.to_string(),
                connection_id,
            }
        }
    }

    fn dummy_outbound() -> crate::infra::relay_scheduler::SchedulerIngress {
        let (ingress, _bulk_rx, _control_rx) =
            crate::infra::relay_scheduler::SchedulerIngress::test_channels(8);
        ingress
    }

    #[test]
    fn parses_announce_and_resolve_node_commands() {
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"announce","labels":["nas","10.0.1.5"]}"#),
            Ok(RemoteAdminCommand::Announce {
                labels: vec!["nas".to_string(), "10.0.1.5".to_string()]
            })
        );
        assert_eq!(
            parse_remote_admin_command(r#"{"command":"resolve-node","label":"nas"}"#),
            Ok(RemoteAdminCommand::ResolveNode {
                label: "nas".to_string()
            })
        );
        // Both stay node-channel-only: the shared local grammar rejects them.
        let local_announce =
            parse_relay_admin_command(r#"{"command":"announce","labels":["nas"]}"#).unwrap_err();
        assert!(
            local_announce.contains("unknown admin command"),
            "{local_announce}"
        );
        let local_resolve =
            parse_relay_admin_command(r#"{"command":"resolve-node","label":"nas"}"#).unwrap_err();
        assert!(
            local_resolve.contains("unknown admin command"),
            "{local_resolve}"
        );

        let missing_labels =
            parse_remote_admin_command(r#"{"command":"announce","labels":[]}"#).unwrap_err();
        assert!(missing_labels.contains("labels"), "{missing_labels}");
        let missing_label =
            parse_remote_admin_command(r#"{"command":"resolve-node"}"#).unwrap_err();
        assert!(missing_label.contains("label"), "{missing_label}");
        let too_many = parse_remote_admin_command(&format!(
            r#"{{"command":"announce","labels":[{}]}}"#,
            (0..MAX_ANNOUNCE_LABELS + 1)
                .map(|_| "\"x\"".to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))
        .unwrap_err();
        assert!(too_many.contains("at most"), "{too_many}");
        let too_long = parse_remote_admin_command(&format!(
            r#"{{"command":"announce","labels":["{}"]}}"#,
            "x".repeat(MAX_LABEL_LEN + 1)
        ))
        .unwrap_err();
        assert!(too_long.contains("64"), "{too_long}");
    }

    #[test]
    fn announce_labels_only_the_requesting_link_and_resolve_node_answers() {
        let holder = TestContext::new();
        let dir = std::env::temp_dir().join(format!(
            "waitagent-remote-announce-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("whitelist dir");

        let link = holder.table.register("fp-node-a", dummy_outbound());
        // A stale/replaced link cannot relabel the entry: announce through a
        // context whose connection id no longer owns the node.
        let stale = handle_remote_admin_request(
            RemoteAdminCommand::Announce {
                labels: vec!["stale".to_string()],
            },
            &holder.context_with_link(&dir, "fp-node-a", link.connection_id + 999),
        );
        let stale: RemoteAdminResponse = serde_json::from_str(&stale).expect("envelope parses");
        assert!(stale.ok, "{stale:?}");
        assert_eq!(
            holder.table.resolve_label("stale"),
            crate::infra::relay_connection_table::LabelResolve::NoMatch
        );

        let announced = handle_remote_admin_request(
            RemoteAdminCommand::Announce {
                labels: vec!["node-a".to_string(), "10.0.1.5".to_string()],
            },
            &holder.context_with_link(&dir, "fp-node-a", link.connection_id),
        );
        let announced: RemoteAdminResponse = serde_json::from_str(&announced).expect("envelope");
        assert!(announced.ok, "{announced:?}");
        assert!(announced.node.is_none(), "{announced:?}");

        // Another registered node does not match the label.
        holder.table.register("fp-node-b", dummy_outbound());
        let body = handle_remote_admin_request(
            RemoteAdminCommand::ResolveNode {
                label: "node-a".to_string(),
            },
            &holder.context(&dir),
        );
        let response: RemoteAdminResponse = serde_json::from_str(&body).expect("envelope");
        assert!(response.ok, "{response:?}");
        assert_eq!(
            response.node,
            Some(RemoteAdminNodeMatch {
                node_id: "fp-node-a".to_string(),
                labels: vec!["node-a".to_string(), "10.0.1.5".to_string()],
            })
        );

        let missing = handle_remote_admin_request(
            RemoteAdminCommand::ResolveNode {
                label: "ghost".to_string(),
            },
            &holder.context(&dir),
        );
        let missing: RemoteAdminResponse = serde_json::from_str(&missing).expect("envelope");
        assert!(!missing.ok, "{missing:?}");
        assert!(
            missing
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("ghost"),
            "{missing:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_node_reports_ambiguous_matches() {
        let holder = TestContext::new();
        let dir = std::env::temp_dir().join(format!(
            "waitagent-remote-resolve-ambiguous-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("whitelist dir");

        for (node, label) in [("fp-a", "nas"), ("fp-b", "nas")] {
            let link = holder.table.register(node, dummy_outbound());
            let body = handle_remote_admin_request(
                RemoteAdminCommand::Announce {
                    labels: vec![label.to_string()],
                },
                &holder.context_with_link(&dir, node, link.connection_id),
            );
            let response: RemoteAdminResponse = serde_json::from_str(&body).expect("envelope");
            assert!(response.ok, "{response:?}");
        }
        let body = handle_remote_admin_request(
            RemoteAdminCommand::ResolveNode {
                label: "nas".to_string(),
            },
            &holder.context(&dir),
        );
        let response: RemoteAdminResponse = serde_json::from_str(&body).expect("envelope");
        assert!(!response.ok, "ambiguous must not guess: {response:?}");
        assert!(response.node.is_none(), "{response:?}");
        let error = response.error.expect("error present");
        assert!(error.contains("2") && error.contains("nas"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_whitelist_envelope_carries_sorted_directory_entries() {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-remote-list-whitelist-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("whitelist dir");
        std::fs::write(dir.join("fedcba9876543210"), b"pem").expect("entry");
        std::fs::write(dir.join("abcdef0123456789"), b"pem").expect("entry");
        std::fs::create_dir(dir.join("not-a-file-entry")).expect("subdir is skipped");

        let holder = TestContext::new();
        let body =
            handle_remote_admin_request(RemoteAdminCommand::ListWhitelist, &holder.context(&dir));
        let response: RemoteAdminResponse = serde_json::from_str(&body).expect("envelope parses");
        assert!(response.ok, "{response:?}");
        assert_eq!(
            response.whitelist,
            Some(vec![
                "abcdef0123456789".to_string(),
                "fedcba9876543210".to_string()
            ]),
            "sorted lowercase entries, directory scan order unspecified: {response:?}"
        );
        assert!(
            response.status.is_none() && response.message.is_none(),
            "{response:?}"
        );

        // A missing directory means an empty whitelist, not an error.
        let body = handle_remote_admin_request(
            RemoteAdminCommand::ListWhitelist,
            &holder.context(&dir.join("missing")),
        );
        let response: RemoteAdminResponse = serde_json::from_str(&body).expect("envelope parses");
        assert!(response.ok, "{response:?}");
        assert_eq!(response.whitelist, Some(Vec::new()), "{response:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_whitelist_response_survives_a_missing_field_round_trip() {
        // Envelopes written before `whitelist` existed (or by a peer that
        // never sets it) must still decode — the field is defaulted.
        let body = r#"{"ok":true,"message":"removed: x"}"#;
        let response: RemoteAdminResponse = serde_json::from_str(body).expect("envelope parses");
        assert_eq!(response.whitelist, None);
        assert_eq!(response.message.as_deref(), Some("removed: x"));
    }
}
