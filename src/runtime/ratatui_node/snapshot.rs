use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::lifecycle::LifecycleError;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::client::ClientHandle;
use super::runtime::SharedState;

/// Status returned by the STATUS one-shot command.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerStatus {
    pub port: u16,
    pub client_count: usize,
    pub uptime_secs: u64,
    pub session_count: usize,
}

/// Snapshot sent from the node server to a TUI client on attach and update.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RatatuiSnapshot {
    pub session_name: String,
    pub client_count: usize,
    pub main: String,
    pub main_lines: Vec<String>,
    pub main_cursor: Option<(u16, u16)>,
    pub sidebar: String,
    pub footer: FooterState,
    pub sessions: Vec<SessionView>,
    pub active_target: Option<String>,
}

/// Serializable session row exposed to the TUI client for sidebar rendering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionView {
    pub id: String,
    pub transport: String,
    pub command_name: String,
    pub authority_node_id: String,
    pub display_authority_id: String,
    pub session_id: String,
    pub task_state: String,
    pub availability: String,
    pub attached_clients: usize,
}

impl SessionView {
    pub(crate) fn from_record(record: &ManagedSessionRecord) -> Self {
        let command_name = record
            .display_command_name
            .as_deref()
            .or(record.command_name.as_deref())
            .unwrap_or("bash")
            .to_string();
        let authority_node_id = record.address.authority_id().to_string();
        let display_authority_id = record.address.display_authority_id().to_string();
        Self {
            id: record.address.qualified_target(),
            transport: match record.address.transport() {
                SessionTransport::LocalTmux => "local".to_string(),
                SessionTransport::RemotePeer => "remote".to_string(),
            },
            command_name,
            authority_node_id,
            display_authority_id,
            session_id: record.address.session_id().to_string(),
            task_state: record.task_state.as_str().to_string(),
            availability: record.availability.as_str().to_string(),
            attached_clients: record.attached_clients,
        }
    }

    pub fn display_label(&self) -> String {
        match self.transport.as_str() {
            "local" => format!("{}@local", self.command_name),
            _ => self.remote_row_label(),
        }
    }

    fn remote_row_label(&self) -> String {
        let (host, port) = self
            .authority_node_id
            .split_once('#')
            .map(|(host, port)| (host, Some(port)))
            .unwrap_or((self.display_authority_id.as_str(), None));
        match port {
            Some(port) => format!("{}@{}:{}", self.command_name, host, port),
            None => format!("{}@{}", self.command_name, host),
        }
    }

    pub fn display_label_candidates(&self) -> Vec<String> {
        match self.transport.as_str() {
            "local" => vec![self.display_label()],
            _ => {
                let mut candidates = Vec::new();
                let full = self.remote_row_label();
                let host_only = format!("{}@{}", self.command_name, self.display_authority_id);
                if full != host_only {
                    candidates.push(full);
                }
                candidates.push(host_only);
                candidates
            }
        }
    }
}

/// Footer state rendered by the TUI client.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FooterState {
    pub active_session: String,
    pub sessions: Vec<SessionSummary>,
    pub listener_endpoint: Option<String>,
    pub connect_endpoint: Option<String>,
    pub remote_count: usize,
}

/// A single entry in the footer session list.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionSummary {
    pub name: String,
    pub client_count: usize,
}

pub(crate) fn build_snapshot(client_count: usize, shared: &SharedState) -> RatatuiSnapshot {
    let guard = shared.sessions.lock().unwrap();
    let sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
    let active_target = shared.active_target.lock().unwrap().clone();
    let active_session_id = active_target
        .as_deref()
        .and_then(|target| {
            guard
                .values()
                .find(|s| s.address.qualified_target() == target)
        })
        .map(|s| s.address.session_id().to_string())
        .unwrap_or_else(|| super::runtime::DEFAULT_SESSION_ID.to_string());
    drop(guard);

    let (main_lines, main_cursor) = active_target
        .as_deref()
        .and_then(|target| target.split_once(':').map(|(_, id)| id.to_string()))
        .and_then(|session_id| {
            let local_guard = shared.local_sessions.lock().unwrap();
            local_guard.get(&session_id).map(|s| s.snapshot())
        })
        .unwrap_or_else(|| (Vec::new(), None));

    RatatuiSnapshot {
        session_name: active_session_id.clone(),
        client_count,
        main: main_lines.join("\n"),
        main_lines,
        main_cursor,
        sidebar: "Sessions".to_string(),
        footer: FooterState {
            active_session: active_session_id,
            sessions: vec![],
            listener_endpoint: Some(shared.network.advertised_listener_label().to_string()),
            connect_endpoint: shared.network.connect_endpoint_uri(),
            remote_count: sessions
                .iter()
                .filter(|session| session.transport == "remote")
                .count(),
        },
        sessions,
        active_target,
    }
}

pub(crate) fn broadcast_snapshot(
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
    shared: &SharedState,
) -> Result<(), LifecycleError> {
    let snapshot = build_snapshot(0, shared);
    let json = serde_json::to_string(&snapshot).unwrap_or_default();
    let mut guard = clients.lock().unwrap();
    guard.retain(|handle| !handle.removed.load(Ordering::SeqCst));
    for handle in guard.iter() {
        let mut stream = &handle.stream;
        let _ = writeln!(stream, "{json}");
        let _ = stream.flush();
    }
    Ok(())
}
