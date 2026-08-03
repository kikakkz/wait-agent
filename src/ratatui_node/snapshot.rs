use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};

fn is_remote_target(target: &str, shared: &SharedState) -> bool {
    let guard = shared
        .sessions
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard
        .get(target)
        .map(|s| s.address.transport() == &SessionTransport::RemotePeer)
        .unwrap_or(false)
}

use super::runtime::SharedState;

/// Status returned by the STATUS one-shot command.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerStatus {
    pub port: u16,
    pub client_count: usize,
    pub uptime_secs: u64,
    pub session_count: usize,
}

/// Structured response returned by control commands.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub broadcast: bool,
}

impl ControlResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            ..Default::default()
        }
    }

    pub fn ok_message(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(message.into()),
            ..Default::default()
        }
    }

    pub fn ok_data(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            ..Default::default()
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(message.into()),
            ..Default::default()
        }
    }
}

/// History buffer returned by the GET_HISTORY command.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryResponse {
    pub target_id: String,
    pub lines: Vec<String>,
    pub styled_lines: Vec<String>,
}

/// Top-level wire message sent from the node server to clients.
///
/// Using an explicit `type` tag keeps snapshots and command responses
/// unambiguous without relying on field-count heuristics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ServerMessageJson {
    Snapshot(Box<RatatuiSnapshot>),
    Response(ControlResponse),
    History(HistoryResponse),
}

pub(crate) fn snapshot_json(snapshot: &RatatuiSnapshot) -> String {
    serde_json::to_string(&ServerMessageJson::Snapshot(Box::new(snapshot.clone())))
        .unwrap_or_default()
}

pub(crate) fn response_json(response: &ControlResponse) -> String {
    serde_json::to_string(&ServerMessageJson::Response(response.clone())).unwrap_or_default()
}

pub(crate) fn history_response_json(response: &HistoryResponse) -> String {
    serde_json::to_string(&ServerMessageJson::History(response.clone())).unwrap_or_default()
}

/// Snapshot sent from the node server to a TUI client on attach and update.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RatatuiSnapshot {
    pub session_name: String,
    pub client_count: usize,
    pub main: String,
    pub main_lines: Vec<String>,
    pub main_styled_lines: Vec<String>,
    pub main_cursor: Option<(u16, u16)>,
    pub sidebar: String,
    pub footer: FooterState,
    pub sessions: Vec<SessionView>,
    pub active_target: Option<String>,
}

/// Serializable session row exposed to the TUI client for sidebar rendering.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    pub current_path: Option<String>,
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
                SessionTransport::Local => "local".to_string(),
                SessionTransport::RemotePeer => "remote".to_string(),
            },
            command_name,
            authority_node_id,
            display_authority_id,
            session_id: record.address.session_id().to_string(),
            task_state: record.task_state.as_str().to_string(),
            availability: record.availability.as_str().to_string(),
            attached_clients: record.attached_clients,
            current_path: record
                .current_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
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
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FooterState {
    pub active_session: String,
    pub sessions: Vec<SessionSummary>,
    pub listener_endpoint: Option<String>,
    pub connect_endpoint: Option<String>,
    pub remote_count: usize,
}

/// A single entry in the footer session list.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionSummary {
    pub name: String,
    pub client_count: usize,
}

pub(crate) fn build_snapshot(client_count: usize, shared: &SharedState) -> RatatuiSnapshot {
    let guard = shared
        .sessions
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
    // Stable ordering keeps the sidebar selection predictable across
    // reconnects and snapshots: local sessions first, then remote peers,
    // each group sorted by qualified target id.
    sessions.sort_by(|a, b| {
        let a_local = a.transport == "local";
        let b_local = b.transport == "local";
        b_local.cmp(&a_local).then_with(|| a.id.cmp(&b.id))
    });

    let active_target = shared
        .sessions
        .active_target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let active_session_id = active_target
        .as_deref()
        .and_then(|target| guard.get(target))
        .map(|s| s.address.session_id().to_string())
        .unwrap_or_else(|| super::runtime::DEFAULT_SESSION_ID.to_string());
    drop(guard);

    let (main_lines, main_styled_lines, main_cursor) = active_target
        .as_deref()
        .map(|target| {
            if is_remote_target(target, shared) {
                let remote_guard = shared
                    .sessions
                    .remote_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                remote_guard
                    .get(target)
                    .map(|s| s.snapshot())
                    .unwrap_or_else(|| (Vec::new(), Vec::new(), None))
            } else {
                let local_guard = shared
                    .sessions
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                local_guard
                    .get(target)
                    .map(|s| s.snapshot())
                    .unwrap_or_else(|| (Vec::new(), Vec::new(), None))
            }
        })
        .unwrap_or_else(|| (Vec::new(), Vec::new(), None));

    RatatuiSnapshot {
        session_name: active_session_id.clone(),
        client_count,
        main: main_lines.join("\n"),
        main_lines,
        main_styled_lines,
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };

    fn sample_session_view() -> SessionView {
        SessionView::from_record(&ManagedSessionRecord {
            address: ManagedSessionAddress::local("local#17474", "1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 3,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: Some("demo".to_string()),
            current_path: Some(std::path::PathBuf::from("/tmp")),
            task_state: ManagedSessionTaskState::Input,
        })
    }

    fn sample_snapshot() -> RatatuiSnapshot {
        RatatuiSnapshot {
            session_name: "1".to_string(),
            client_count: 2,
            main: "hello".to_string(),
            main_lines: vec!["hello".to_string()],
            main_styled_lines: vec!["hello".to_string()],
            main_cursor: Some((0, 0)),
            sidebar: "Sessions".to_string(),
            footer: FooterState {
                active_session: "1".to_string(),
                sessions: vec![SessionSummary {
                    name: "1".to_string(),
                    client_count: 2,
                }],
                listener_endpoint: Some("0.0.0.0:17474".to_string()),
                connect_endpoint: None,
                remote_count: 0,
            },
            sessions: vec![sample_session_view()],
            active_target: Some("local#17474:1".to_string()),
        }
    }

    #[test]
    fn snapshot_serializes_and_deserializes() {
        let snap = sample_snapshot();
        let json = serde_json::to_string(&snap).expect("serialize snapshot");
        let decoded: RatatuiSnapshot = serde_json::from_str(&json).expect("deserialize snapshot");
        assert_eq!(snap, decoded);
    }

    #[test]
    fn session_view_round_trips() {
        let view = sample_session_view();
        let json = serde_json::to_string(&view).expect("serialize session view");
        let decoded: SessionView = serde_json::from_str(&json).expect("deserialize session view");
        assert_eq!(view, decoded);
    }
}
