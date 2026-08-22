use crate::domain::agent_detector::{AgentDetector, InputStabilityPolicy};
use crate::domain::agent_detector_common::{
    detect_confirm_from_lines, detect_input_from_lines, is_user_question_tool, AgentKeywords,
};
use crate::domain::agent_signal::AgentStateEffect;
use crate::domain::session_catalog::ManagedSessionTaskState;
use serde_json::Value;

const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "PostToolBatch",
    "Notification",
    "Stop",
    "SessionEnd",
];

const CLAUDE_KEYWORDS: AgentKeywords = AgentKeywords {
    confirm_phrases: &["run this command", "allow this", "approve this"],
    input_phrases: &["ready", "type your message", "send a message"],
    prompt_chars: &['❯', '›'],
    input_window: None,
};

pub struct ClaudeDetector;

impl AgentDetector for ClaudeDetector {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn detect_from_process(
        &self,
        current_command: &str,
        argv: Option<&[String]>,
    ) -> Option<&'static str> {
        if current_command == "claude" || current_command == "claude.js" {
            return Some("claude");
        }
        if let Some(argv) = argv {
            let is_claude = argv.first().and_then(|arg| {
                std::path::Path::new(arg)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
            }) == Some("claude")
                || argv.iter().skip(1).any(|arg| {
                    std::path::Path::new(arg)
                        .file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        == Some("claude")
                        || std::path::Path::new(arg)
                            .file_name()
                            .and_then(std::ffi::OsStr::to_str)
                            == Some("claude.js")
                });
            if is_claude {
                return Some("claude");
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
        if command_name != "claude" {
            return None;
        }
        let normalized_lines: Vec<&str> = pane_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();

        // 1. Shared confirm scanner.
        if let Some(state) = detect_confirm_from_lines(&normalized_lines, &CLAUDE_KEYWORDS) {
            return Some(state);
        }

        // 2. Claude-specific numbered menu confirmation.
        //    ❯ 1. Yes
        //      2. No
        for (i, line) in normalized_lines.iter().enumerate() {
            if line.starts_with('❯') && line.contains(" 1.") {
                if let Some(next) = normalized_lines.get(i + 1) {
                    if next.starts_with("2.") || next.starts_with("2 ") {
                        return Some(ManagedSessionTaskState::Confirm);
                    }
                }
            }
        }

        // 3. Shared input scanner.
        if let Some(state) = detect_input_from_lines(&normalized_lines, &CLAUDE_KEYWORDS) {
            return Some(state);
        }

        Some(ManagedSessionTaskState::Running)
    }

    fn input_stability_policy(
        &self,
        command_name: Option<&str>,
        pane_text: &str,
    ) -> Option<InputStabilityPolicy> {
        if command_name.unwrap_or_default() != "claude" {
            return None;
        }
        if claude_has_stable_input_prompt(pane_text) {
            Some(InputStabilityPolicy::Immediate)
        } else {
            Some(InputStabilityPolicy::StableContent)
        }
    }

    fn hook_events(&self) -> &'static [&'static str] {
        CLAUDE_HOOK_EVENTS
    }

    fn signal_state_effect(&self, event: &str, payload: &Value) -> Option<AgentStateEffect> {
        let state = match event {
            "PreToolUse" => {
                if is_user_question_tool(payload, &["AskUserQuestion"]) {
                    ManagedSessionTaskState::Confirm
                } else {
                    ManagedSessionTaskState::Running
                }
            }
            "UserPromptSubmit" | "PostToolUse" | "PostToolBatch" => {
                ManagedSessionTaskState::Running
            }
            "PermissionRequest" => ManagedSessionTaskState::Confirm,
            "Notification" if notification_mentions_permission(payload) => {
                ManagedSessionTaskState::Confirm
            }
            "Stop" => ManagedSessionTaskState::Input,
            "SessionEnd" => return Some(AgentStateEffect::Clear),
            _ => return None,
        };
        Some(AgentStateEffect::Set(state))
    }
}

fn notification_mentions_permission(payload: &Value) -> bool {
    let lowered = payload.to_string().to_ascii_lowercase();
    lowered.contains("permission")
        || lowered.contains("approve")
        || lowered.contains("approval")
        || lowered.contains("allow")
}

fn claude_has_stable_input_prompt(pane_text: &str) -> bool {
    let lines = pane_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let recent_start = lines.len().saturating_sub(3);
    lines.iter().skip(recent_start).any(|line| {
        let after_claude_prompt = line.trim_start_matches('❯').trim_start();
        line.starts_with('❯') && !after_claude_prompt.starts_with("1.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session_catalog::ManagedSessionTaskState;

    #[test]
    fn detects_input_state_from_prompt_line() {
        let detector = ClaudeDetector;
        let pane_text = "previous output\n❯ ";
        assert_eq!(
            detector.infer_task_state(Some("claude"), pane_text),
            Some(ManagedSessionTaskState::Input)
        );
    }

    #[test]
    fn detects_running_state_from_output() {
        let detector = ClaudeDetector;
        let pane_text = "Computing result...\nHere is the answer";
        assert_eq!(
            detector.infer_task_state(Some("claude"), pane_text),
            Some(ManagedSessionTaskState::Running)
        );
    }

    #[test]
    fn ignores_irrelevant_lines() {
        let detector = ClaudeDetector;
        assert_eq!(
            detector.infer_task_state(Some("bash"), "❯ "),
            None,
            "detector should ignore lines when command is not claude"
        );
        assert_eq!(
            detector.detect_from_process("bash", None),
            None,
            "detector should ignore non-claude processes"
        );
    }

    #[test]
    fn numbered_menu_is_confirm() {
        let detector = ClaudeDetector;
        let pane_text = "Create file?\n❯ 1. Yes\n  2. No";
        assert_eq!(
            detector.infer_task_state(Some("claude"), pane_text),
            Some(ManagedSessionTaskState::Confirm)
        );
    }

    #[test]
    fn pre_tool_use_for_ask_user_question_sets_confirm() {
        let detector = ClaudeDetector;
        let payload = serde_json::json!({
            "tool_name": "AskUserQuestion",
            "tool_input": { "questions": [] },
        });
        assert_eq!(
            detector.signal_state_effect("PreToolUse", &payload),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Confirm))
        );
    }

    #[test]
    fn pre_tool_use_for_other_tool_sets_running() {
        let detector = ClaudeDetector;
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "ls" },
        });
        assert_eq!(
            detector.signal_state_effect("PreToolUse", &payload),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
    }

    #[test]
    fn post_tool_use_clears_to_running() {
        let detector = ClaudeDetector;
        let payload = serde_json::json!({ "tool_name": "AskUserQuestion" });
        assert_eq!(
            detector.signal_state_effect("PostToolUse", &payload),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
    }
}
