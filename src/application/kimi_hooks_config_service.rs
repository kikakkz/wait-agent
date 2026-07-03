use crate::domain::agent_detector::DetectorRegistry;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const WAITAGENT_HOOK_TAG: &str = "waitagent-agent-signal";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiHooksConfigService {
    kimi_home: PathBuf,
    sender_path: PathBuf,
}

impl KimiHooksConfigService {
    pub fn new(kimi_home: PathBuf, sender_path: PathBuf) -> Self {
        Self {
            kimi_home,
            sender_path,
        }
    }

    pub fn from_env(sender_path: PathBuf) -> Self {
        let kimi_home = std::env::var_os("KIMI_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".kimi-code")))
            .unwrap_or_else(|| PathBuf::from(".kimi-code"));
        Self::new(kimi_home, sender_path)
    }

    pub fn reconcile(&self) -> io::Result<()> {
        let config_path = self.kimi_home.join("config.toml");
        let content = read_config_or_backup(&config_path)?;
        let next = reconcile_config_text(&content, &self.sender_path, kimi_hook_events());
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, next)?;
        Ok(())
    }
}

fn read_config_or_backup(path: &Path) -> io::Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let content = fs::read_to_string(path)?;
    if config_text_has_unclosed_multiline_string(&content) {
        let backup = path.with_file_name(format!(
            "config.toml.waitagent-bak-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0)
        ));
        fs::write(&backup, content.as_bytes())?;
        return Ok(String::new());
    }
    Ok(content)
}

fn reconcile_config_text(content: &str, sender_path: &Path, events: &[&str]) -> String {
    let mut kept = Vec::new();
    let mut current_hook = Vec::new();
    let mut in_hook = false;
    for line in content.lines() {
        if line.trim() == "[[hooks]]" {
            flush_hook(&mut kept, &mut current_hook);
            current_hook.push(line.to_string());
            in_hook = true;
            continue;
        }
        if in_hook && line.trim_start().starts_with('[') {
            flush_hook(&mut kept, &mut current_hook);
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
    flush_hook(&mut kept, &mut current_hook);
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    for event in events {
        out.push_str(&kimi_hook_block(sender_path, event));
        out.push('\n');
    }
    out
}

fn kimi_hook_events() -> &'static [&'static str] {
    DetectorRegistry::default()
        .hook_events_for_agent("kimi")
        .unwrap_or(&[])
}

fn flush_hook(kept: &mut Vec<String>, current_hook: &mut Vec<String>) {
    if current_hook.is_empty() {
        return;
    }
    let text = current_hook.join("\n");
    if !is_waitagent_hook_block(&text) {
        kept.extend(current_hook.drain(..));
    } else {
        current_hook.clear();
    }
}

fn is_waitagent_hook_block(block: &str) -> bool {
    block.contains(WAITAGENT_HOOK_TAG) || block.contains("agent-signal-send")
}

fn kimi_hook_block(sender_path: &Path, event: &str) -> String {
    format!(
        "[[hooks]]\nevent = {}\ncommand = {}\ntimeout = 5\n# {WAITAGENT_HOOK_TAG}:{event}\n",
        toml_string(event),
        toml_string(&kimi_hook_command(sender_path, event))
    )
}

fn kimi_hook_command(sender_path: &Path, event: &str) -> String {
    format!(
        "WAITAGENT_AGENT_NAME=kimi {} {}",
        shell_single_quote(sender_path.to_string_lossy().as_ref()),
        shell_single_quote(event)
    )
}

fn toml_string(value: &str) -> String {
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

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn config_text_has_unclosed_multiline_string(content: &str) -> bool {
    content.matches("\"\"\"").count() % 2 != 0 || content.matches("'''").count() % 2 != 0
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
