use crate::domain::workspace::WorkspaceSessionRole;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionTransport {
    LocalTmux,
    RemotePeer,
}

impl SessionTransport {
    fn stable_prefix(&self) -> &'static str {
        match self {
            Self::LocalTmux => "local-tmux",
            Self::RemotePeer => "remote-peer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetId(String);

impl TargetId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn for_transport(transport: &SessionTransport, authority_id: &str, session_id: &str) -> Self {
        Self(format!(
            "{}:{}:{}",
            transport.stable_prefix(),
            authority_id,
            session_id
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedSessionAddress {
    id: TargetId,
    transport: SessionTransport,
    authority_id: String,
    session_id: String,
}

impl ManagedSessionAddress {
    pub fn local_tmux(server_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        let authority_id = server_id.into();
        let session_id = session_id.into();
        let transport = SessionTransport::LocalTmux;
        Self {
            id: TargetId::for_transport(&transport, &authority_id, &session_id),
            transport,
            authority_id,
            session_id,
        }
    }

    pub fn remote_peer(authority_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        let authority_id = authority_id.into();
        let session_id = session_id.into();
        let transport = SessionTransport::RemotePeer;
        Self {
            id: TargetId::for_transport(&transport, &authority_id, &session_id),
            transport,
            authority_id,
            session_id,
        }
    }

    pub fn transport(&self) -> &SessionTransport {
        &self.transport
    }

    pub fn id(&self) -> &TargetId {
        &self.id
    }

    pub fn authority_id(&self) -> &str {
        &self.authority_id
    }

    pub fn server_id(&self) -> &str {
        self.authority_id()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn qualified_target(&self) -> String {
        format!("{}:{}", self.authority_id, self.session_id)
    }

    pub fn display_location(&self) -> &str {
        match self.transport {
            SessionTransport::LocalTmux => "local",
            SessionTransport::RemotePeer => "remote",
        }
    }

    pub fn display_authority_id(&self) -> &str {
        self.authority_id
            .split_once('#')
            .map(|(host, _)| host)
            .unwrap_or(self.authority_id())
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
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Input => "input",
            Self::Confirm => "confirm",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "input" => Some(Self::Input),
            "confirm" => Some(Self::Confirm),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

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

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionAvailability {
    Online,
    Offline,
    Exited,
    #[default]
    Unknown,
}

impl SessionAvailability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
            Self::Exited => "exited",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "online" => Some(Self::Online),
            "offline" => Some(Self::Offline),
            "exited" => Some(Self::Exited),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLocation {
    LocalWorkspace,
    ServerConsole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleAttachment {
    pub console_id: String,
    pub location: ConsoleLocation,
    pub has_pty_resize_authority: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSessionRecord {
    pub address: ManagedSessionAddress,
    pub selector: Option<String>,
    pub availability: SessionAvailability,
    pub workspace_dir: Option<PathBuf>,
    pub workspace_key: Option<String>,
    pub session_role: Option<WorkspaceSessionRole>,
    pub opened_by: Vec<ConsoleAttachment>,
    pub attached_clients: usize,
    pub window_count: usize,
    pub command_name: Option<String>,
    pub current_path: Option<PathBuf>,
    pub task_state: ManagedSessionTaskState,
}

impl ManagedSessionRecord {
    pub fn is_workspace_chrome(&self) -> bool {
        self.session_role == Some(WorkspaceSessionRole::WorkspaceChrome)
    }

    pub fn is_target_host(&self) -> bool {
        self.session_role == Some(WorkspaceSessionRole::TargetHost)
    }

    pub fn is_workspace_session(&self) -> bool {
        matches!(
            self.session_role,
            Some(WorkspaceSessionRole::WorkspaceChrome | WorkspaceSessionRole::TargetHost)
        )
    }

    pub fn matches_target(&self, value: &str) -> bool {
        value == self.address.id().as_str()
            || self.selector.as_deref() == Some(value)
            || value == self.address.display_session_id()
            || value == self.address.session_id()
            || value == self.address.authority_id()
            || value == self.address.display_authority_id()
            || value == self.address.server_id()
            || value == self.address.qualified_target()
            || value
                == format!(
                    "{}:{}",
                    self.address.display_authority_id(),
                    self.address.session_id()
                )
            || value
                == format!(
                    "{}:{}",
                    self.address.display_authority_id(),
                    self.address.display_session_id()
                )
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
            self.display_scope()
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

    fn display_scope(&self) -> String {
        match self.address.transport() {
            SessionTransport::LocalTmux => "local".to_string(),
            SessionTransport::RemotePeer => format!(
                "{}:{}",
                self.address.display_authority_id(),
                self.address.display_session_id()
            ),
        }
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
        ConsoleAttachment, ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord,
        ManagedSessionTaskState, SessionAvailability, SessionTransport,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::path::PathBuf;

    #[test]
    fn managed_session_matches_native_tmux_targets() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
            selector: Some("wa-1234:waitagent-1234".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some("1234".to_string()),
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
            opened_by: vec![ConsoleAttachment {
                console_id: "workspace-main".to_string(),
                location: ConsoleLocation::LocalWorkspace,
                has_pty_resize_authority: true,
            }],
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        };

        assert_eq!(record.address.transport(), &SessionTransport::LocalTmux);
        assert_eq!(
            record.address.id().as_str(),
            "local-tmux:wa-1234:waitagent-1234"
        );
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
            selector: Some("wa-1234:waitagent-1234".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some("1234".to_string()),
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
            opened_by: Vec::new(),
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
            selector: Some("wa-1234:waitagent-1234".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        };

        assert_eq!(record.display_label(), "codex@local");
    }

    #[test]
    fn remote_display_label_hides_internal_node_port_suffix() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("10.1.29.165#7721", "pty1"),
            selector: Some("10.1.29.165#7721:pty1".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        };

        assert_eq!(record.display_label(), "codex@10.1.29.165:pty1");
        assert!(record.matches_target("10.1.29.165:pty1"));
        assert!(record.matches_target("10.1.29.165#7721:pty1"));
    }

    #[test]
    fn remote_session_display_label_includes_authority_and_session() {
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("10.1.29.165", "pty1"),
            selector: Some("10.1.29.165:pty1".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        };

        assert_eq!(record.display_label(), "codex@10.1.29.165:pty1");
    }

    #[test]
    fn managed_session_exposes_workspace_role_helpers() {
        let chrome = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
            selector: Some("wa-1234:waitagent-1234".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Unknown,
        };
        let target = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-5678"),
            selector: Some("wa-1234:waitagent-5678".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Unknown,
        };

        assert!(chrome.is_workspace_chrome());
        assert!(!chrome.is_target_host());
        assert!(target.is_target_host());
        assert!(!target.is_workspace_chrome());
    }

    #[test]
    fn managed_session_remote_addresses_keep_transport_and_authority_explicit() {
        let address = ManagedSessionAddress::remote_peer("peer-a", "shell-7");

        assert_eq!(address.transport(), &SessionTransport::RemotePeer);
        assert_eq!(address.id().as_str(), "remote-peer:peer-a:shell-7");
        assert_eq!(address.authority_id(), "peer-a");
        assert_eq!(address.server_id(), "peer-a");
        assert_eq!(address.qualified_target(), "peer-a:shell-7");
        assert_eq!(address.display_location(), "remote");
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
