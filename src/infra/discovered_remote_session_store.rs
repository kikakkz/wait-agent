use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::tmux::TmuxError;
use std::fs;
use std::path::PathBuf;

const DISCOVERED_REMOTE_SESSION_RECORD_VERSION: &str = "v1";
const OPTIONAL_NONE_SENTINEL: &str = "~";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRemoteSessionStore {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRemoteSessionRecord {
    pub node_id: String,
    pub session: ManagedSessionRecord,
}

impl Default for DiscoveredRemoteSessionStore {
    fn default() -> Self {
        Self::new(default_discovered_remote_session_store_path())
    }
}

impl DiscoveredRemoteSessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn list_sessions(&self) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        Ok(self
            .list_records()?
            .into_iter()
            .map(|record| record.session)
            .collect())
    }

    pub fn list_records(&self) -> Result<Vec<DiscoveredRemoteSessionRecord>, TmuxError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let contents = fs::read_to_string(&self.path).map_err(|error| {
            TmuxError::new(format!(
                "failed to read discovered remote session store {}: {error}",
                self.path.display()
            ))
        })?;
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_discovered_remote_session_record)
            .collect()
    }

    pub fn list_records_for_node(
        &self,
        node_id: &str,
    ) -> Result<Vec<DiscoveredRemoteSessionRecord>, TmuxError> {
        Ok(self
            .list_records()?
            .into_iter()
            .filter(|record| record.node_id == node_id)
            .collect())
    }

    pub fn upsert_session_from_node(
        &self,
        node_id: &str,
        session: &ManagedSessionRecord,
    ) -> Result<bool, TmuxError> {
        validate_discovered_remote_session(session)?;
        let mut records = self.list_records()?;
        let updated = DiscoveredRemoteSessionRecord {
            node_id: node_id.to_string(),
            session: session.clone(),
        };
        if let Some(existing) = records.iter_mut().find(|record| {
            record.node_id == node_id
                && record.session.address.id().as_str() == session.address.id().as_str()
        }) {
            if *existing == updated {
                return Ok(false);
            }
            *existing = updated;
        } else {
            records.push(updated);
        }
        self.write_records(&records)?;
        Ok(true)
    }

    pub fn remove_session_from_node(
        &self,
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<bool, TmuxError> {
        let session_id = ManagedSessionAddress::remote_peer(authority_id, transport_session_id)
            .id()
            .as_str()
            .to_string();
        let mut records = self.list_records()?;
        let prior_len = records.len();
        records.retain(|record| {
            !(record.node_id == node_id && record.session.address.id().as_str() == session_id)
        });
        if prior_len == records.len() {
            return Ok(false);
        }
        self.write_records(&records)?;
        Ok(true)
    }

    pub fn mark_node_sessions_offline(&self, node_id: &str) -> Result<bool, TmuxError> {
        let mut records = self.list_records()?;
        let mut changed = false;
        for record in &mut records {
            if record.node_id != node_id {
                continue;
            }
            if record.session.availability != SessionAvailability::Offline {
                record.session.availability = SessionAvailability::Offline;
                changed = true;
            }
        }
        if changed {
            self.write_records(&records)?;
        }
        Ok(changed)
    }

    fn write_records(&self, records: &[DiscoveredRemoteSessionRecord]) -> Result<(), TmuxError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                TmuxError::new(format!(
                    "failed to create discovered remote session directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let contents = if records.is_empty() {
            String::new()
        } else {
            let mut lines = records
                .iter()
                .map(render_discovered_remote_session_record)
                .collect::<Vec<_>>()
                .join("\n");
            lines.push('\n');
            lines
        };
        fs::write(&self.path, contents).map_err(|error| {
            TmuxError::new(format!(
                "failed to write discovered remote session store {}: {error}",
                self.path.display()
            ))
        })
    }
}

fn default_discovered_remote_session_store_path() -> PathBuf {
    std::env::temp_dir().join("waitagent-discovered-remote-sessions.tsv")
}

fn render_discovered_remote_session_record(record: &DiscoveredRemoteSessionRecord) -> String {
    let current_path = record
        .session
        .current_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    [
        DISCOVERED_REMOTE_SESSION_RECORD_VERSION.to_string(),
        encode_string_field(&record.node_id),
        encode_string_field(record.session.address.authority_id()),
        encode_string_field(record.session.address.session_id()),
        encode_optional_string_field(record.session.selector.as_deref()),
        record.session.availability.as_str().to_string(),
        encode_optional_string_field(
            record
                .session
                .session_role
                .as_ref()
                .map(WorkspaceSessionRole::as_str),
        ),
        encode_optional_string_field(record.session.workspace_key.as_deref()),
        encode_optional_string_field(record.session.command_name.as_deref()),
        encode_optional_string_field(current_path.as_deref()),
        record.session.attached_clients.to_string(),
        record.session.window_count.to_string(),
    ]
    .join("\t")
}

fn parse_discovered_remote_session_record(
    line: &str,
) -> Result<DiscoveredRemoteSessionRecord, TmuxError> {
    let parts = line.split('\t').collect::<Vec<_>>();
    if parts.len() != 12 {
        return Err(TmuxError::new(format!(
            "discovered remote session record version `{}` must contain 12 tab-separated fields, got {}",
            DISCOVERED_REMOTE_SESSION_RECORD_VERSION,
            parts.len()
        )));
    }
    if parts[0] != DISCOVERED_REMOTE_SESSION_RECORD_VERSION {
        return Err(TmuxError::new(format!(
            "unsupported discovered remote session record version `{}`",
            parts[0]
        )));
    }

    let node_id = decode_string_field(parts[1])?;
    let authority_id = decode_string_field(parts[2])?;
    let transport_session_id = decode_string_field(parts[3])?;
    let selector = decode_optional_string_field(parts[4])?;
    let availability = SessionAvailability::parse(parts[5]).ok_or_else(|| {
        TmuxError::new(format!(
            "unsupported discovered remote session availability `{}`",
            parts[5]
        ))
    })?;
    let session_role = decode_optional_string_field(parts[6])?
        .as_deref()
        .and_then(WorkspaceSessionRole::parse);
    let workspace_key = decode_optional_string_field(parts[7])?;
    let command_name = decode_optional_string_field(parts[8])?;
    let current_path = decode_optional_string_field(parts[9])?.map(PathBuf::from);
    let attached_clients = parts[10].parse::<usize>().map_err(|error| {
        TmuxError::new(format!(
            "invalid discovered remote session attached client count `{}`: {error}",
            parts[10]
        ))
    })?;
    let window_count = parts[11].parse::<usize>().map_err(|error| {
        TmuxError::new(format!(
            "invalid discovered remote session window count `{}`: {error}",
            parts[11]
        ))
    })?;

    Ok(DiscoveredRemoteSessionRecord {
        node_id,
        session: ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, transport_session_id),
            selector,
            availability,
            workspace_dir: None,
            workspace_key,
            session_role,
            opened_by: Vec::new(),
            attached_clients,
            window_count,
            command_name,
            current_path,
            task_state: ManagedSessionTaskState::Unknown,
        },
    })
}

fn validate_discovered_remote_session(session: &ManagedSessionRecord) -> Result<(), TmuxError> {
    if session.address.transport() != &crate::domain::session_catalog::SessionTransport::RemotePeer
    {
        return Err(TmuxError::new(format!(
            "discovered session `{}` is not a remote-peer session",
            session.address.id().as_str()
        )));
    }
    Ok(())
}

fn encode_string_field(value: &str) -> String {
    encode_base64(value.as_bytes())
}

fn decode_string_field(value: &str) -> Result<String, TmuxError> {
    let decoded = decode_base64(value)
        .map_err(|error| TmuxError::new(format!("invalid base64 field `{value}`: {error}")))?;
    String::from_utf8(decoded).map_err(|error| {
        TmuxError::new(format!(
            "discovered remote session field is not valid UTF-8: {error}"
        ))
    })
}

fn encode_optional_string_field(value: Option<&str>) -> String {
    value
        .map(encode_string_field)
        .unwrap_or_else(|| OPTIONAL_NONE_SENTINEL.to_string())
}

fn decode_optional_string_field(value: &str) -> Result<Option<String>, TmuxError> {
    if value == OPTIONAL_NONE_SENTINEL {
        return Ok(None);
    }
    decode_string_field(value).map(Some)
}

#[cfg(test)]
mod tests {
    use super::{DiscoveredRemoteSessionStore, SessionAvailability};
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::path::PathBuf;

    #[test]
    fn discovered_remote_session_store_round_trips_node_records() {
        let path = test_store_path("round-trip");
        let store = DiscoveredRemoteSessionStore::new(&path);
        store
            .upsert_session_from_node("peer-a", &remote_session("peer-a", "shell-1"))
            .expect("session should store");

        let stored = store.list_records().expect("records should load");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].node_id, "peer-a");
        assert_eq!(
            stored[0].session.address.qualified_target(),
            "peer-a:shell-1"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn discovered_remote_session_store_marks_node_sessions_offline() {
        let path = test_store_path("offline");
        let store = DiscoveredRemoteSessionStore::new(&path);
        store
            .upsert_session_from_node("peer-a", &remote_session("peer-a", "shell-1"))
            .expect("session should store");

        let changed = store
            .mark_node_sessions_offline("peer-a")
            .expect("offline mark should succeed");

        assert!(changed);
        let record = store
            .list_records_for_node("peer-a")
            .expect("records should load")
            .into_iter()
            .next()
            .expect("record should remain present");
        assert_eq!(record.session.availability, SessionAvailability::Offline);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn discovered_remote_session_store_removes_only_matching_node_session() {
        let path = test_store_path("remove");
        let store = DiscoveredRemoteSessionStore::new(&path);
        store
            .upsert_session_from_node("peer-a", &remote_session("peer-a", "shell-1"))
            .expect("first session should store");
        store
            .upsert_session_from_node("peer-b", &remote_session("peer-b", "shell-1"))
            .expect("second session should store");

        let changed = store
            .remove_session_from_node("peer-a", "peer-a", "shell-1")
            .expect("session should remove");

        assert!(changed);
        let remaining = store.list_sessions().expect("sessions should load");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].address.qualified_target(), "peer-b:shell-1");

        let _ = std::fs::remove_file(path);
    }

    fn remote_session(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some(format!("wa-{authority_id}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Unknown,
        }
    }

    fn test_store_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-discovered-remote-session-store-{name}-{}-{}.tsv",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ))
    }
}
