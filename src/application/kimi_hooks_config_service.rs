use crate::application::agent_hooks_config_service::AgentHooksConfigService;
use std::path::PathBuf;

#[cfg(test)]
use crate::application::agent_hooks_config_service as generic;
#[cfg(test)]
use std::path::Path;

/// Thin wrapper around [`AgentHooksConfigService`] for Kimi compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiHooksConfigService(AgentHooksConfigService);

impl KimiHooksConfigService {
    pub fn new(kimi_home: PathBuf, sender_path: PathBuf) -> Self {
        Self(AgentHooksConfigService::kimi(kimi_home, sender_path))
    }

    pub fn from_env(sender_path: PathBuf) -> Self {
        let kimi_home = std::env::var_os("KIMI_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".kimi-code")))
            .unwrap_or_else(|| PathBuf::from(".kimi-code"));
        Self::new(kimi_home, sender_path)
    }
}

impl crate::ports::hooks_config::HooksConfigPort for KimiHooksConfigService {
    fn agent_name(&self) -> &'static str {
        self.0.agent_name()
    }

    fn reconcile(&self) -> std::io::Result<()> {
        self.0.reconcile()
    }
}

#[cfg(test)]
fn reconcile_config_text(content: &str, sender_path: &Path, events: &[&str]) -> String {
    generic::reconcile_toml_hooks_text(
        content,
        sender_path,
        events,
        generic::kimi_waitagent_hook_predicate,
        "kimi",
    )
}

#[cfg(test)]
fn kimi_hook_events() -> &'static [&'static str] {
    generic::hook_events_for_agent("kimi")
}

#[cfg(test)]
fn kimi_hook_command(sender_path: &Path, event: &str) -> String {
    generic::hook_command(sender_path, event, "kimi")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_preserves_user_config_and_replaces_waitagent_hooks() {
        let content = r#"default_model = "kimi-code/kimi-for-coding"

[providers."managed:kimi-code"]
type = "kimi"

[[hooks]]
event = "PreToolUse"
command = "echo user"

[[hooks]]
event = "Stop"
command = "/tmp/agent-signal-send Stop"
# waitagent-agent-signal:Stop
"#;

        let next = reconcile_config_text(
            content,
            Path::new("/tmp/agent signal send"),
            kimi_hook_events(),
        );
        assert!(next.contains("default_model = \"kimi-code/kimi-for-coding\""));
        assert!(next.contains("[providers.\"managed:kimi-code\"]"));
        assert!(next.contains("command = \"echo user\""));
        assert!(!next.contains("/tmp/agent-signal-send Stop"));
        assert!(next.contains("WAITAGENT_AGENT_NAME=kimi"));
        assert!(next.contains("event = \"SessionStart\""));
        assert!(next.contains("event = \"UserPromptSubmit\""));
        assert!(next.contains("event = \"PermissionResult\""));
        assert!(next.contains("event = \"SessionEnd\""));
    }

    #[test]
    fn hook_command_quotes_sender_path_and_event() {
        assert_eq!(
            kimi_hook_command(Path::new("/tmp/agent signal send"), "Stop"),
            "WAITAGENT_AGENT_NAME=kimi '/tmp/agent signal send' 'Stop'"
        );
    }
}
