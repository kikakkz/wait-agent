use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::tmux::TmuxError;
use std::fs;
use std::path::PathBuf;

const DISCOVERED_TARGET_RECORD_VERSION: &str = "v1";
const OPTIONAL_NONE_SENTINEL: &str = "~";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRemoteTargetStore {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRemoteTargetRecord {
    pub node_id: String,
    pub target: ManagedSessionRecord,
}

impl Default for DiscoveredRemoteTargetStore {
    fn default() -> Self {
        Self::new(default_discovered_remote_target_store_path())
    }
}

impl DiscoveredRemoteTargetStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        Ok(self
            .list_records()?
            .into_iter()
            .map(|record| record.target)
            .collect())
    }

    pub fn list_records(&self) -> Result<Vec<DiscoveredRemoteTargetRecord>, TmuxError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let contents = fs::read_to_string(&self.path).map_err(|error| {
            TmuxError::new(format!(
                "failed to read discovered remote target store {}: {error}",
                self.path.display()
            ))
        })?;
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_discovered_remote_target_record)
            .collect()
    }

    pub fn list_records_for_node(
        &self,
        node_id: &str,
    ) -> Result<Vec<DiscoveredRemoteTargetRecord>, TmuxError> {
        Ok(self
            .list_records()?
            .into_iter()
            .filter(|record| record.node_id == node_id)
            .collect())
    }

    pub fn upsert_target_from_node(
        &self,
        node_id: &str,
        target: &ManagedSessionRecord,
    ) -> Result<bool, TmuxError> {
        validate_discovered_remote_target(target)?;
        let mut records = self.list_records()?;
        let updated = DiscoveredRemoteTargetRecord {
            node_id: node_id.to_string(),
            target: target.clone(),
        };
        if let Some(existing) = records.iter_mut().find(|record| {
            record.node_id == node_id
                && record.target.address.id().as_str() == target.address.id().as_str()
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

    pub fn remove_target_from_node(
        &self,
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<bool, TmuxError> {
        let target_id = ManagedSessionAddress::remote_peer(authority_id, transport_session_id)
            .id()
            .as_str()
            .to_string();
        let mut records = self.list_records()?;
        let prior_len = records.len();
        records.retain(|record| {
            !(record.node_id == node_id && record.target.address.id().as_str() == target_id)
        });
        if prior_len == records.len() {
            return Ok(false);
        }
        self.write_records(&records)?;
        Ok(true)
    }

    pub fn mark_node_targets_offline(&self, node_id: &str) -> Result<bool, TmuxError> {
        let mut records = self.list_records()?;
        let mut changed = false;
        for record in &mut records {
            if record.node_id != node_id {
                continue;
            }
            if record.target.availability != SessionAvailability::Offline {
                record.target.availability = SessionAvailability::Offline;
                changed = true;
            }
        }
        if changed {
            self.write_records(&records)?;
        }
        Ok(changed)
    }

    fn write_records(&self, records: &[DiscoveredRemoteTargetRecord]) -> Result<(), TmuxError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                TmuxError::new(format!(
                    "failed to create discovered remote target directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let contents = if records.is_empty() {
            String::new()
        } else {
            let mut lines = records
                .iter()
                .map(render_discovered_remote_target_record)
                .collect::<Vec<_>>()
                .join("\n");
            lines.push('\n');
            lines
        };
        fs::write(&self.path, contents).map_err(|error| {
            TmuxError::new(format!(
                "failed to write discovered remote target store {}: {error}",
                self.path.display()
            ))
        })
    }
}

fn default_discovered_remote_target_store_path() -> PathBuf {
    std::env::temp_dir().join("waitagent-discovered-remote-targets.tsv")
}

fn render_discovered_remote_target_record(record: &DiscoveredRemoteTargetRecord) -> String {
    let current_path = record
        .target
        .current_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    [
        DISCOVERED_TARGET_RECORD_VERSION.to_string(),
        encode_string_field(&record.node_id),
        encode_string_field(record.target.address.authority_id()),
        encode_string_field(record.target.address.session_id()),
        encode_optional_string_field(record.target.selector.as_deref()),
        record.target.availability.as_str().to_string(),
        encode_optional_string_field(
            record
                .target
                .session_role
                .as_ref()
                .map(WorkspaceSessionRole::as_str),
        ),
        encode_optional_string_field(record.target.workspace_key.as_deref()),
        encode_optional_string_field(record.target.command_name.as_deref()),
        encode_optional_string_field(current_path.as_deref()),
        record.target.attached_clients.to_string(),
        record.target.window_count.to_string(),
    ]
    .join("\t")
}

fn parse_discovered_remote_target_record(
    line: &str,
) -> Result<DiscoveredRemoteTargetRecord, TmuxError> {
    let parts = line.split('\t').collect::<Vec<_>>();
    if parts.len() != 12 {
        return Err(TmuxError::new(format!(
            "discovered remote target record version `{}` must contain 12 tab-separated fields, got {}",
            DISCOVERED_TARGET_RECORD_VERSION,
            parts.len()
        )));
    }
    if parts[0] != DISCOVERED_TARGET_RECORD_VERSION {
        return Err(TmuxError::new(format!(
            "unsupported discovered remote target record version `{}`",
            parts[0]
        )));
    }

    let node_id = decode_string_field(parts[1])?;
    let authority_id = decode_string_field(parts[2])?;
    let transport_session_id = decode_string_field(parts[3])?;
    let selector = decode_optional_string_field(parts[4])?;
    let availability = SessionAvailability::parse(parts[5]).ok_or_else(|| {
        TmuxError::new(format!(
            "unsupported discovered remote target availability `{}`",
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
            "invalid discovered remote target attached client count `{}`: {error}",
            parts[10]
        ))
    })?;
    let window_count = parts[11].parse::<usize>().map_err(|error| {
        TmuxError::new(format!(
            "invalid discovered remote target window count `{}`: {error}",
            parts[11]
        ))
    })?;

    Ok(DiscoveredRemoteTargetRecord {
        node_id,
        target: ManagedSessionRecord {
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

fn validate_discovered_remote_target(target: &ManagedSessionRecord) -> Result<(), TmuxError> {
    if target.address.transport() != &crate::domain::session_catalog::SessionTransport::RemotePeer {
        return Err(TmuxError::new(format!(
            "discovered target `{}` is not a remote-peer target",
            target.address.id().as_str()
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
            "discovered target field is not valid UTF-8: {error}"
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
    use super::{DiscoveredRemoteTargetStore, SessionAvailability};
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::path::PathBuf;

    #[test]
    fn discovered_remote_target_store_round_trips_node_records() {
        let path = test_store_path("round-trip");
        let store = DiscoveredRemoteTargetStore::new(&path);
        store
            .upsert_target_from_node("peer-a", &remote_target("peer-a", "shell-1"))
            .expect("target should store");

        let stored = store.list_records().expect("records should load");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].node_id, "peer-a");
        assert_eq!(
            stored[0].target.address.qualified_target(),
            "peer-a:shell-1"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn discovered_remote_target_store_marks_node_targets_offline() {
        let path = test_store_path("offline");
        let store = DiscoveredRemoteTargetStore::new(&path);
        store
            .upsert_target_from_node("peer-a", &remote_target("peer-a", "shell-1"))
            .expect("target should store");

        let changed = store
            .mark_node_targets_offline("peer-a")
            .expect("offline mark should succeed");

        assert!(changed);
        let record = store
            .list_records_for_node("peer-a")
            .expect("records should load")
            .into_iter()
            .next()
            .expect("record should remain present");
        assert_eq!(record.target.availability, SessionAvailability::Offline);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn discovered_remote_target_store_removes_only_matching_node_target() {
        let path = test_store_path("remove");
        let store = DiscoveredRemoteTargetStore::new(&path);
        store
            .upsert_target_from_node("peer-a", &remote_target("peer-a", "shell-1"))
            .expect("first target should store");
        store
            .upsert_target_from_node("peer-b", &remote_target("peer-b", "shell-1"))
            .expect("second target should store");

        let changed = store
            .remove_target_from_node("peer-a", "peer-a", "shell-1")
            .expect("target should remove");

        assert!(changed);
        let remaining = store.list_targets().expect("targets should load");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].address.qualified_target(), "peer-b:shell-1");

        let _ = std::fs::remove_file(path);
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
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
            "waitagent-discovered-remote-target-store-{name}-{}-{}.tsv",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ))
    }
}
