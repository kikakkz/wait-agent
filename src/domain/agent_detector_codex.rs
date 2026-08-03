use crate::domain::agent_detector::{AgentDetector, InputStabilityPolicy};
use crate::domain::agent_detector_common::{
    detect_confirm_from_lines, detect_input_from_lines, AgentKeywords,
};
use crate::domain::agent_signal::AgentStateEffect;
use crate::domain::session_catalog::ManagedSessionTaskState;
use serde_json::Value;

const CODEX_HOOK_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "Interrupt",
];

const CODEX_KEYWORDS: AgentKeywords = AgentKeywords {
    confirm_phrases: &[
        "run this command",
        "allow this",
        "allow codex",
        "do you trust the contents of this directory",
        "hooks need review",
    ],
    input_phrases: &["type your message", "send a message"],
    prompt_chars: &['›', '❯'],
    input_window: Some(12),
};

pub struct CodexDetector;

impl AgentDetector for CodexDetector {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn detect_from_process(
        &self,
        current_command: &str,
        argv: Option<&[String]>,
    ) -> Option<&'static str> {
        if current_command == "codex" || current_command == "codex.js" {
            return Some("codex");
        }
        if let Some(argv) = argv {
            let is_codex = argv.first().and_then(|arg| {
                std::path::Path::new(arg)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
            }) == Some("codex")
                || argv.iter().skip(1).any(|arg| {
                    std::path::Path::new(arg)
                        .file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        == Some("codex")
                        || std::path::Path::new(arg)
                            .file_name()
                            .and_then(std::ffi::OsStr::to_str)
                            == Some("codex.js")
                });
            if is_codex {
                return Some("codex");
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
        if command_name != "codex" {
            return None;
        }
        let normalized_lines: Vec<&str> = pane_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();

        // 1. Shared confirm scanner.
        if let Some(state) = detect_confirm_from_lines(&normalized_lines, &CODEX_KEYWORDS) {
            return Some(state);
        }

        // 2. Codex-specific numbered menu confirmation.
        for (i, _line) in normalized_lines.iter().enumerate() {
            if codex_numbered_menu_selection(&normalized_lines, i) {
                return Some(ManagedSessionTaskState::Confirm);
            }
        }

        // 3. Active-work marker must take precedence over input prompts.
        if codex_has_active_work_marker(&normalized_lines) {
            return Some(ManagedSessionTaskState::Running);
        }

        // 4. Shared input scanner.
        if let Some(state) = detect_input_from_lines(&normalized_lines, &CODEX_KEYWORDS) {
            return Some(state);
        }

        Some(ManagedSessionTaskState::Running)
    }

    fn input_stability_policy(
        &self,
        command_name: Option<&str>,
        _pane_text: &str,
    ) -> Option<InputStabilityPolicy> {
        if command_name.unwrap_or_default() == "codex" {
            Some(InputStabilityPolicy::Immediate)
        } else {
            None
        }
    }

    fn hook_events(&self) -> &'static [&'static str] {
        CODEX_HOOK_EVENTS
    }

    fn signal_state_effect(&self, event: &str, _payload: &Value) -> Option<AgentStateEffect> {
        let state = match event {
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => ManagedSessionTaskState::Running,
            "PermissionRequest" => ManagedSessionTaskState::Confirm,
            "Stop" | "Interrupt" => ManagedSessionTaskState::Input,
            _ => return None,
        };
        Some(AgentStateEffect::Set(state))
    }
}

fn codex_has_active_work_marker(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let lc = line.to_ascii_lowercase();
        lc == "working"
            || lc.starts_with("working ")
            || lc.starts_with("working...")
            || lc.starts_with("• working")
            || lc.starts_with("codex is working")
            || lc.contains("esc to interrupt")
            || lc.contains("ctrl-c to interrupt")
    })
}

fn codex_numbered_menu_selection(lines: &[&str], selected_index: usize) -> bool {
    let selected_number = lines
        .get(selected_index)
        .and_then(|line| line.strip_prefix('›'))
        .and_then(parse_numbered_option);
    let Some(selected_number) = selected_number else {
        return false;
    };

    let start = selected_index.saturating_sub(4);
    let end = (selected_index + 5).min(lines.len());
    lines[start..end].iter().enumerate().any(|(offset, line)| {
        start + offset != selected_index
            && parse_numbered_option(line).is_some_and(|number| number != selected_number)
    })
}

fn parse_numbered_option(line: &str) -> Option<u32> {
    let trimmed = line.trim_start();
    let digit_end = trimmed
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .last()
        .map(|(index, ch)| index + ch.len_utf8())?;
    let number = trimmed[..digit_end].parse::<u32>().ok()?;
    let rest = trimmed[digit_end..].trim_start();
    if rest.starts_with('.') || rest.starts_with(')') {
        Some(number)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session_catalog::ManagedSessionTaskState;

    #[test]
    fn detects_input_state_from_prompt_line() {
        let detector = CodexDetector;
        let pane_text = "previous output\n› ";
        assert_eq!(
            detector.infer_task_state(Some("codex"), pane_text),
            Some(ManagedSessionTaskState::Input)
        );
    }

    #[test]
    fn detects_running_state_from_output() {
        let detector = CodexDetector;
        let pane_text = "working\nGenerating code...";
        assert_eq!(
            detector.infer_task_state(Some("codex"), pane_text),
            Some(ManagedSessionTaskState::Running)
        );
    }

    #[test]
    fn ignores_irrelevant_lines() {
        let detector = CodexDetector;
        assert_eq!(
            detector.infer_task_state(Some("bash"), "› "),
            None,
            "detector should ignore lines when command is not codex"
        );
        assert_eq!(
            detector.detect_from_process("bash", None),
            None,
            "detector should ignore non-codex processes"
        );
    }
}
