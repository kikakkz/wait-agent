use crate::application::agent_hooks_config_service::AgentHooksConfigService;
use std::path::PathBuf;

#[cfg(test)]
use crate::application::agent_hooks_config_service as generic;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use std::path::Path;

/// Thin wrapper around [`AgentHooksConfigService`] for Codex compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHooksConfigService(AgentHooksConfigService);

impl CodexHooksConfigService {
    pub fn new(codex_home: PathBuf, sender_path: PathBuf) -> Self {
        Self(AgentHooksConfigService::codex(codex_home, sender_path))
    }

    pub fn from_env(sender_path: PathBuf) -> Self {
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
            .unwrap_or_else(|| PathBuf::from(".codex"));
        Self::new(codex_home, sender_path)
    }
}

impl crate::ports::hooks_config::HooksConfigPort for CodexHooksConfigService {
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
        generic::codex_waitagent_hook_predicate,
        "codex",
    )
}

#[cfg(test)]
fn codex_hook_events() -> &'static [&'static str] {
    generic::hook_events_for_agent("codex")
}

#[cfg(test)]
fn codex_hook_command(sender_path: &Path, event: &str) -> String {
    generic::hook_command(sender_path, event, "codex")
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
                "PermissionRequest": [
                    {"hooks": [{"type": "command", "command": "echo user"}]},
                    {
                        "hooks": [{
                            "type": "command",
                            "command": "old",
                            "statusMessage": "waitagent-agent-signal:old"
                        }]
                    }
                ]
            },
            "SessionStart": [
                {"name": "keep me", "command": "echo start"}
            ]
        });

        let sender = PathBuf::from("/tmp/agent signal send");
        let next = reconcile_hooks_value(value, &sender, codex_hook_events());
        let permission = next
            .get("hooks")
            .and_then(|hooks| hooks.get("PermissionRequest"))
            .and_then(Value::as_array)
            .expect("permission entries should exist");
        assert_eq!(permission.len(), 2);
        assert_eq!(
            permission[0]
                .get("hooks")
                .and_then(Value::as_array)
                .and_then(|hooks| hooks.first())
                .and_then(|hook| hook.get("command"))
                .and_then(Value::as_str),
            Some("echo user")
        );
        assert_eq!(
            permission[1]
                .get("hooks")
                .and_then(Value::as_array)
                .and_then(|hooks| hooks.first())
                .and_then(|hook| hook.get("command"))
                .and_then(Value::as_str),
            Some(codex_hook_command(&sender, "PermissionRequest").as_str())
        );
        assert!(next.get("SessionStart").is_some());
        for event in codex_hook_events() {
            assert!(next
                .get("hooks")
                .and_then(|hooks| hooks.get(event))
                .and_then(Value::as_array)
                .is_some());
        }
        assert!(next
            .get("hooks")
            .and_then(|hooks| hooks.get("Interrupt"))
            .and_then(Value::as_array)
            .is_some());
    }

    #[test]
    fn hook_command_invokes_bundled_sender_with_event_arg() {
        let command = codex_hook_command(Path::new("/tmp/agent signal send"), "Stop");
        assert_eq!(command, "'/tmp/agent signal send' 'Stop'");
    }
}
