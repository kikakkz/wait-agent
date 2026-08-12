use crate::host::ssh::remote_host_home::waitagent_home;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

/// A lightweight reference to an active outbound-dial remote connection.
///
/// The full connection details (host, port, TLS pin, credentials) live in the
/// saved `RemoteHostProfile`; this entry only keeps enough information to find
/// the profile again after a control-plane restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundConnectionSnapshotEntry {
    pub profile_name: String,
    pub authority_node_id: String,
    pub connected_at: String,
}

#[derive(Debug, Clone)]
pub struct OutboundConnectionSnapshotStore {
    path: PathBuf,
}

impl OutboundConnectionSnapshotStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> PathBuf {
        waitagent_home().join("outbound-connections.toml")
    }

    pub fn load(
        &self,
    ) -> Result<Vec<OutboundConnectionSnapshotEntry>, OutboundConnectionSnapshotStoreError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text =
            fs::read_to_string(&self.path).map_err(OutboundConnectionSnapshotStoreError::io)?;
        parse_snapshots(&text)
    }

    pub fn save(
        &self,
        entries: &[OutboundConnectionSnapshotEntry],
    ) -> Result<(), OutboundConnectionSnapshotStoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(OutboundConnectionSnapshotStoreError::io)?;
        }
        fs::write(&self.path, serialize_snapshots(entries))
            .map_err(OutboundConnectionSnapshotStoreError::io)
    }

    pub fn upsert(
        &self,
        profile_name: impl Into<String>,
        authority_node_id: impl Into<String>,
    ) -> Result<(), OutboundConnectionSnapshotStoreError> {
        let mut entries = self.load()?;
        let profile_name = profile_name.into();
        let authority_node_id = authority_node_id.into();
        let connected_at = current_timestamp();
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.authority_node_id == authority_node_id)
        {
            existing.profile_name = profile_name;
            existing.connected_at = connected_at;
        } else {
            entries.push(OutboundConnectionSnapshotEntry {
                profile_name,
                authority_node_id,
                connected_at,
            });
        }
        self.save(&entries)
    }

    pub fn remove(
        &self,
        authority_node_id: &str,
    ) -> Result<Option<OutboundConnectionSnapshotEntry>, OutboundConnectionSnapshotStoreError> {
        let mut entries = self.load()?;
        let Some(index) = entries
            .iter()
            .position(|entry| entry.authority_node_id == authority_node_id)
        else {
            return Ok(None);
        };
        let removed = entries.remove(index);
        self.save(&entries)?;
        Ok(Some(removed))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundConnectionSnapshotStoreError {
    message: String,
}

impl OutboundConnectionSnapshotStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for OutboundConnectionSnapshotStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for OutboundConnectionSnapshotStoreError {}

fn serialize_snapshots(entries: &[OutboundConnectionSnapshotEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str("[[connections]]\n");
        out.push_str(&format!(
            "profile_name = \"{}\"\n",
            escape(&entry.profile_name)
        ));
        out.push_str(&format!(
            "authority_node_id = \"{}\"\n",
            escape(&entry.authority_node_id)
        ));
        out.push_str(&format!(
            "connected_at = \"{}\"\n",
            escape(&entry.connected_at)
        ));
        out.push('\n');
    }
    out
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn parse_snapshots(
    text: &str,
) -> Result<Vec<OutboundConnectionSnapshotEntry>, OutboundConnectionSnapshotStoreError> {
    let mut entries = Vec::new();
    let mut current = RawSnapshot::default();
    let mut in_connection = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[connections]]" {
            if in_connection {
                entries.push(current.into_entry()?);
                current = RawSnapshot::default();
            }
            in_connection = true;
            continue;
        }
        if !in_connection {
            return Err(OutboundConnectionSnapshotStoreError::new(
                "outbound connection snapshot field appears before [[connections]]",
            ));
        }
        let (key, value) = parse_key_value(line)?;
        current.set(&key, value)?;
    }

    if in_connection {
        entries.push(current.into_entry()?);
    }

    Ok(entries)
}

#[derive(Default)]
struct RawSnapshot {
    profile_name: Option<String>,
    authority_node_id: Option<String>,
    connected_at: Option<String>,
}

impl RawSnapshot {
    fn set(
        &mut self,
        key: &str,
        value: String,
    ) -> Result<(), OutboundConnectionSnapshotStoreError> {
        match key {
            "profile_name" => self.profile_name = Some(value),
            "authority_node_id" => self.authority_node_id = Some(value),
            "connected_at" => self.connected_at = Some(value),
            other => {
                return Err(OutboundConnectionSnapshotStoreError::new(format!(
                    "unknown outbound connection snapshot field `{other}`"
                )));
            }
        }
        Ok(())
    }

    fn into_entry(
        self,
    ) -> Result<OutboundConnectionSnapshotEntry, OutboundConnectionSnapshotStoreError> {
        Ok(OutboundConnectionSnapshotEntry {
            profile_name: required(self.profile_name, "profile_name")?,
            authority_node_id: required(self.authority_node_id, "authority_node_id")?,
            connected_at: self.connected_at.unwrap_or_else(current_timestamp),
        })
    }
}

fn parse_key_value(line: &str) -> Result<(String, String), OutboundConnectionSnapshotStoreError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(OutboundConnectionSnapshotStoreError::new(format!(
            "invalid outbound connection snapshot line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        return Ok((key, parse_quoted(value)?));
    }
    Ok((key, value.to_string()))
}

fn parse_quoted(value: &str) -> Result<String, OutboundConnectionSnapshotStoreError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(OutboundConnectionSnapshotStoreError::new(
            "unterminated outbound connection snapshot string",
        ));
    }
    let mut out = String::new();
    let mut chars = value[1..value.len() - 1].chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    Ok(out)
}

fn required(
    value: Option<String>,
    field: &str,
) -> Result<String, OutboundConnectionSnapshotStoreError> {
    match value.filter(|value| !value.trim().is_empty()) {
        Some(value) => Ok(value),
        None => Err(OutboundConnectionSnapshotStoreError::new(format!(
            "outbound connection snapshot `{field}` is required"
        ))),
    }
}

fn current_timestamp() -> String {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let nanos = duration.subsec_nanos();
    // RFC 3339 formatted UTC timestamp (no leap-second handling).
    let days_since_epoch = (secs / 86_400) as i64;
    let seconds_in_day = (secs % 86_400) as i64;
    let (year, month, day) = days_to_ymd(days_since_epoch);
    let (hour, minute, second) = (
        (seconds_in_day / 3600) as u32,
        ((seconds_in_day % 3600) / 60) as u32,
        (seconds_in_day % 60) as u32,
    );
    let millis = nanos / 1_000_000;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

fn days_to_ymd(mut days: i64) -> (i32, u32, u32) {
    // Approximate conversion good for years 1970..2100.
    let mut year = 1970i32;
    loop {
        let year_days = if is_leap_year(year) { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }
    let month_lengths = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    for &length in &month_lengths {
        if days < length as i64 {
            break;
        }
        days -= length as i64;
        month += 1;
    }
    (year, month, (days + 1) as u32)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    #[test]
    fn snapshot_default_path_uses_waitagent_home() {
        let path = OutboundConnectionSnapshotStore::default_path();
        assert!(path.ends_with(PathBuf::from(".waitagent/outbound-connections.toml")));
    }

    #[test]
    fn snapshot_persists_and_loads_entries() {
        let path = unique_path("outbound-snapshot.toml");
        let store = OutboundConnectionSnapshotStore::new(&path);

        store.upsert("kk@10.1.29.130", "10.1.29.130#7476").unwrap();
        store.upsert("cloud@1.2.3.4", "1.2.3.4#7477").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].profile_name, "kk@10.1.29.130");
        assert_eq!(loaded[0].authority_node_id, "10.1.29.130#7476");
        assert_eq!(loaded[1].authority_node_id, "1.2.3.4#7477");

        crate::infra::best_effort::remove_file(path);
    }

    #[test]
    fn snapshot_upsert_updates_existing_authority() {
        let path = unique_path("outbound-snapshot-upsert.toml");
        let store = OutboundConnectionSnapshotStore::new(&path);

        store.upsert("old-name", "10.1.29.130#7476").unwrap();
        store.upsert("new-name", "10.1.29.130#7476").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].profile_name, "new-name");

        crate::infra::best_effort::remove_file(path);
    }

    #[test]
    fn snapshot_remove_missing_returns_none() {
        let path = unique_path("outbound-snapshot-remove.toml");
        let store = OutboundConnectionSnapshotStore::new(&path);

        store.upsert("a", "10.1.29.130#7476").unwrap();
        let removed = store.remove("missing").unwrap();

        assert!(removed.is_none());
        assert_eq!(store.load().unwrap().len(), 1);

        crate::infra::best_effort::remove_file(path);
    }
}
