use serde_json::{json, Map, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const WAITAGENT_HOOK_TAG: &str = "waitagent-agent-signal";
const CLAUDE_EVENTS: [&str; 7] = [
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "PostToolBatch",
    "Notification",
    "Stop",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeHooksConfigService {
    claude_home: PathBuf,
    sender_path: PathBuf,
}

impl ClaudeHooksConfigService {
    pub fn new(claude_home: PathBuf, sender_path: PathBuf) -> Self {
        Self {
            claude_home,
            sender_path,
        }
    }

    pub fn from_env(sender_path: PathBuf) -> Self {
        let claude_home = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
            .unwrap_or_else(|| PathBuf::from(".claude"));
        Self::new(claude_home, sender_path)
    }

    pub fn reconcile(&self) -> io::Result<()> {
        let settings_path = self.claude_home.join("settings.json");
        let value = read_json_or_backup(&settings_path)?;
        let next = reconcile_hooks_value(value, &self.sender_path);
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&settings_path, serde_json::to_vec_pretty(&next)?)?;
        Ok(())
    }
}

fn read_json_or_backup(path: &Path) -> io::Result<Value> {
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
                "settings.json.waitagent-bak-{}",
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

fn reconcile_hooks_value(value: Value, sender_path: &Path) -> Value {
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
    for event in CLAUDE_EVENTS {
        let entries = hooks
            .entry(event)
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
                .map(|command| command.contains("agent-signal-send"))
                .unwrap_or(false);
            tagged_by_status && !tagged_by_command
        });
        array.push(waitagent_hook_group(event, sender_path));
    }
    Value::Object(root)
}

fn waitagent_hook_group(event: &str, sender_path: &Path) -> Value {
    json!({
        "matcher": "",
        "hooks": [
            {
                "type": "command",
                "command": claude_hook_command(sender_path, event),
                "statusMessage": format!("{WAITAGENT_HOOK_TAG}:{event}")
            }
        ]
    })
}

fn claude_hook_command(sender_path: &Path, event: &str) -> String {
    format!(
        "{} {}",
        shell_single_quote(sender_path.to_string_lossy().as_ref()),
        shell_single_quote(event)
    )
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_preserves_user_hooks_and_replaces_waitagent_hooks() {
        let value = json!({
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
        let next = reconcile_hooks_value(value, &sender);
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
        for event in CLAUDE_EVENTS {
            assert!(next
                .get("hooks")
                .and_then(|hooks| hooks.get(event))
                .and_then(Value::as_array)
                .is_some());
        }
    }
}
