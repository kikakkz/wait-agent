//! Per-server last-negotiated geometry store for managed nodes.
//!
//! Implements the persistence contract from
//! `docs/remote-geometry-coordination-design.md` section 8: a managed node
//! remembers the last negotiated target-pane geometry per server, so
//! headless session creation and mirror open can start from a sane size
//! instead of the tmux 80x24 detached default.
//!
//! Stored values are initial values only; runtime coordination always
//! re-negotiates.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGeometry {
    pub cols: u32,
    pub rows: u32,
    pub node_id: String,
    pub updated_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PerServerGeometryStore {
    entries: BTreeMap<String, StoredGeometry>,
}

impl PerServerGeometryStore {
    /// Load the store, tolerating a missing or corrupt file.
    pub fn load(path: &Path) -> Self {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    pub fn lookup(&self, server_id: &str) -> Option<&StoredGeometry> {
        self.entries.get(server_id)
    }

    pub fn record(&mut self, server_id: &str, node_id: String, cols: u32, rows: u32, now: u64) {
        self.entries.insert(
            server_id.to_string(),
            StoredGeometry {
                cols,
                rows,
                node_id,
                updated_at: now,
            },
        );
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Persist atomically (write-then-rename), creating the parent dir.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create store dir {}: {error}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let content = serde_json::to_string_pretty(self)
            .map_err(|error| format!("encode geometry store: {error}"))?;
        std::fs::write(&tmp, content)
            .map_err(|error| format!("write geometry store {}: {error}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|error| format!("rename geometry store to {}: {error}", path.display()))?;
        Ok(())
    }
}

/// Default store location: alongside the vendored tmux binary in the
/// per-user waitagent data dir.
pub fn default_store_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/share/waitagent/per-server-geometry.json")
}

/// Current unix time in seconds.
pub fn store_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "waitagent-geometry-store-{label}-{}-{nanos:x}.json",
            std::process::id()
        ))
    }

    #[test]
    fn record_and_lookup_are_independent_per_server() {
        let mut store = PerServerGeometryStore::default();
        store.record("server-a:7474", "node-b#7474".to_string(), 167, 47, 100);
        store.record("server-c:7474", "node-b#7474".to_string(), 47, 22, 200);

        let a = store.lookup("server-a:7474").expect("server-a entry");
        assert_eq!((a.cols, a.rows, a.updated_at), (167, 47, 100));
        let c = store.lookup("server-c:7474").expect("server-c entry");
        assert_eq!((c.cols, c.rows, c.updated_at), (47, 22, 200));
        assert_eq!(c.node_id, "node-b#7474");
        assert!(store.lookup("server-unknown").is_none());
    }

    #[test]
    fn save_and_load_round_trips() {
        let path = temp_path("roundtrip");
        let mut store = PerServerGeometryStore::default();
        store.record("server-a:7474", "node-b#7474".to_string(), 167, 47, 100);
        store.record("server-c:7474", "node-b#7474".to_string(), 47, 22, 200);
        store.save(&path).expect("store should save");

        let loaded = PerServerGeometryStore::load(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded.lookup("server-a:7474").map(|g| (g.cols, g.rows)),
            Some((167, 47))
        );
        assert_eq!(
            loaded.lookup("server-c:7474").map(|g| (g.cols, g.rows)),
            Some((47, 22))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_tolerates_missing_and_corrupt_files() {
        let missing = temp_path("missing");
        assert_eq!(PerServerGeometryStore::load(&missing).len(), 0);

        let corrupt = temp_path("corrupt");
        std::fs::write(&corrupt, b"not json").expect("corrupt file should write");
        assert_eq!(PerServerGeometryStore::load(&corrupt).len(), 0);
        let _ = std::fs::remove_file(&corrupt);
    }
}
