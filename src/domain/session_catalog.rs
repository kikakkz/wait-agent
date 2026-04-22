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

    pub fn display_session_id(&self) -> &str {
        self.session_id
            .strip_prefix("waitagent-")
            .unwrap_or(self.session_id.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSessionRecord {
    pub address: ManagedSessionAddress,
    pub workspace_dir: Option<PathBuf>,
    pub workspace_key: Option<String>,
    pub attached_clients: usize,
    pub window_count: usize,
}

impl ManagedSessionRecord {
    pub fn matches_target(&self, value: &str) -> bool {
        value == self.address.display_session_id()
            || value == self.address.session_id()
            || value == self.address.server_id()
            || value == self.address.qualified_target()
            || value
                == format!(
                    "{}:{}",
                    self.address.server_id(),
                    self.address.display_session_id()
                )
    }

    pub fn summary_line(&self) -> String {
        format!(
            "{}: {} windows ({})",
            self.address.display_session_id(),
            self.window_count,
            if self.attached_clients > 0 {
                "attached"
            } else {
                "detached"
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ManagedSessionAddress, ManagedSessionRecord, SessionTransport};
    use std::path::PathBuf;

    #[test]
    fn managed_session_matches_native_tmux_targets() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some("1234".to_string()),
            attached_clients: 1,
            window_count: 1,
        };

        assert_eq!(record.address.transport(), &SessionTransport::LocalTmux);
        assert!(record.matches_target("1234"));
        assert!(record.matches_target("waitagent-1234"));
        assert!(record.matches_target("wa-1234"));
        assert!(record.matches_target("wa-1234:waitagent-1234"));
        assert!(record.matches_target("wa-1234:1234"));
        assert!(!record.matches_target("other"));
    }

    #[test]
    fn managed_session_summary_line_matches_tmux_like_shape() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some("1234".to_string()),
            attached_clients: 2,
            window_count: 3,
        };

        let line = record.summary_line();
        assert_eq!(line, "1234: 3 windows (attached)");
    }
}
