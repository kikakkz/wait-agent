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

    #[cfg(test)]
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
}

impl Default for ManagedSessionTaskState {
    fn default() -> Self {
        Self::Unknown
    }
}

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
    pub fn session_instance_id(&self) -> &str {
        self.address.session_id()
    }

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
        let role_tag = if self.is_workspace_chrome() {
            " [main]"
        } else if self.is_target_host() {
            " [target]"
        } else {
            ""
        };
        format!(
            "{}: {} windows ({}){}",
            self.address.display_session_id(),
            self.window_count,
            if self.attached_clients > 0 {
                "attached"
            } else {
                "detached"
            },
            role_tag,
        )
    }

    fn display_scope(&self) -> String {
        match self.address.transport() {
            SessionTransport::LocalTmux => {
                if self.is_workspace_chrome() {
                    "local [main]".to_string()
                } else {
                    "local".to_string()
                }
            }
            SessionTransport::RemotePeer => format!(
                "{}:{}",
                self.address.display_authority_id(),
                self.address.display_session_id()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConsoleAttachment, ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord,
        ManagedSessionTaskState, SessionAvailability, SessionTransport,
    };
    use crate::domain::agent_detector::DetectorRegistry;
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
        assert_eq!(line, "1234: 3 windows (attached) [main]");
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
    fn task_state_infers_confirm_from_trailing_choice_indicator() {
        let state = DetectorRegistry::default()
            .infer_task_state(Some("claude"), "Run this command?\nApprove? [y/N]");

        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_confirm_from_claude_tool_approval() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "Claude wants to run:\n  ls -la\nAllow this command?",
        );

        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_does_not_infer_confirm_from_conversational_text() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "I confirm this is a good idea\nLet me run the tool now\nRunning tool call xyz",
        );

        // "confirm" in running output should NOT trigger Confirm
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_infers_input_from_codex_prompt_line() {
        let state = DetectorRegistry::default().infer_task_state(Some("codex"), "Tip\n› ");

        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_input_from_kimi_welcome_prompt() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("kimi"),
            "Welcome to Kimi Code!\n\
             Send /help for help information.\n\
             Directory: /opt/data/workspace/app-insight\n\
             ╭────────────────────────────────────────╮\n\
             │ >                                      │\n\
             ╰────────────────────────────────────────╯\n\
             K2.7 Code thinking  /model: switch model",
        );

        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_input_from_resumed_kimi_prompt() {
        let command_name = DetectorRegistry::default().detect_command_name("kimi-code", None, "");
        let state = DetectorRegistry::default().infer_task_state(
            Some(&command_name),
            "✨ 列出当前目录文件\n\
             ● Used Bash (ls -la)\n\
             ● 当前目录 /home/kk/ozon 的文件和目录如下：\n\
             ╭────────────────────────────────────────╮\n\
             │ >                                      │\n\
             ╰────────────────────────────────────────╯\n\
             K2.7 Code thinking  ~/ozon  /sessions to browse and resume earlier sessions\n\
             context: 6.7% (17.6k/262.1k)",
        );

        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_keeps_kimi_running_without_prompt() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("kimi"),
            "Kimi Code\n\
             Reading files...\n\
             Updating implementation\n\
             K2.7 Code thinking  context: 12.3%",
        );

        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_keeps_kimi_running_when_spinner_and_prompt_are_visible() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("kimi"),
            "✨ say one short sentence about waitagent\n\
             ⠼ thinking...\n\
             No tools needed since it is just a\n\
             ╭────────────────────────────────────────╮\n\
             │ >                                      │\n\
             ╰────────────────────────────────────────╯\n\
             K2.7 Code thinking  …/data/workspace/wait-agent\n\
             context: 0.0% (0/262.1k)",
        );

        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_keeps_kimi_running_when_working_spinner_and_prompt_are_visible() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("kimi"),
            "4. Check the controlling TTY and ioctl/ttyname resolution\n\
             Reasoning: Detached or nested sessions can return unexpected paths\n\
             ⠹ working...\n\
             ╭────────────────────────────────────────╮\n\
             │ >                                      │\n\
             ╰────────────────────────────────────────╯\n\
             K2.7 Code thinking  …/data/workspace/wait-agent\n\
             context: 0.0% (0/262.1k)",
        );

        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_keeps_kimi_running_when_moon_spinner_and_prompt_are_visible() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("kimi"),
            "✨ hi\n\
             🌔\n\
             ╭────────────────────────────────────────╮\n\
             │ >                                      │\n\
             ╰────────────────────────────────────────╯\n\
             K2.7 Code thinking  …/data/workspace/wait-agent\n\
             context: 0.0% (0/262.1k)",
        );

        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_infers_input_from_claude_prompt_line_case_insensitively() {
        let state = DetectorRegistry::default()
            .infer_task_state(Some("claude"), "Ready\nType your message");

        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_does_not_infer_input_from_stale_agent_prompt_line() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "Claude\nType your message\n\nRunning tool call",
        );

        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_infers_input_from_shell_prompt_line() {
        let state = DetectorRegistry::default().infer_task_state(Some("bash"), "k@host:/tmp$");

        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_input_from_claude_tui_prompt_with_footer() {
        // Claude Code's TUI places the ❯ prompt line above a footer/status line.
        // The last non-empty line is NOT the prompt — this tests that the detector
        // scans ALL lines for the ❯ prompt character.
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "▐▛███▜▌   Claude Code v2.1.128\n\
             ─────────────────────────────────────\n\
             ❯ \n\
             ─────────────────────────────────────\n\
               ? for shortcuts    ● high · /effort",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_input_from_codex_tui_prompt_with_trailing_instruction() {
        // Codex's input prompt (non-menu) should be Input, not Confirm.
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "Chat with Codex CLI\n\
             ──────────────────\n\
             › \n\
             Press enter or type your message",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_confirm_from_codex_trust_prompt() {
        // Codex's trust prompt uses a numbered menu (› 1. / 2.) —
        // this must be Confirm, not Input.
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "You are in /opt/data/workspace\n\
             › 1. Yes, continue\n\
             2. No, quit\n\
             Press enter to continue",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_confirm_from_codex_directory_trust_screen() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "> You are in /tmp/waitagent-repro-codex-hooks\n\
             \n\
             Note: You're in a subdirectory of a Git\n\
             project. Trusting will apply to the\n\
             repository root: /tmp\n\
             \n\
             Do you trust the contents of this directory?\n\
             Working with untrusted contents comes with\n\
             higher risk of prompt injection. Trusting the\n\
             directory allows project-local config, hooks,\n\
             and exec policies to load.\n\
             \n\
             › 1. Yes, continue\n\
             2. No, quit\n\
             \n\
             Press enter to continue",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_confirm_from_codex_hooks_review_screen() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "Hooks need review\n\
             5 hooks are new or changed.\n\
             Hooks can run outside the sandbox after you\n\
             \n\
             › 1. Review hooks\n\
             2. Trust all and continue\n\
             3. Continue without trusting (hooks won't\n\
             run)\n\
             \n\
             Press enter to confirm or esc to go back",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_confirm_from_codex_update_menu_with_wrapped_option() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "Update available! 0.125.0 -> 0.128.0\n\
             Release notes: https://github.com/openai/codex\n\
             \n\
             › 1. Update now (runs `npm install -g\n\
             @openai/codex`)\n\
             2. Skip\n\
             3. Skip until next version\n\
             \n\
             Press enter to continue",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_input_from_codex_startup_input_screen() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "╭─────────────────────────────────────────────╮\n\
             │ >_ OpenAI Codex (v0.142.2)                  │\n\
             │                                             │\n\
             │ model:     gpt-5.5   /model to change       │\n\
             │ directory: /tmp/waitagent-repro-codex-hooks │\n\
             ╰─────────────────────────────────────────────╯\n\
             \n\
             Tip: GPT-5.5 is now available in Codex. It's\n\
             our strongest agentic coding model yet, built\n\
             to reason through large codebases, check\n\
             assumptions with tools, and keep going until\n\
             the work is done.\n\
             \n\
             Learn more:\n\
             https://openai.com/index/introducing-gpt-5-5/\n\
             \n\
             \n\
             › Find and fix a bug in @filename\n\
             \n\
             gpt-5.5 default · /tmp/waitagent-repro-codex…",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_input_from_codex_startup_prompt_with_footer() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "⚠ Codex could not find bubblewrap on PATH.\n\
             Install bubblewrap with your OS package\n\
             manager. See the sandbox prerequisites:\n\
             https://developers.openai.com/codex/concepts/\n\
             sandboxing#prerequisites.\n\
             Codex will use the bundled bubblewrap in the\n\
             meantime.\n\
             \n\
             ╭─────────────────────────────────────────────╮\n\
             │ >_ OpenAI Codex (v0.142.2)                  │\n\
             │                                             │\n\
             │ model:     gpt-5.5 high   /model to change  │\n\
             │ directory: /tmp/…t-repro-fix-workspace-auth │\n\
             ╰─────────────────────────────────────────────╯\n\
             \n\
             Tip: New Build faster with Codex.\n\
             \n\
             \n\
             › Use /skills to list available skills\n\
             \n\
             gpt-5.5 high · /tmp/waitagent-repro-fix-work…",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_input_from_codex_update_notice_with_long_tip() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "╭─────────────────────────────────────────────────╮\n\
             │ ✨ Update available! 0.142.2 -> 0.142.3         │\n\
             │ Run npm install -g @openai/codex to update.     │\n\
             │                                                 │\n\
             │ See full release notes:                         │\n\
             │ https://github.com/openai/codex/releases/latest │\n\
             ╰─────────────────────────────────────────────────╯\n\
             \n\
             ⚠ Codex could not find bubblewrap on PATH. Install bubblewrap with your OS package manager. See the sandbox prerequisites:\n\
             https://developers.openai.com/codex/concepts/sandboxing#prerequisites. Codex will use the bundled bubblewrap in the meantime.\n\
             \n\
             ╭─────────────────────────────────────────────────╮\n\
             │ >_ OpenAI Codex (v0.142.2)                      │\n\
             │                                                 │\n\
             │ model:     gpt-5.5 high   /model to change      │\n\
             │ directory: /tmp/waitagent-repro-update-notice-r │\n\
             ╰─────────────────────────────────────────────────╯\n\
             \n\
             Tip: Use the OpenAI docs MCP for API questions; enable it with codex mcp add openaiDeveloperDocs --url https://developers.openai.com/mcp.\n\
             \n\
             \n\
             › Improve documentation in @filename\n\
             \n\
             gpt-5.5 high · /tmp/waitagent-repro-update-notice-r",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_confirm_from_claude_tui_numbered_menu() {
        // Claude Code's TUI confirmation uses a numbered menu with ❯ for selection.
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "Do you want to create claude_test_file.txt?\n\
             ❯ 1. Yes\n\
             2. No\n\
             Esc to cancel · Tab to amend",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_confirm_from_codex_allow_keyword() {
        // Codex's "Allow Codex to run" prompt (pre-menu, no numbered options yet).
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "Allow Codex to run this command: echo hello\n\
             › ",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_confirm_from_codex_dialog_marker() {
        // Codex uses `?` at line start as a ratatui dialog marker for
        // confirmation prompts, before the numbered menu renders.
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "? Allow Codex to run: echo hello\n\
             › ",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_infers_confirm_from_claude_dialog_marker() {
        // Claude uses `?` dialog marker for confirmation, with ❯ prompt below.
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "? Allow this command?\n\
             ❯ ",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_input_not_confirm_from_user_question_before_prompt() {
        // A user question ending with `?` in the conversation, followed by
        // Codex's response and then `›`, should still be Input (not Confirm).
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "User: How do I list files?\n\
             Codex: You can use `ls`.\n\
             \n\
             › \n\
             tip: use @ to reference",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_typing_in_codex_confirm_dialog_stays_confirm() {
        // When the "allow codex" keyword is present, even typing at the prompt
        // stays Confirm — the user is still in the confirmation flow.
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "? Allow Codex to run: echo hello\n\
             › yes I want to run this",
        );
        assert_eq!(state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn task_state_input_not_confirm_when_arrow_has_no_menu() {
        // Plain ❯ on its own (no "1." / "2." menu) must still be Input.
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "Some output\n\
             ─────────────────────\n\
             ❯ \n\
             ─────────────────────\n\
             status line",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_input_when_claude_has_partial_typed_text() {
        // User typing at the prompt: ❯ followed by text, no numbered menu.
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "❯ create a file named hello.txt\n\
             ──────────────────────────────\n\
             status line",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_remains_running_during_claude_tool_execution() {
        // When claude is actively executing tools, ❯ should NOT appear in the pane.
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "I'll help you with that!\n\
             Creating files:\n\
               - src/main.rs\n\
             Running tool call xyz",
        );
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_remains_running_during_codex_tool_execution() {
        // During codex execution, › prompt should not appear.
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "Searching files...\n\
             Running analysis\n\
             Done.",
        );
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_remains_running_when_codex_working_with_visible_prompt() {
        let state = DetectorRegistry::default().infer_task_state(
            Some("codex"),
            "• Working\n\
             └ Reading src/main.rs\n\
             \n\
             › \n\
             esc to interrupt",
        );
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn task_state_input_when_claude_visible_prompt_regardless_of_status() {
        // The ❯ prompt followed by a ── separator triggers Input detection
        // regardless of status-line content. Execution vs. idle is
        // disambiguated by the temporal content-change check in
        // session_metadata.rs, not by the detector alone.
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "❯ run echo hello\n\
             ● Bash(echo hello)\n\
             \n\
             ─────────────────────\n\
             ❯ \n\
             ─────────────────────\n\
             esc to interrupt    ● high · /effort",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn task_state_infers_input_when_claude_prompt_with_normal_status() {
        // After execution completes, status returns to "? for shortcuts" → Input.
        let state = DetectorRegistry::default().infer_task_state(
            Some("claude"),
            "❯ run echo hello\n\
             ● Done.\n\
             \n\
             ─────────────────────\n\
             ❯ \n\
             ─────────────────────\n\
             ? for shortcuts    ● high · /effort",
        );
        assert_eq!(state, ManagedSessionTaskState::Input);
    }
}
