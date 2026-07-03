use crate::domain::agent_detector::DetectorRegistry;
use serde_json::{json, Map, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const WAITAGENT_HOOK_TAG: &str = "waitagent-agent-signal";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHooksConfigService {
    codex_home: PathBuf,
    sender_path: PathBuf,
}

impl CodexHooksConfigService {
    pub fn new(codex_home: PathBuf, sender_path: PathBuf) -> Self {
        Self {
            codex_home,
            sender_path,
        }
    }

    pub fn from_env(sender_path: PathBuf) -> Self {
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
            .unwrap_or_else(|| PathBuf::from(".codex"));
        Self::new(codex_home, sender_path)
    }

    pub fn reconcile(&self) -> io::Result<()> {
        let hooks_path = self.codex_home.join("hooks.json");
        let value = read_hooks_json_or_backup(&hooks_path)?;
        let next = reconcile_hooks_value(value, &self.sender_path, codex_hook_events());
        if let Some(parent) = hooks_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&hooks_path, serde_json::to_vec_pretty(&next)?)?;
        Ok(())
    }
}

fn read_hooks_json_or_backup(path: &Path) -> io::Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let bytes = fs::read(path)?;
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(json!({}));
    }
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(value),
        Err(_) => {
            let backup = path.with_file_name(format!(
                "hooks.json.waitagent-bak-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_millis())
                    .unwrap_or(0)
            ));
            fs::write(&backup, bytes)?;
            Ok(json!({}))
        }
    }
}

fn reconcile_hooks_value(value: Value, sender_path: &Path, events: &[&str]) -> Value {
    let mut root = match value {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let hooks = hooks
        .as_object_mut()
        .expect("hooks was just normalized to object");
    for event in events {
        let entries = hooks
            .entry(*event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let array = match entries {
            Value::Array(array) => array,
            _ => {
                *entries = Value::Array(Vec::new());
                entries.as_array_mut().expect("entry was just set to array")
            }
        };
        array.retain(|entry| {
            let first_hook = entry
                .get("hooks")
                .and_then(Value::as_array)
                .and_then(|hooks| hooks.first());
            let tagged_by_status = first_hook
                .and_then(|hook| hook.get("statusMessage"))
                .and_then(Value::as_str)
                .map(|message| !message.starts_with(WAITAGENT_HOOK_TAG))
                .unwrap_or(true);
            let tagged_by_command = first_hook
                .and_then(|hook| hook.get("command"))
                .and_then(Value::as_str)
                .map(|command| {
                    command.contains("agent-signal-send")
                        || (command.contains("WAITAGENT_SIGNAL_SOCKET")
                            && command.contains("UNIX-SENDTO")
                            && command.contains("\"agent\":\"codex\""))
                })
                .unwrap_or(false);
            tagged_by_status && !tagged_by_command
        });
        array.push(waitagent_hook_group(event, sender_path));
    }
    Value::Object(root)
}

fn codex_hook_events() -> &'static [&'static str] {
    DetectorRegistry::default()
        .hook_events_for_agent("codex")
        .unwrap_or(&[])
}

fn waitagent_hook_group(event: &str, sender_path: &Path) -> Value {
    json!({
        "hooks": [
            {
                "type": "command",
                "command": codex_hook_command(sender_path, event)
            }
        ]
    })
}

fn codex_hook_command(sender_path: &Path, event: &str) -> String {
    format!(
        "{} {}",
        shell_single_quote(sender_path.to_string_lossy().as_ref()),
        shell_single_quote(event)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_preserves_user_hooks_and_replaces_waitagent_hooks() {
        let value = json!({
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

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
