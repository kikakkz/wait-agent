use crate::domain::agent_detector::DetectorRegistry;
use serde_json::{json, Map, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const WAITAGENT_HOOK_TAG: &str = "waitagent-agent-signal";

/// Discriminates the on-disk hook configuration format for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    /// JSON-based config (Claude, Codex). The predicate decides whether a
    /// single hook object belongs to waitagent and should be replaced.
    Json {
        is_waitagent_hook: fn(&Value) -> bool,
    },
    /// TOML-based config (Kimi). The predicate decides whether a `[[hooks]]`
    /// block belongs to waitagent and should be replaced.
    Toml { is_waitagent_hook: fn(&str) -> bool },
}

/// Generic, agent-parameterized hook configuration service.
///
/// The three agent-specific services (`ClaudeHooksConfigService`,
/// `CodexHooksConfigService`, `KimiHooksConfigService`) are thin wrappers
/// around this type so that the shared reconcile/backup/quoting logic lives
/// in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHooksConfigService {
    agent_name: &'static str,
    home_dir: PathBuf,
    config_file_name: &'static str,
    backup_file_name: &'static str,
    sender_path: PathBuf,
    config_format: ConfigFormat,
}

impl AgentHooksConfigService {
    pub fn new(
        agent_name: &'static str,
        home_dir: PathBuf,
        config_file_name: &'static str,
        backup_file_name: &'static str,
        sender_path: PathBuf,
        config_format: ConfigFormat,
    ) -> Self {
        Self {
            agent_name,
            home_dir,
            config_file_name,
            backup_file_name,
            sender_path,
            config_format,
        }
    }

    pub fn claude(claude_home: PathBuf, sender_path: PathBuf) -> Self {
        Self::new(
            "claude",
            claude_home,
            "settings.json",
            "settings.json",
            sender_path,
            ConfigFormat::Json {
                is_waitagent_hook: claude_waitagent_hook_predicate,
            },
        )
    }

    pub fn codex(codex_home: PathBuf, sender_path: PathBuf) -> Self {
        Self::new(
            "codex",
            codex_home,
            "hooks.json",
            "hooks.json",
            sender_path,
            ConfigFormat::Json {
                is_waitagent_hook: codex_waitagent_hook_predicate,
            },
        )
    }

    pub fn kimi(kimi_home: PathBuf, sender_path: PathBuf) -> Self {
        Self::new(
            "kimi",
            kimi_home,
            "config.toml",
            "config.toml",
            sender_path,
            ConfigFormat::Toml {
                is_waitagent_hook: kimi_waitagent_hook_predicate,
            },
        )
    }

    pub fn agent_name(&self) -> &'static str {
        self.agent_name
    }

    pub fn reconcile(&self) -> io::Result<()> {
        let config_path = self.home_dir.join(self.config_file_name);
        let events = hook_events_for_agent(self.agent_name);

        let next = match &self.config_format {
            ConfigFormat::Json { is_waitagent_hook } => {
                let value = read_json_or_backup(&config_path, self.backup_file_name)?;
                let next = reconcile_json_hooks_value(
                    value,
                    &self.sender_path,
                    events,
                    *is_waitagent_hook,
                    self.agent_name,
                );
                serde_json::to_vec_pretty(&next)?
            }
            ConfigFormat::Toml { is_waitagent_hook } => {
                let content = read_toml_or_backup(&config_path, self.backup_file_name)?;
                reconcile_toml_hooks_text(
                    &content,
                    &self.sender_path,
                    events,
                    *is_waitagent_hook,
                    self.agent_name,
                )
                .into_bytes()
            }
        };

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, next)?;
        Ok(())
    }
}

impl crate::ports::hooks_config::HooksConfigPort for AgentHooksConfigService {
    fn agent_name(&self) -> &'static str {
        self.agent_name
    }

    fn reconcile(&self) -> io::Result<()> {
        self.reconcile()
    }
}

pub(crate) fn hook_events_for_agent(agent_name: &'static str) -> &'static [&'static str] {
    DetectorRegistry::default()
        .hook_events_for_agent(agent_name)
        .unwrap_or(&[])
}

fn backup_path(path: &Path, backup_stem: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    path.with_file_name(format!("{backup_stem}.waitagent-bak-{timestamp}"))
}

pub fn read_json_or_backup(path: &Path, backup_stem: &str) -> io::Result<Value> {
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
            let backup = backup_path(path, backup_stem);
            fs::write(&backup, bytes)?;
            Ok(json!({}))
        }
    }
}

pub fn reconcile_json_hooks_value(
    value: Value,
    sender_path: &Path,
    events: &[&str],
    is_waitagent_hook: fn(&Value) -> bool,
    agent_name: &str,
) -> Value {
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
    // `hooks` was normalized to `Value::Object` immediately above, so this
    // conversion is an invariant and cannot fail.
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
                // `entries` was just set to `Value::Array` above, so this
                // conversion is an invariant and cannot fail.
                *entries = Value::Array(Vec::new());
                entries.as_array_mut().expect("entry was just set to array")
            }
        };
        array.retain(|entry| !is_waitagent_hook_entry(entry, is_waitagent_hook));
        array.push(waitagent_json_hook_group(event, sender_path, agent_name));
    }
    Value::Object(root)
}

fn is_waitagent_hook_entry(entry: &Value, is_waitagent_hook: fn(&Value) -> bool) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .and_then(|hooks| hooks.first())
        .map(is_waitagent_hook)
        .unwrap_or(false)
}

pub fn waitagent_json_hook_group(event: &str, sender_path: &Path, agent_name: &str) -> Value {
    let command = hook_command(sender_path, event, agent_name);
    if agent_name == "claude" {
        json!({
            "matcher": "",
            "hooks": [
                {
                    "type": "command",
                    "command": command,
                    "statusMessage": format!("{WAITAGENT_HOOK_TAG}:{event}")
                }
            ]
        })
    } else {
        json!({
            "hooks": [
                {
                    "type": "command",
                    "command": command
                }
            ]
        })
    }
}

pub fn hook_command(sender_path: &Path, event: &str, agent_name: &str) -> String {
    let quoted_sender = shell_single_quote(sender_path.to_string_lossy().as_ref());
    let quoted_event = shell_single_quote(event);
    if agent_name == "kimi" {
        format!("WAITAGENT_AGENT_NAME=kimi {quoted_sender} {quoted_event}")
    } else {
        format!("{quoted_sender} {quoted_event}")
    }
}

pub fn claude_waitagent_hook_predicate(hook: &Value) -> bool {
    let tagged_by_status = hook
        .get("statusMessage")
        .and_then(Value::as_str)
        .map(|message| message.starts_with(WAITAGENT_HOOK_TAG))
        .unwrap_or(false);
    let tagged_by_command = hook
        .get("command")
        .and_then(Value::as_str)
        .map(|command| command.contains("agent-signal-send"))
        .unwrap_or(false);
    tagged_by_status || tagged_by_command
}

pub fn codex_waitagent_hook_predicate(hook: &Value) -> bool {
    let tagged_by_status = hook
        .get("statusMessage")
        .and_then(Value::as_str)
        .map(|message| message.starts_with(WAITAGENT_HOOK_TAG))
        .unwrap_or(false);
    let tagged_by_command = hook
        .get("command")
        .and_then(Value::as_str)
        .map(|command| {
            command.contains("agent-signal-send")
                || (command.contains("WAITAGENT_SIGNAL_SOCKET")
                    && command.contains("UNIX-SENDTO")
                    && command.contains("\"agent\":\"codex\""))
        })
        .unwrap_or(false);
    tagged_by_status || tagged_by_command
}

pub fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn read_toml_or_backup(path: &Path, backup_stem: &str) -> io::Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let content = fs::read_to_string(path)?;
    if config_text_has_unclosed_multiline_string(&content) {
        let backup = backup_path(path, backup_stem);
        fs::write(&backup, content.as_bytes())?;
        return Ok(String::new());
    }
    Ok(content)
}

pub fn reconcile_toml_hooks_text(
    content: &str,
    sender_path: &Path,
    events: &[&str],
    is_waitagent_hook: fn(&str) -> bool,
    agent_name: &str,
) -> String {
    let mut kept = Vec::new();
    let mut current_hook = Vec::new();
    let mut in_hook = false;
    for line in content.lines() {
        if line.trim() == "[[hooks]]" {
            flush_hook(&mut kept, &mut current_hook, is_waitagent_hook);
            current_hook.push(line.to_string());
            in_hook = true;
            continue;
        }
        if in_hook && line.trim_start().starts_with('[') {
            flush_hook(&mut kept, &mut current_hook, is_waitagent_hook);
            in_hook = false;
            kept.push(line.to_string());
            continue;
        }
        if in_hook {
            current_hook.push(line.to_string());
        } else {
            kept.push(line.to_string());
        }
    }
    flush_hook(&mut kept, &mut current_hook, is_waitagent_hook);
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    for event in events {
        out.push_str(&toml_hook_block(sender_path, event, agent_name));
        out.push('\n');
    }
    out
}

fn flush_hook(
    kept: &mut Vec<String>,
    current_hook: &mut Vec<String>,
    is_waitagent_hook: fn(&str) -> bool,
) {
    if current_hook.is_empty() {
        return;
    }
    let text = current_hook.join("\n");
    if is_waitagent_hook(&text) {
        current_hook.clear();
    } else {
        kept.append(current_hook);
    }
}

fn toml_hook_block(sender_path: &Path, event: &str, agent_name: &str) -> String {
    format!(
        "[[hooks]]\nevent = {}\ncommand = {}\ntimeout = 5\n# {WAITAGENT_HOOK_TAG}:{event}\n",
        toml_string(event),
        toml_string(&hook_command(sender_path, event, agent_name))
    )
}

pub fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn kimi_waitagent_hook_predicate(block: &str) -> bool {
    block.contains(WAITAGENT_HOOK_TAG) || block.contains("agent-signal-send")
}

pub fn config_text_has_unclosed_multiline_string(content: &str) -> bool {
    content.matches("\"\"\"").count() % 2 != 0 || content.matches("'''").count() % 2 != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_home(agent: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("waitagent-test-{agent}-{timestamp}"))
    }

    #[test]
    fn generic_service_reconciles_claude_settings() {
        let home = temp_home("claude");
        fs::create_dir_all(&home).unwrap();
        let settings_path = home.join("settings.json");
        let mut file = fs::File::create(&settings_path).unwrap();
        file.write_all(
            br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"old"}]}]}}"#,
        )
        .unwrap();

        let service = AgentHooksConfigService::claude(home.clone(), PathBuf::from("/tmp/send"));
        service.reconcile().unwrap();

        let content = fs::read_to_string(&settings_path).unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        assert!(value
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(Value::as_array)
            .is_some());
        assert!(value
            .get("hooks")
            .and_then(|h| h.get("SessionEnd"))
            .and_then(Value::as_array)
            .is_some());
        crate::infra::best_effort::remove_dir_all(&home);
    }

    #[test]
    fn generic_service_reconciles_codex_hooks() {
        let home = temp_home("codex");
        fs::create_dir_all(&home).unwrap();
        let hooks_path = home.join("hooks.json");
        fs::write(&hooks_path, br#"{"hooks":{"PermissionRequest":[]}}"#).unwrap();

        let service = AgentHooksConfigService::codex(home.clone(), PathBuf::from("/tmp/send"));
        service.reconcile().unwrap();

        let value: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        assert!(value
            .get("hooks")
            .and_then(|h| h.get("PermissionRequest"))
            .and_then(Value::as_array)
            .is_some());
        crate::infra::best_effort::remove_dir_all(&home);
    }

    #[test]
    fn generic_service_reconciles_kimi_hooks() {
        let home = temp_home("kimi");
        fs::create_dir_all(&home).unwrap();
        let config_path = home.join("config.toml");
        fs::write(
            &config_path,
            "default_model = \"k\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"/tmp/send Stop\"\n# waitagent-agent-signal:Stop\n",
        )
        .unwrap();

        let service = AgentHooksConfigService::kimi(home.clone(), PathBuf::from("/tmp/send"));
        service.reconcile().unwrap();

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("default_model"));
        assert!(!content.contains("/tmp/send Stop"));
        assert!(content.contains("WAITAGENT_AGENT_NAME=kimi"));
        crate::infra::best_effort::remove_dir_all(&home);
    }

    #[test]
    fn json_reconcile_preserves_non_hook_entries() {
        let value = json!({
            "permissions": { "allow": ["Bash(ls:*)"] }
        });
        let next = reconcile_json_hooks_value(
            value,
            Path::new("/tmp/send"),
            &["PreToolUse"],
            claude_waitagent_hook_predicate,
            "claude",
        );
        assert!(next.get("permissions").is_some());
    }

    #[test]
    fn toml_reconcile_replaces_waitagent_blocks() {
        let content = r#"[[hooks]]
event = "Stop"
command = "/tmp/send Stop"
# waitagent-agent-signal:Stop
"#;
        let next = reconcile_toml_hooks_text(
            content,
            Path::new("/tmp/send"),
            &["UserPromptSubmit"],
            kimi_waitagent_hook_predicate,
            "kimi",
        );
        assert!(!next.contains("/tmp/send Stop"));
        assert!(next.contains("event = \"UserPromptSubmit\""));
    }
}
