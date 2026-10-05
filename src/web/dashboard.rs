//! The read-only relay dashboard (issue #131, slice 2). Data reaches this
//! process only through its enrolled node identity: the handler issues a
//! `status` admin request over the authenticated node<->relay link
//! ([`RelayClientHandle::admin_request`]) and renders the answer with an
//! askama template (compile-time checked, HTML-escaped, zero JS framework;
//! the 10s poll is a plain `<meta http-equiv="refresh">`).
//!
//! domain-web: the handler is async end to end — the admin request future
//! suspends instead of blocking a worker — and shared state enters through
//! the extractor as `Arc<DashboardState>`.

use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use thiserror::Error;
use tokio::time::timeout;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::relay_client::{RelayAdminError, RelayClientHandle};
use crate::infra::relay_remote_admin::{RemoteAdminResponse, RemoteAdminStatus};

/// The `status` request body, identical to the local admin protocol.
const STATUS_COMMAND: &str = r#"{"command":"status"}"#;

/// Upper bound on one dashboard refresh: the relay answers on the same
/// loopback link in milliseconds; anything beyond is a broken link.
const ADMIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared dashboard state (domain-web: state in the extractor).
pub(crate) struct DashboardState {
    /// The enrolled web node's persistent relay link.
    client: RelayClientHandle,
}

impl DashboardState {
    pub(crate) fn new(client: RelayClientHandle) -> Self {
        Self { client }
    }
}

/// `GET /`: fetch the relay status over the node channel and render it.
/// A failed fetch answers 503 rather than a cached or partial page — the
/// dashboard is read-only and must not imply stale truth.
pub(crate) async fn dashboard(State(state): State<Arc<DashboardState>>) -> Response {
    match fetch_status(&state.client).await {
        Ok(status) => Html(render(status)).into_response(),
        Err(error) => {
            ERROR_LOG.log_warn(format!("[web] dashboard status fetch failed: {error}"));
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "relay status unavailable\n",
            )
                .into_response()
        }
    }
}

/// Errors of the dashboard status fetch.
#[derive(Debug, Error)]
enum DashboardError {
    #[error("relay link is not established")]
    NotConnected,
    #[error("relay admin request timed out")]
    Timeout,
    #[error("relay admin response error: {0}")]
    Relay(String),
    #[error("relay admin response undecodable: {0}")]
    Undecodable(String),
}

impl From<RelayAdminError> for DashboardError {
    fn from(error: RelayAdminError) -> Self {
        match error {
            RelayAdminError::NotConnected => Self::NotConnected,
            RelayAdminError::LinkLost => Self::Relay("relay link lost".to_string()),
        }
    }
}

async fn fetch_status(client: &RelayClientHandle) -> Result<RemoteAdminStatus, DashboardError> {
    let body = timeout(ADMIN_REQUEST_TIMEOUT, client.admin_request(STATUS_COMMAND))
        .await
        .map_err(|_| DashboardError::Timeout)??;
    let response: RemoteAdminResponse = serde_json::from_str(&body)
        .map_err(|error| DashboardError::Undecodable(error.to_string()))?;
    match response {
        RemoteAdminResponse {
            ok: true,
            status: Some(status),
            ..
        } => Ok(status),
        RemoteAdminResponse {
            error: Some(message),
            ..
        } => Err(DashboardError::Relay(message)),
        other => Err(DashboardError::Undecodable(format!(
            "unexpected admin envelope: {other:?}"
        ))),
    }
}

/// One connection-table row in the template.
struct NodeRow {
    node_id: String,
    idle_ms: u128,
    /// The connection table lists registered nodes, so every row is online;
    /// the column exists so a future presence feed does not need a template
    /// change (and color never carries the meaning alone).
    online: bool,
}

/// The askama view: pure data, pre-formatted strings, no logic in the
/// template beyond the row loop.
#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    listen: String,
    uptime: String,
    registered_nodes: usize,
    active_streams: usize,
    forwarded_bytes_per_sec: u64,
    max_nodes: usize,
    max_streams: usize,
    max_throughput_bytes_per_sec: u64,
    nodes: Vec<NodeRow>,
}

fn render(status: RemoteAdminStatus) -> String {
    let template = DashboardTemplate {
        listen: status.listen.to_string(),
        uptime: format_uptime(status.uptime_ms),
        registered_nodes: status.registered_nodes,
        active_streams: status.usage.active_streams,
        forwarded_bytes_per_sec: status.usage.forwarded_bytes_per_sec,
        max_nodes: status.capacity.max_nodes,
        max_streams: status.capacity.max_streams,
        max_throughput_bytes_per_sec: status.capacity.max_throughput_bytes_per_sec,
        nodes: status
            .nodes
            .into_iter()
            .map(|node| NodeRow {
                node_id: node.node_id,
                idle_ms: node.idle_ms,
                online: true,
            })
            .collect(),
    };
    template
        .render()
        .unwrap_or_else(|error| format!("dashboard render failed: {error}"))
}

/// `HH:MM:SS`, folding whole days into a `Nd ` prefix.
fn format_uptime(ms: u64) -> String {
    let secs = ms / 1000;
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use crate::infra::relay_capacity::{RelayCapacityConfig, RelayUsage};
    use crate::infra::relay_remote_admin::{RemoteAdminNodeEntry, RemoteAdminStatus};

    fn sample_status() -> RemoteAdminStatus {
        RemoteAdminStatus {
            listen: SocketAddr::from(([127, 0, 0, 1], 7475)),
            uptime_ms: 7_000,
            registered_nodes: 2,
            nodes: vec![
                RemoteAdminNodeEntry {
                    node_id: "a".repeat(64),
                    idle_ms: 12,
                },
                RemoteAdminNodeEntry {
                    node_id: "b".repeat(64),
                    idle_ms: 0,
                },
            ],
            capacity: RelayCapacityConfig {
                max_nodes: 8,
                max_streams: 16,
                max_throughput_bytes_per_sec: 1024,
            },
            usage: RelayUsage {
                registered_nodes: 2,
                active_streams: 3,
                forwarded_bytes_per_sec: 512,
            },
        }
    }

    #[test]
    fn dashboard_renders_status_and_connection_table() {
        let html = render(sample_status());
        assert!(html.contains("127.0.0.1:7475"), "listen: {html}");
        assert!(html.contains("up 00:00:07"), "uptime: {html}");
        assert!(html.contains("2 / 8"), "registered/capacity: {html}");
        assert!(html.contains("3 / 16"), "streams/capacity: {html}");
        assert!(
            html.contains("512 B/s (cap 1024 B/s)"),
            "throughput: {html}"
        );
        assert!(html.contains(&"a".repeat(64)), "node id row: {html}");
        assert!(html.contains(">12<"), "idle column: {html}");
        assert!(html.contains("online"), "state column: {html}");
        assert!(
            html.contains(r#"<meta http-equiv="refresh" content="10">"#),
            "auto refresh: {html}"
        );
    }

    #[test]
    fn dashboard_renders_an_empty_connection_table() {
        let mut status = sample_status();
        status.registered_nodes = 0;
        status.nodes = Vec::new();
        status.usage.registered_nodes = 0;
        let html = render(status);
        assert!(html.contains("no nodes registered"), "{html}");
    }

    #[test]
    fn uptime_formats_days_hours_minutes_seconds() {
        assert_eq!(format_uptime(7_000), "00:00:07");
        assert_eq!(format_uptime(3_723_000), "01:02:03");
        assert_eq!(format_uptime(86_400_000 + 3_723_000), "1d 01:02:03");
    }
}
