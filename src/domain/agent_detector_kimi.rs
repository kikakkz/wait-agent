use crate::domain::agent_detector::{AgentDetector, InputStabilityPolicy};
use crate::domain::agent_signal::AgentStateEffect;
use crate::domain::session_catalog::ManagedSessionTaskState;
use serde_json::Value;

const KIMI_HOOK_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PermissionRequest",
    "PermissionResult",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
    "StopFailure",
    "Interrupt",
    "SessionEnd",
];

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
        if is_kimi_process_name(current_command) {
            return Some("kimi");
        }
        if let Some(argv) = argv {
            let is_kimi = argv.iter().any(|arg| {
                std::path::Path::new(arg)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(is_kimi_process_name)
            });
            if is_kimi {
                return Some("kimi");
            }
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

        if kimi_has_running_background_task(&normalized_lines) {
            return Some(ManagedSessionTaskState::Running);
        }

        for line in normalized_lines.iter().skip(last_lines_start) {
            if kimi_prompt_line(line) {
                return Some(ManagedSessionTaskState::Input);
            }
        }

        Some(ManagedSessionTaskState::Running)
    }

    fn input_stability_policy(
        &self,
        command_name: Option<&str>,
        pane_text: &str,
    ) -> Option<InputStabilityPolicy> {
        if command_name.unwrap_or_default() != "kimi" {
            return None;
        }
        if kimi_has_stable_input_prompt(pane_text) {
            Some(InputStabilityPolicy::Immediate)
        } else {
            Some(InputStabilityPolicy::StableContent)
        }
    }

    fn matches_agent_signal(&self, agent: &str, command_name: &str) -> bool {
        agent == "kimi" && (command_name == "kimi" || command_name == "claude")
    }

    fn hook_events(&self) -> &'static [&'static str] {
        KIMI_HOOK_EVENTS
    }

    fn signal_state_effect(&self, event: &str, payload: &Value) -> Option<AgentStateEffect> {
        let state = match event {
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
                ManagedSessionTaskState::Running
            }
            "PermissionRequest" => ManagedSessionTaskState::Confirm,
            "PermissionResult" => permission_result_state(payload),
            "Stop" | "StopFailure" | "Interrupt" => ManagedSessionTaskState::Input,
            "SessionEnd" => return Some(AgentStateEffect::Clear),
            _ => return None,
        };
        Some(AgentStateEffect::Set(state))
    }
}

fn permission_result_state(payload: &Value) -> ManagedSessionTaskState {
    let lowered = payload.to_string().to_ascii_lowercase();
    if lowered.contains("deny")
        || lowered.contains("denied")
        || lowered.contains("reject")
        || lowered.contains("\"approved\":false")
        || lowered.contains("\"allow\":false")
    {
        ManagedSessionTaskState::Input
    } else {
        ManagedSessionTaskState::Running
    }
}

fn is_kimi_process_name(name: &str) -> bool {
    matches!(name, "kimi" | "kimi.js" | "kimi-code")
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
    line.chars()
        .next()
        .is_some_and(|ch| "🌑🌒🌓🌔🌕🌖🌗🌘".contains(ch))
}

fn kimi_prompt_is_empty(line: &str) -> bool {
    let line = line.trim();
    line == ">"
        || line == "│ >"
        || line
            .strip_prefix("│ >")
            .is_some_and(|rest| rest.trim().is_empty())
}

fn kimi_has_running_background_task(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let mut rest = *line;
        while let Some((_, after_open)) = rest.split_once('[') {
            let Some((status, after_close)) = after_open.split_once(']') else {
                return false;
            };
            if kimi_background_task_status_is_running(status) {
                return true;
            }
            rest = after_close;
        }
        false
    })
}

fn kimi_background_task_status_is_running(status: &str) -> bool {
    let lowered = status.trim().to_ascii_lowercase();
    if lowered == "1 task running" {
        return true;
    }
    lowered
        .strip_suffix(" tasks running")
        .and_then(|count| count.trim().parse::<usize>().ok())
        .is_some_and(|count| count > 0)
}

fn looks_like_status_line(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.contains("running")
        || lowered.contains("thinking")
        || lowered.contains("context:")
        || lowered.contains("/model")
}

fn kimi_has_stable_input_prompt(pane_text: &str) -> bool {
    let normalized_lines = pane_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let recent_start = normalized_lines.len().saturating_sub(8);
    normalized_lines
        .iter()
        .skip(recent_start)
        .any(|line| kimi_prompt_line(line))
}
