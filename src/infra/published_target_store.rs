use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::tmux::TmuxError;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

const PUBLISHED_TARGET_RECORD_VERSION: &str = "v4";
const PUBLISHED_TARGET_RECORD_VERSION_V3: &str = "v3";
const PUBLISHED_TARGET_RECORD_VERSION_V2: &str = "v2";
const PUBLISHED_TARGET_RECORD_VERSION_V1: &str = "v1";
const OPTIONAL_NONE_SENTINEL: &str = "~";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedTargetStore {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedTargetRecord {
    pub source_bindings: BTreeSet<PublishedTargetSourceBinding>,
    pub target: ManagedSessionRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PublishedTargetSourceBinding {
    pub socket_name: String,
    pub session_name: Option<String>,
}

impl Default for PublishedTargetStore {
    fn default() -> Self {
        Self::new(default_published_target_store_path())
    }
}

impl PublishedTargetStore {
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

    pub fn list_records(&self) -> Result<Vec<PublishedTargetRecord>, TmuxError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let contents = fs::read_to_string(&self.path).map_err(|error| {
            TmuxError::new(format!(
                "failed to read published remote target store {}: {error}",
                self.path.display()
            ))
        })?;
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_published_target_record)
            .collect()
    }

    pub fn list_records_for_source_socket(
        &self,
        socket_name: &str,
    ) -> Result<Vec<PublishedTargetRecord>, TmuxError> {
        Ok(self
            .list_records()?
            .into_iter()
            .filter(|record| {
                record
                    .source_bindings
                    .iter()
                    .any(|binding| binding.socket_name == socket_name)
            })
            .collect())
    }

    pub fn list_records_for_source_binding(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<Vec<PublishedTargetRecord>, TmuxError> {
        Ok(self
            .list_records()?
            .into_iter()
            .filter(|record| {
                record
                    .source_bindings
                    .contains(&PublishedTargetSourceBinding {
                        socket_name: socket_name.to_string(),
                        session_name: Some(session_name.to_string()),
                    })
            })
            .collect())
    }

    pub fn upsert_target_from_source(
        &self,
        source_socket_name: &str,
        source_session_name: Option<&str>,
        target: &ManagedSessionRecord,
    ) -> Result<bool, TmuxError> {
        validate_published_remote_target(target)?;
        let mut records = self.list_records()?;
        let record = PublishedTargetRecord {
            source_bindings: [PublishedTargetSourceBinding {
                socket_name: source_socket_name.to_string(),
                session_name: source_session_name.map(str::to_string),
            }]
            .into_iter()
            .collect(),
            target: target.clone(),
        };
        if let Some(existing) = records
            .iter_mut()
            .find(|existing| existing.target.address.id() == target.address.id())
        {
            let mut updated = existing.clone();
            updated.source_bindings.retain(|binding| {
                !(binding.socket_name == source_socket_name && binding.session_name.is_none())
            });
            updated
                .source_bindings
                .insert(PublishedTargetSourceBinding {
                    socket_name: source_socket_name.to_string(),
                    session_name: source_session_name.map(str::to_string),
                });
            updated.target = target.clone();
            if *existing == updated {
                return Ok(false);
            }
            *existing = updated;
        } else {
            records.push(record);
        }
        self.write_records(&records)?;
        Ok(true)
    }

    pub fn remove_target_from_source(
        &self,
        source_socket_name: &str,
        source_session_name: Option<&str>,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<bool, TmuxError> {
        let target_id = ManagedSessionAddress::remote_peer(authority_id, transport_session_id)
            .id()
            .as_str()
            .to_string();
        let mut records = self.list_records()?;
        let mut changed = false;

        for record in &mut records {
            if record.target.address.id().as_str() != target_id {
                continue;
            }
            let prior_len = record.source_bindings.len();
            match source_session_name {
                Some(source_session_name) => {
                    record.source_bindings.retain(|binding| {
                        !(binding.socket_name == source_socket_name
                            && binding.session_name.as_deref() == Some(source_session_name))
                    });
                }
                None => {
                    record
                        .source_bindings
                        .retain(|binding| binding.socket_name != source_socket_name);
                }
            }
            changed = changed || prior_len != record.source_bindings.len();
        }
        records.retain(|record| !record.source_bindings.is_empty());
        if !changed {
            return Ok(false);
        }
        self.write_records(&records)?;
        Ok(true)
    }

    pub fn remove_source_socket_targets_except(
        &self,
        socket_name: &str,
        keep_target_ids: &BTreeSet<String>,
    ) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        let mut records = self.list_records()?;
        let mut removed = Vec::new();
        let mut changed = false;
        for record in &mut records {
            if keep_target_ids.contains(record.target.address.id().as_str()) {
                continue;
            }
            let prior_len = record.source_bindings.len();
            record
                .source_bindings
                .retain(|binding| binding.socket_name != socket_name);
            if prior_len != record.source_bindings.len() {
                changed = true;
                if record.source_bindings.is_empty() {
                    removed.push(record.target.clone());
                }
            }
        }
        records.retain(|record| !record.source_bindings.is_empty());
        if changed {
            self.write_records(&records)?;
        }
        Ok(removed)
    }

    fn write_records(&self, records: &[PublishedTargetRecord]) -> Result<(), TmuxError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                TmuxError::new(format!(
                    "failed to create published remote target directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let contents = if records.is_empty() {
            String::new()
        } else {
            let mut lines = records
                .iter()
                .map(render_published_target_record)
                .collect::<Vec<_>>()
                .join("\n");
            lines.push('\n');
            lines
        };
        fs::write(&self.path, contents).map_err(|error| {
            TmuxError::new(format!(
                "failed to write published remote target store {}: {error}",
                self.path.display()
            ))
        })
    }
}

fn default_published_target_store_path() -> PathBuf {
    std::env::temp_dir().join("waitagent-published-remote-targets.tsv")
}

fn render_published_target_record(record: &PublishedTargetRecord) -> String {
    let current_path = record
        .target
        .current_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    [
        PUBLISHED_TARGET_RECORD_VERSION.to_string(),
        encode_source_binding_list_field(&record.source_bindings),
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

fn parse_published_target_record(line: &str) -> Result<PublishedTargetRecord, TmuxError> {
    let parts = line.split('\t').collect::<Vec<_>>();
    let (source_bindings, index_offset) = match parts.first().copied() {
        Some(PUBLISHED_TARGET_RECORD_VERSION) => {
            if parts.len() != 12 {
                return Err(TmuxError::new(format!(
                    "published remote target record version `{}` must contain 12 tab-separated fields, got {}",
                    PUBLISHED_TARGET_RECORD_VERSION,
                    parts.len()
                )));
            }
            (decode_source_binding_list_field(parts[1])?, 1)
        }
        Some(PUBLISHED_TARGET_RECORD_VERSION_V3) => {
            if parts.len() != 12 {
                return Err(TmuxError::new(format!(
                    "published remote target record version `{}` must contain 12 tab-separated fields, got {}",
                    PUBLISHED_TARGET_RECORD_VERSION_V3,
                    parts.len()
                )));
            }
            (
                decode_string_list_field(parts[1])?
                    .into_iter()
                    .map(PublishedTargetSourceBinding::legacy_socket)
                    .collect::<BTreeSet<_>>(),
                1,
            )
        }
        Some(PUBLISHED_TARGET_RECORD_VERSION_V2) => {
            if parts.len() != 12 {
                return Err(TmuxError::new(format!(
                    "published remote target record version `{}` must contain 12 tab-separated fields, got {}",
                    PUBLISHED_TARGET_RECORD_VERSION_V2,
                    parts.len()
                )));
            }
            (
                decode_optional_string_field(parts[1])?
                    .into_iter()
                    .map(PublishedTargetSourceBinding::legacy_socket)
                    .collect::<BTreeSet<_>>(),
                1,
            )
        }
        Some(PUBLISHED_TARGET_RECORD_VERSION_V1) => {
            if parts.len() != 11 {
                return Err(TmuxError::new(format!(
                    "published remote target record version `{}` must contain 11 tab-separated fields, got {}",
                    PUBLISHED_TARGET_RECORD_VERSION_V1,
                    parts.len()
                )));
            }
            (BTreeSet::new(), 0)
        }
        Some(other) => {
            return Err(TmuxError::new(format!(
                "unsupported published remote target record version `{other}`"
            )));
        }
        None => return Err(TmuxError::new("published remote target record is empty")),
    };

    let authority_id = decode_string_field(parts[1 + index_offset])?;
    let transport_session_id = decode_string_field(parts[2 + index_offset])?;
    let selector = decode_optional_string_field(parts[3 + index_offset])?;
    let availability = SessionAvailability::parse(parts[4 + index_offset]).ok_or_else(|| {
        TmuxError::new(format!(
            "unsupported published remote target availability `{}`",
            parts[4 + index_offset]
        ))
    })?;
    let session_role = decode_optional_string_field(parts[5 + index_offset])?
        .as_deref()
        .and_then(WorkspaceSessionRole::parse);
    let workspace_key = decode_optional_string_field(parts[6 + index_offset])?;
    let command_name = decode_optional_string_field(parts[7 + index_offset])?;
    let current_path = decode_optional_string_field(parts[8 + index_offset])?.map(PathBuf::from);
    let attached_clients = parts[9 + index_offset].parse::<usize>().map_err(|error| {
        TmuxError::new(format!(
            "invalid published remote target attached client count `{}`: {error}",
            parts[9 + index_offset]
        ))
    })?;
    let window_count = parts[10 + index_offset].parse::<usize>().map_err(|error| {
        TmuxError::new(format!(
            "invalid published remote target window count `{}`: {error}",
            parts[10 + index_offset]
        ))
    })?;

    Ok(PublishedTargetRecord {
        source_bindings,
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

fn validate_published_remote_target(target: &ManagedSessionRecord) -> Result<(), TmuxError> {
    if target.address.transport() != &crate::domain::session_catalog::SessionTransport::RemotePeer {
        return Err(TmuxError::new(format!(
            "published target `{}` is not a remote-peer target",
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
            "published target field is not valid UTF-8: {error}"
        ))
    })
}

fn encode_optional_string_field(value: Option<&str>) -> String {
    value
        .map(encode_string_field)
        .unwrap_or_else(|| OPTIONAL_NONE_SENTINEL.to_string())
}

fn encode_string_list_field(values: &BTreeSet<String>) -> String {
    if values.is_empty() {
        return OPTIONAL_NONE_SENTINEL.to_string();
    }
    values
        .iter()
        .map(|value| encode_string_field(value))
        .collect::<Vec<_>>()
        .join(",")
}

fn encode_source_binding_list_field(values: &BTreeSet<PublishedTargetSourceBinding>) -> String {
    if values.is_empty() {
        return OPTIONAL_NONE_SENTINEL.to_string();
    }
    values
        .iter()
        .map(encode_source_binding_field)
        .collect::<Vec<_>>()
        .join(",")
}

fn decode_optional_string_field(value: &str) -> Result<Option<String>, TmuxError> {
    if value == OPTIONAL_NONE_SENTINEL {
        return Ok(None);
    }
    decode_string_field(value).map(Some)
}

fn decode_string_list_field(value: &str) -> Result<BTreeSet<String>, TmuxError> {
    if value == OPTIONAL_NONE_SENTINEL {
        return Ok(BTreeSet::new());
    }
    value
        .split(',')
        .map(decode_string_field)
        .collect::<Result<BTreeSet<_>, _>>()
}

fn encode_source_binding_field(value: &PublishedTargetSourceBinding) -> String {
    format!(
        "{}:{}",
        encode_string_field(&value.socket_name),
        encode_optional_string_field(value.session_name.as_deref())
    )
}

fn decode_source_binding_field(value: &str) -> Result<PublishedTargetSourceBinding, TmuxError> {
    let Some((socket_name, session_name)) = value.split_once(':') else {
        return Err(TmuxError::new(format!(
            "invalid published source binding field `{value}`"
        )));
    };
    Ok(PublishedTargetSourceBinding {
        socket_name: decode_string_field(socket_name)?,
        session_name: decode_optional_string_field(session_name)?,
    })
}

fn decode_source_binding_list_field(
    value: &str,
) -> Result<BTreeSet<PublishedTargetSourceBinding>, TmuxError> {
    if value == OPTIONAL_NONE_SENTINEL {
        return Ok(BTreeSet::new());
    }
    value
        .split(',')
        .map(decode_source_binding_field)
        .collect::<Result<BTreeSet<_>, _>>()
}

impl PublishedTargetSourceBinding {
    fn legacy_socket(socket_name: String) -> Self {
        Self {
            socket_name,
            session_name: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_published_target_record, render_published_target_record, PublishedTargetRecord,
        PublishedTargetSourceBinding, PublishedTargetStore,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn published_target_store_round_trips_remote_targets() {
        let store = PublishedTargetStore::new(test_store_path("round-trip"));
        let target = remote_target("peer-a", "shell-1", Some("wa-local:shell-1"));

        store
            .upsert_target_from_source("wa-local", Some("target-1"), &target)
            .expect("upsert should succeed");
        let stored = store.list_targets().expect("list should succeed");

        assert_eq!(stored, vec![target]);
    }

    #[test]
    fn published_target_store_upsert_replaces_existing_target() {
        let store = PublishedTargetStore::new(test_store_path("replace"));
        store
            .upsert_target_from_source(
                "wa-local",
                Some("target-1"),
                &remote_target("peer-a", "shell-1", Some("wa-local:shell-1")),
            )
            .expect("first upsert should succeed");
        store
            .upsert_target_from_source(
                "wa-local",
                Some("target-1"),
                &remote_target("peer-a", "shell-1", Some("wa-local:shell-2")),
            )
            .expect("second upsert should replace existing target");

        let stored = store.list_targets().expect("list should succeed");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].selector.as_deref(), Some("wa-local:shell-2"));
    }

    #[test]
    fn published_target_store_remove_target_from_source_deletes_matching_record() {
        let store = PublishedTargetStore::new(test_store_path("remove"));
        store
            .upsert_target_from_source(
                "wa-local",
                Some("target-1"),
                &remote_target("peer-a", "shell-1", Some("wa-local:shell-1")),
            )
            .expect("upsert should succeed");

        assert!(store
            .remove_target_from_source("wa-local", Some("target-1"), "peer-a", "shell-1")
            .expect("remove should succeed"));
        assert!(store
            .list_targets()
            .expect("list should succeed")
            .is_empty());
    }

    #[test]
    fn published_target_record_format_round_trips_optional_fields() {
        let record = remote_target("peer-a", "shell-1", None);
        let rendered = render_published_target_record(&PublishedTargetRecord {
            source_bindings: [source_binding("wa-local", Some("target-1"))]
                .into_iter()
                .collect(),
            target: record.clone(),
        });
        let parsed =
            parse_published_target_record(&rendered).expect("rendered record should parse");

        assert_eq!(
            parsed,
            PublishedTargetRecord {
                source_bindings: [source_binding("wa-local", Some("target-1"))]
                    .into_iter()
                    .collect(),
                target: record,
            }
        );
    }

    #[test]
    fn remove_source_socket_targets_except_only_prunes_matching_socket_scope() {
        let store = PublishedTargetStore::new(test_store_path("prune"));
        store
            .upsert_target_from_source(
                "wa-local",
                Some("target-1"),
                &remote_target("peer-a", "shell-1", None),
            )
            .expect("first upsert should succeed");
        store
            .upsert_target_from_source(
                "wa-other",
                Some("target-2"),
                &remote_target("peer-b", "shell-2", None),
            )
            .expect("second upsert should succeed");

        let removed = store
            .remove_source_socket_targets_except("wa-local", &BTreeSet::new())
            .expect("prune should succeed");

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].address.authority_id(), "peer-a");
        let remaining = store.list_targets().expect("list should succeed");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].address.authority_id(), "peer-b");
    }

    #[test]
    fn published_target_store_upsert_reports_no_change_for_identical_record() {
        let store = PublishedTargetStore::new(test_store_path("no-op-upsert"));
        let target = remote_target("peer-a", "shell-1", Some("wa-local:shell-1"));

        assert!(store
            .upsert_target_from_source("wa-local", Some("target-1"), &target)
            .expect("first upsert should report a change"));
        assert!(!store
            .upsert_target_from_source("wa-local", Some("target-1"), &target)
            .expect("identical upsert should report no change"));
    }

    #[test]
    fn published_target_store_keeps_target_while_other_sources_still_publish_it() {
        let store = PublishedTargetStore::new(test_store_path("multi-source"));
        let target = remote_target("peer-a", "shell-1", Some("wa-local:shell-1"));

        store
            .upsert_target_from_source("wa-local", Some("target-1"), &target)
            .expect("first upsert should succeed");
        store
            .upsert_target_from_source("wa-other", Some("target-2"), &target)
            .expect("second upsert should succeed");

        assert!(store
            .remove_target_from_source("wa-local", Some("target-1"), "peer-a", "shell-1")
            .expect("source-scoped remove should succeed"));
        let stored = store.list_records().expect("records should list");
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].source_bindings,
            [source_binding("wa-other", Some("target-2"))]
                .into_iter()
                .collect()
        );
        assert_eq!(
            store.list_targets().expect("targets should list"),
            vec![target]
        );
    }

    fn remote_target(
        authority_id: &str,
        session_id: &str,
        selector: Option<&str>,
    ) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: selector.map(str::to_string),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Unknown,
        }
    }

    fn test_store_path(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-published-target-store-{name}-{}-{millis}.tsv",
            process::id()
        ))
    }

    fn source_binding(
        socket_name: &str,
        session_name: Option<&str>,
    ) -> PublishedTargetSourceBinding {
        PublishedTargetSourceBinding {
            socket_name: socket_name.to_string(),
            session_name: session_name.map(str::to_string),
        }
    }

    #[test]
    fn remove_target_from_source_ignores_missing_path() {
        let store = PublishedTargetStore::new(test_store_path("missing"));
        assert!(!store
            .remove_target_from_source("wa-local", Some("target-1"), "peer-a", "shell-1")
            .expect("removing absent target should succeed"));
        assert_eq!(
            store.list_targets().expect("list should succeed"),
            Vec::new()
        );
    }

    #[test]
    fn store_default_path_is_stable_temp_file() {
        let store = PublishedTargetStore::default();
        assert!(store
            .path
            .ends_with("waitagent-published-remote-targets.tsv"));
    }
}
