use crate::domain::agent_detector::AgentDetector;
use crate::domain::session_catalog::ManagedSessionTaskState;

pub struct KimiDetector;

impl AgentDetector for KimiDetector {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn detect_from_process(
        &self,
        current_command: &str,
        argv: Option<&[String]>,
    ) -> Option<&'static str> {
        if current_command == "kimi" || current_command == "kimi.js" {
            return Some("kimi");
        }
        if let Some(argv) = argv {
            let is_kimi = argv.iter().any(|arg| {
                std::path::Path::new(arg)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name == "kimi" || name == "kimi.js")
            });
            if is_kimi {
                return Some("kimi");
            }
        }
        None
    }

    fn detect_from_pane_text(
        &self,
        _current_command: &str,
        pane_text: &str,
    ) -> Option<&'static str> {
        let lowered = pane_text.to_ascii_lowercase();
        if lowered.contains("welcome to kimi code")
            || lowered.contains("kimi code")
            || lowered.contains("k2.7 code")
            || lowered.contains("send /help for help information")
        {
            return Some("kimi");
        }
        None
    }

    fn infer_task_state(
        &self,
        command_name: Option<&str>,
        pane_text: &str,
    ) -> Option<ManagedSessionTaskState> {
        let command_name = command_name.unwrap_or_default();
        if command_name != "kimi" {
            return None;
        }
        let normalized_lines: Vec<&str> = pane_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let last_lines_start = normalized_lines.len().saturating_sub(8);

        for (i, line) in normalized_lines.iter().enumerate() {
            let lc = line.to_ascii_lowercase();
            if lc.contains("run this command")
                || lc.contains("allow this")
                || lc.ends_with("[y/n]")
                || lc.ends_with("(y/n)")
            {
                return Some(ManagedSessionTaskState::Confirm);
            }
            if line.trim_start().starts_with('?') && i + 1 < normalized_lines.len() {
                let next = normalized_lines[i + 1].trim_start();
                if kimi_prompt_is_empty(next) {
                    return Some(ManagedSessionTaskState::Confirm);
                }
            }
        }

        if kimi_has_active_animation(&normalized_lines) {
            return Some(ManagedSessionTaskState::Running);
        }

        for line in normalized_lines.iter().skip(last_lines_start) {
            if kimi_prompt_line(line) {
                return Some(ManagedSessionTaskState::Input);
            }
        }

        Some(ManagedSessionTaskState::Running)
    }
}

fn kimi_prompt_line(line: &str) -> bool {
    let line = line.trim();
    if kimi_prompt_is_empty(line) {
        return true;
    }
    if let Some(rest) = line.strip_prefix("│ >") {
        return !looks_like_status_line(rest);
    }
    if let Some(rest) = line.strip_prefix("> ") {
        return !looks_like_status_line(rest);
    }
    false
}

fn kimi_has_active_animation(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line = line.trim();
        let lowered = line.to_ascii_lowercase();
        kimi_moon_spinner(line)
            || (!lowered.starts_with("k2.7 code thinking")
                && (lowered.contains("thinking") || lowered.contains("working"))
                && line
                    .chars()
                    .next()
                    .is_some_and(|ch| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(ch)))
    })
}

fn kimi_moon_spinner(line: &str) -> bool {
    line.chars().count() == 1 && "🌑🌒🌓🌔🌕🌖🌗🌘".contains(line)
}

fn kimi_prompt_is_empty(line: &str) -> bool {
    let line = line.trim();
    line == ">"
        || line == "│ >"
        || line
            .strip_prefix("│ >")
            .is_some_and(|rest| rest.trim().is_empty())
}

fn looks_like_status_line(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.contains("running")
        || lowered.contains("thinking")
        || lowered.contains("context:")
        || lowered.contains("/model")
}
