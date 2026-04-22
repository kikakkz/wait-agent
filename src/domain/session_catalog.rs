use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionTransport {
    LocalTmux,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedSessionAddress {
    transport: SessionTransport,
    server_id: String,
    session_id: String,
}

impl ManagedSessionAddress {
    pub fn local_tmux(server_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            transport: SessionTransport::LocalTmux,
            server_id: server_id.into(),
            session_id: session_id.into(),
        }
    }

    pub fn transport(&self) -> &SessionTransport {
        &self.transport
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn qualified_target(&self) -> String {
        format!("{}:{}", self.server_id, self.session_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSessionRecord {
    pub address: ManagedSessionAddress,
    pub workspace_dir: Option<PathBuf>,
    pub workspace_key: Option<String>,
    pub attached_clients: usize,
}

impl ManagedSessionRecord {
    pub fn matches_target(&self, value: &str) -> bool {
        value == self.address.session_id()
            || value == self.address.server_id()
            || value == self.address.qualified_target()
            || self.workspace_key.as_deref() == Some(value)
            || self
                .workspace_dir
                .as_deref()
                .and_then(|path| path.to_str())
                .map_or(false, |path| path == value)
    }

    pub fn summary_line(&self) -> String {
        let workspace = self
            .workspace_dir
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        let key = self.workspace_key.as_deref().unwrap_or("-");

        format!(
            "{} | socket={} | attached={} | workspace={} | key={}",
            self.address.session_id(),
            self.address.server_id(),
            self.attached_clients,
            workspace,
            key
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ManagedSessionAddress, ManagedSessionRecord, SessionTransport};
    use std::path::PathBuf;

    #[test]
    fn managed_session_matches_native_and_workspace_alias_targets() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some("1234".to_string()),
            attached_clients: 1,
        };

        assert_eq!(record.address.transport(), &SessionTransport::LocalTmux);
        assert!(record.matches_target("waitagent-1234"));
        assert!(record.matches_target("wa-1234"));
        assert!(record.matches_target("wa-1234:waitagent-1234"));
        assert!(record.matches_target("1234"));
        assert!(record.matches_target("/tmp/demo"));
        assert!(!record.matches_target("other"));
    }

    #[test]
    fn managed_session_summary_line_includes_native_and_waitagent_metadata() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some("1234".to_string()),
            attached_clients: 2,
        };

        let line = record.summary_line();
        assert!(line.contains("waitagent-1234"));
        assert!(line.contains("socket=wa-1234"));
        assert!(line.contains("attached=2"));
        assert!(line.contains("workspace=/tmp/demo"));
        assert!(line.contains("key=1234"));
    }
}
