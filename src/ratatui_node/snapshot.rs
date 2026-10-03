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

use super::runtime::{RelayLinkErrorSnapshot, SharedState};

/// Console-facing view of the last connection-level relay error. Rendered
/// from the structured snapshot channel; carries the wire code plus the
/// relay's message so unassigned (future) codes stay explainable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelayErrorView {
    pub code: u16,
    pub message: String,
}

impl RelayErrorView {
    pub(crate) fn from_snapshot(snapshot: RelayLinkErrorSnapshot) -> Self {
        Self {
            code: snapshot.code,
            message: snapshot.message,
        }
    }
}

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

pub(crate) fn snapshot_json(snapshot: RatatuiSnapshot) -> String {
    serde_json::to_string(&ServerMessageJson::Snapshot(Box::new(snapshot))).unwrap_or_default()
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
    #[serde(default)]
    pub main_cursor_visible: bool,
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
    pub agent_command_name: Option<String>,
    pub authority_node_id: String,
    pub display_authority_id: String,
    pub session_id: String,
    pub task_state: String,
    pub availability: String,
    pub attached_clients: usize,
    pub current_path: Option<String>,
    /// Relay presence of the session's authority node ("online"/"offline")
    /// when the node is watched on the relay; `None` for local sessions and
    /// unwatched/unknown peers. Filled by `build_snapshot`.
    #[serde(default)]
    pub relay_presence: Option<String>,
    /// Last connection-level relay link error, attached by `build_snapshot`
    /// to relay-routed sessions only (`relay_presence` is `Some`).
    #[serde(default)]
    pub relay_error: Option<RelayErrorView>,
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
            agent_command_name: record.agent_command_name.clone(),
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
            relay_presence: None,
            relay_error: None,
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
    pub public_endpoint: Option<String>,
    pub connect_endpoint: Option<String>,
    pub remote_count: usize,
    /// Relay-watched peers currently online.
    #[serde(default)]
    pub relay_peers_online: usize,
    /// Relay-watched peers in total (online + offline).
    #[serde(default)]
    pub relay_watch_count: usize,
    /// Last connection-level relay link error, for the console status line.
    #[serde(default)]
    pub relay_error: Option<RelayErrorView>,
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

    let session_snap = active_target
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
                    .unwrap_or_default()
            } else {
                let local_guard = shared
                    .sessions
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                local_guard
                    .get(target)
                    .map(|s| s.snapshot())
                    .unwrap_or_default()
            }
        })
        .unwrap_or_default();
    let main_lines = session_snap.lines;
    let main_styled_lines = session_snap.styled_lines;
    let main_cursor = session_snap.cursor;
    let main_cursor_visible = session_snap.cursor_visible;

    // Relay presence: correlate each remote session's authority node with
    // the watched-node map (matched on the peer's TLS pin = certificate
    // fingerprint). `relay_presence` is a leaf lock, taken after the session
    // locks above have been released.
    let presence_guard = shared
        .relay_presence
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for session in sessions.iter_mut() {
        if session.transport != "remote" {
            continue;
        }
        let state = shared
            .remote_node_connection(&session.authority_node_id)
            .map(|info| info.tls_pin_sha256.to_lowercase())
            .and_then(|fingerprint| presence_guard.get(&fingerprint).copied());
        session.relay_presence = state.map(|online| {
            if online {
                "online".to_string()
            } else {
                "offline".to_string()
            }
        });
    }
    let relay_watch_count = presence_guard.len();
    let relay_peers_online = presence_guard.values().filter(|online| **online).count();
    drop(presence_guard);

    // Relay link error: read once (leaf lock), surfaced in the footer and
    // attached to every relay-routed session (`relay_presence` is `Some`) —
    // a link-level error concerns exactly those.
    let relay_error = shared
        .relay_error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .map(RelayErrorView::from_snapshot);
    for session in sessions.iter_mut() {
        if session.relay_presence.is_some() {
            session.relay_error = relay_error.clone();
        }
    }

    RatatuiSnapshot {
        session_name: active_session_id.clone(),
        client_count,
        main: main_lines.join("\n"),
        main_lines,
        main_styled_lines,
        main_cursor,
        main_cursor_visible,
        sidebar: "Sessions".to_string(),
        footer: FooterState {
            active_session: active_session_id,
            sessions: vec![],
            listener_endpoint: Some(shared.network.advertised_listener_label().to_string()),
            public_endpoint: Some(shared.advertised_public_endpoint_label().to_string()),
            connect_endpoint: shared.network.connect_endpoint_uri(),
            remote_count: sessions
                .iter()
                .filter(|session| session.transport == "remote")
                .count(),
            relay_peers_online,
            relay_watch_count,
            relay_error,
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
            agent_command_name: None,
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
            main_cursor_visible: true,
            sidebar: "Sessions".to_string(),
            footer: FooterState {
                active_session: "1".to_string(),
                sessions: vec![SessionSummary {
                    name: "1".to_string(),
                    client_count: 2,
                }],
                listener_endpoint: Some("0.0.0.0:17474".to_string()),
                public_endpoint: Some("0.0.0.0:17474".to_string()),
                connect_endpoint: None,
                remote_count: 0,
                relay_peers_online: 0,
                relay_watch_count: 0,
                relay_error: None,
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
    fn snapshot_presence_fields_round_trip() {
        let mut snap = sample_snapshot();
        snap.footer.relay_peers_online = 1;
        snap.footer.relay_watch_count = 2;
        snap.footer.relay_error = Some(RelayErrorView {
            code: 0x0009,
            message: "node access revoked by the relay operator".to_string(),
        });
        let mut remote_view = sample_session_view();
        remote_view.transport = "remote".to_string();
        remote_view.relay_presence = Some("online".to_string());
        remote_view.relay_error = Some(RelayErrorView {
            code: 0x0009,
            message: "node access revoked by the relay operator".to_string(),
        });
        snap.sessions.push(remote_view);
        let json = serde_json::to_string(&snap).expect("serialize snapshot");
        let decoded: RatatuiSnapshot = serde_json::from_str(&json).expect("deserialize snapshot");
        assert_eq!(snap, decoded);
        assert_eq!(decoded.footer.relay_peers_online, 1);
        assert_eq!(decoded.footer.relay_watch_count, 2);
        assert_eq!(
            decoded.sessions[1].relay_presence.as_deref(),
            Some("online")
        );
        assert_eq!(
            decoded.sessions[1].relay_error.as_ref().map(|e| e.code),
            Some(0x0009)
        );
    }

    fn sample_remote_record() -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("peer#1", "1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            agent_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        }
    }

    fn sample_local_record() -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local("local#17474", "1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: Some("demo".to_string()),
            agent_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        }
    }

    #[test]
    fn snapshot_relay_error_reaches_routed_sessions_only() {
        use super::super::runtime::{
            RelayLinkErrorSnapshot, RemoteNodeConnectionInfo, RemoteNodeConnectionMode,
        };
        use crate::cli::RemoteNetworkConfig;

        let shared = SharedState::new(RemoteNetworkConfig::default())
            .expect("SharedState::new should succeed");
        let local = sample_local_record();
        let remote = sample_remote_record();
        {
            let mut guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.insert(local.address.qualified_target(), local);
            guard.insert(remote.address.qualified_target(), remote);
        }
        shared.record_remote_node_connection(
            "peer#1",
            RemoteNodeConnectionInfo {
                mode: RemoteNodeConnectionMode::OutboundDial,
                host: "peer".to_string(),
                port: 1,
                tls_pin_sha256: "DEADBEEF".to_string(),
                profile_name: "profile".to_string(),
                server_can_reach_peer: false,
                via: None,
            },
        );
        shared
            .relay_presence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("deadbeef".to_string(), true);
        *shared.relay_error.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(RelayLinkErrorSnapshot {
                code: 0x0009,
                message: "node access revoked by the relay operator".to_string(),
            });

        let snapshot = build_snapshot(0, &shared);
        let routed = snapshot
            .sessions
            .iter()
            .find(|s| s.transport == "remote")
            .expect("remote session in snapshot");
        assert_eq!(
            routed.relay_error,
            Some(RelayErrorView {
                code: 0x0009,
                message: "node access revoked by the relay operator".to_string(),
            }),
            "a relay-routed session carries the link error"
        );
        assert_eq!(routed.relay_presence.as_deref(), Some("online"));
        let local_view = snapshot
            .sessions
            .iter()
            .find(|s| s.transport == "local")
            .expect("local session in snapshot");
        assert_eq!(
            local_view.relay_error, None,
            "a local session is not relay-routed and carries no link error"
        );
        assert_eq!(
            snapshot.footer.relay_error,
            Some(RelayErrorView {
                code: 0x0009,
                message: "node access revoked by the relay operator".to_string(),
            }),
            "the footer carries the link error once"
        );
    }

    #[test]
    fn session_view_round_trips() {
        let view = sample_session_view();
        let json = serde_json::to_string(&view).expect("serialize session view");
        let decoded: SessionView = serde_json::from_str(&json).expect("deserialize session view");
        assert_eq!(view, decoded);
    }

    #[test]
    fn session_view_preserves_agent_command_name() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local("local#17474", "1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            agent_command_name: Some("kimi".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        };
        let view = SessionView::from_record(&record);
        assert_eq!(view.command_name, "bash");
        assert_eq!(view.agent_command_name.as_deref(), Some("kimi"));
    }
}
