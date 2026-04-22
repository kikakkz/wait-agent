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

    pub fn display_location(&self) -> &str {
        match self.transport {
            SessionTransport::LocalTmux => "local",
        }
    }

    pub fn display_session_id(&self) -> &str {
        self.session_id
            .strip_prefix("waitagent-")
            .unwrap_or(self.session_id.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedSessionTaskState {
    Running,
    Input,
    Confirm,
    Unknown,
}

impl ManagedSessionTaskState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Input => "INPUT",
            Self::Confirm => "CONFIRM",
            Self::Unknown => "UNKNOWN",
        }
    }

    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Running => "R",
            Self::Input => "I",
            Self::Confirm => "C",
            Self::Unknown => "U",
        }
    }

    pub fn infer(command_name: Option<&str>, pane_text: &str) -> Self {
        let normalized_lines = pane_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if normalized_lines.is_empty() {
            return Self::Unknown;
        }

        if normalized_lines
            .iter()
            .map(|line| line.to_ascii_lowercase())
            .any(|line| {
                line.contains("approve")
                    || line.contains("approval")
                    || line.contains("confirm")
                    || line.contains("continue?")
                    || line.contains("allow")
                    || line.contains("permission")
                    || line.contains("[y/n]")
                    || line.contains("(y/n)")
                    || line.contains("yes/no")
            })
        {
            return Self::Confirm;
        }

        let command_name = command_name.unwrap_or_default();
        let last_line = normalized_lines.last().copied().unwrap_or_default();
        if looks_like_shell_prompt(command_name, last_line)
            || looks_like_agent_input(command_name, last_line)
        {
            return Self::Input;
        }

        Self::Running
    }
}

impl Default for ManagedSessionTaskState {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSessionRecord {
    pub address: ManagedSessionAddress,
    pub workspace_dir: Option<PathBuf>,
    pub workspace_key: Option<String>,
    pub attached_clients: usize,
    pub window_count: usize,
    pub command_name: Option<String>,
    pub current_path: Option<PathBuf>,
    pub task_state: ManagedSessionTaskState,
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

    pub fn display_label(&self) -> String {
        format!(
            "{}@{}",
            self.command_name.as_deref().unwrap_or("bash"),
            self.address.display_location()
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

fn looks_like_shell_prompt(command_name: &str, line: &str) -> bool {
    matches!(command_name, "" | "bash" | "zsh" | "fish" | "sh")
        && matches!(line.chars().last(), Some('$' | '#' | '%'))
}

fn looks_like_agent_input(command_name: &str, line: &str) -> bool {
    matches!(command_name, "codex" | "claude")
        && (line.starts_with('›')
            || line.starts_with("> ")
            || line.contains("type your message")
            || line.contains("send a message"))
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionTransport,
    };
    use std::path::PathBuf;

    #[test]
    fn managed_session_matches_native_tmux_targets() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some("1234".to_string()),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
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
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Running,
        };

        let line = record.summary_line();
        assert_eq!(line, "1234: 3 windows (attached)");
    }

    #[test]
    fn managed_session_display_label_uses_transport_aware_location() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
            workspace_dir: None,
            workspace_key: None,
            attached_clients: 0,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        };

        assert_eq!(record.display_label(), "codex@local");
    }

    #[test]
    fn task_state_infers_confirm_from_visible_prompt_text() {
        let state = ManagedSessionTaskState::infer(
            Some("codex"),
            "Allow this action?\nType yes/no to continue",
        );

        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_input_from_codex_prompt_line() {
        let state = ManagedSessionTaskState::infer(Some("codex"), "Tip\n› ");

        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_input_from_shell_prompt_line() {
        let state = ManagedSessionTaskState::infer(Some("bash"), "k@host:/tmp$");

        assert_eq!(state, ManagedSessionTaskState::Input);
    }
}
