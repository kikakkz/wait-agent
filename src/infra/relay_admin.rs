//! Relay local admin socket: the owner-only local channel for bootstrap
//! and emergency management (docs/relay-design.md 管理通道). Follows the
//! owner-control pattern in `platform::remote_ipc`: UDS under the temp dir
//! on Unix, TCP loopback with a derived port on Windows.
//!
//! Protocol: one JSON request per connection — the client writes the
//! request and half-closes; the server reads to EOF, answers once, and
//! closes (the exact idiom of the remote runtime owner control channel).
//! `status` queries the connection table; `shutdown` stops the relay
//! gracefully. invite/remove land with the enrollment slice.

use std::net::SocketAddr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::relay_capacity::{RelayCapacityConfig, RelayUsage, SharedUsageMeter};
use crate::infra::relay_connection_table::RelayConnectionTable;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayAdminCommand {
    Status,
    Shutdown,
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
        other => Err(format!("unknown admin command: {other:?}")),
    }
}

/// Builds the response for a parsed command against a table snapshot. Pure;
/// the shutdown side effect lives in [`run_admin_listener`].
pub(crate) fn handle_relay_admin_command(
    command: RelayAdminCommand,
    snapshot: RelayAdminStatus,
) -> RelayAdminResponse {
    match command {
        RelayAdminCommand::Status => RelayAdminResponse::status(snapshot),
        RelayAdminCommand::Shutdown => RelayAdminResponse::shutdown(),
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
    fn parse_rejects_unknown_and_malformed_requests() {
        let unknown = parse_relay_admin_command(r#"{"command":"invite"}"#).unwrap_err();
        assert!(unknown.contains("unknown admin command"));
        let malformed = parse_relay_admin_command("{not json").unwrap_err();
        assert!(malformed.contains("invalid admin request"));
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
