use crate::application::agent_hooks_config_service::AgentHooksConfigService;
use std::path::PathBuf;

#[cfg(test)]
use crate::application::agent_hooks_config_service as generic;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use std::path::Path;

/// Thin wrapper around [`AgentHooksConfigService`] for Claude compatibility.
///
/// New code should prefer [`AgentHooksConfigService::claude`] directly; this
/// type is kept so existing call sites and tests continue to compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeHooksConfigService(AgentHooksConfigService);

impl ClaudeHooksConfigService {
    pub fn new(claude_home: PathBuf, sender_path: PathBuf) -> Self {
        Self(AgentHooksConfigService::claude(claude_home, sender_path))
    }

    pub fn from_env(sender_path: PathBuf) -> Self {
        let claude_home = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
            .unwrap_or_else(|| PathBuf::from(".claude"));
        Self::new(claude_home, sender_path)
    }
}

impl crate::ports::hooks_config::HooksConfigPort for ClaudeHooksConfigService {
    fn agent_name(&self) -> &'static str {
        self.0.agent_name()
    }

    fn reconcile(&self) -> std::io::Result<()> {
        self.0.reconcile()
    }
}

#[cfg(test)]
fn reconcile_hooks_value(value: Value, sender_path: &Path, events: &[&str]) -> Value {
    generic::reconcile_json_hooks_value(
        value,
        sender_path,
        events,
        generic::claude_waitagent_hook_predicate,
        "claude",
    )
}

#[cfg(test)]
fn claude_hook_events() -> &'static [&'static str] {
    generic::hook_events_for_agent("claude")
}

#[cfg(test)]
fn claude_hook_command(sender_path: &Path, event: &str) -> String {
    generic::hook_command(sender_path, event, "claude")
}

#[cfg(test)]
fn shell_single_quote(value: &str) -> String {
    generic::shell_single_quote(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_preserves_user_hooks_and_replaces_waitagent_hooks() {
        let value = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "echo user"}]
                    },
                    {
                        "hooks": [{
                            "type": "command",
                            "command": "old",
                            "statusMessage": "waitagent-agent-signal:old"
                        }]
                    }
                ]
            },
            "permissions": {
                "allow": ["Bash(ls:*)"]
            }
        });

        let sender = PathBuf::from("/tmp/agent signal send");
        let next = reconcile_hooks_value(value, &sender, claude_hook_events());
        let pre_tool = next
            .get("hooks")
            .and_then(|hooks| hooks.get("PreToolUse"))
            .and_then(Value::as_array)
            .expect("PreToolUse entries should exist");
        assert_eq!(pre_tool.len(), 2);
        assert_eq!(
            pre_tool[0]
                .get("hooks")
                .and_then(Value::as_array)
                .and_then(|hooks| hooks.first())
                .and_then(|hook| hook.get("command"))
                .and_then(Value::as_str),
            Some("echo user")
        );
        assert_eq!(pre_tool[1].get("matcher").and_then(Value::as_str), Some(""));
        assert_eq!(
            pre_tool[1]
                .get("hooks")
                .and_then(Value::as_array)
                .and_then(|hooks| hooks.first())
                .and_then(|hook| hook.get("command"))
                .and_then(Value::as_str),
            Some(claude_hook_command(&sender, "PreToolUse").as_str())
        );
        assert!(next.get("permissions").is_some());
        for event in claude_hook_events() {
            assert!(next
                .get("hooks")
                .and_then(|hooks| hooks.get(event))
                .and_then(Value::as_array)
                .is_some());
        }
        assert!(next
            .get("hooks")
            .and_then(|hooks| hooks.get("SessionEnd"))
            .and_then(Value::as_array)
            .is_some());
    }
}
