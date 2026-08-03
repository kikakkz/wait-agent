use crate::domain::agent_detector::{AgentDetector, InputStabilityPolicy, SHELL_NAMES};
use crate::domain::session_catalog::ManagedSessionTaskState;

/// Default/fallback detector for plain shell sessions (bash, zsh, fish, sh).
pub struct ShellDetector;

impl AgentDetector for ShellDetector {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn detect_from_process(
        &self,
        _current_command: &str,
        _argv: Option<&[String]>,
    ) -> Option<&'static str> {
        None
    }

    fn infer_task_state(
        &self,
        command_name: Option<&str>,
        pane_text: &str,
    ) -> Option<ManagedSessionTaskState> {
        let command_name = command_name.unwrap_or_default();
        if !SHELL_NAMES.contains(&command_name) && !command_name.is_empty() {
            return None;
        }
        let normalized_lines: Vec<&str> = pane_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if normalized_lines.is_empty() {
            return None;
        }
        let last_line = normalized_lines.last().copied().unwrap_or_default();
        let last_char = last_line.chars().last();
        if matches!(
            last_char,
            Some('$' | '#' | '%' | '❯' | '›' | '➜' | 'λ' | '»')
        ) {
            return Some(ManagedSessionTaskState::Input);
        }
        // Also detect common prompt patterns like "user@host:path$ " or just a colon at end
        if last_line.contains("❯") || last_line.starts_with("➜") {
            return Some(ManagedSessionTaskState::Input);
        }
        Some(ManagedSessionTaskState::Running)
    }

    fn input_stability_policy(
        &self,
        command_name: Option<&str>,
        _pane_text: &str,
    ) -> Option<InputStabilityPolicy> {
        let command_name = command_name.unwrap_or_default();
        if SHELL_NAMES.contains(&command_name) || command_name.is_empty() {
            Some(InputStabilityPolicy::Immediate)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session_catalog::ManagedSessionTaskState;

    #[test]
    fn detects_input_state_from_prompt_line() {
        let detector = ShellDetector;
        assert_eq!(
            detector.infer_task_state(Some("bash"), "user@host:~$ "),
            Some(ManagedSessionTaskState::Input)
        );
        assert_eq!(
            detector.infer_task_state(Some("zsh"), "~ % "),
            Some(ManagedSessionTaskState::Input)
        );
    }

    #[test]
    fn detects_running_state_from_output() {
        let detector = ShellDetector;
        let pane_text = "Compiling project...\nDone";
        assert_eq!(
            detector.infer_task_state(Some("bash"), pane_text),
            Some(ManagedSessionTaskState::Running)
        );
    }

    #[test]
    fn ignores_irrelevant_lines() {
        let detector = ShellDetector;
        assert_eq!(
            detector.infer_task_state(Some("claude"), "$ "),
            None,
            "detector should ignore lines when command is not a shell"
        );
        assert_eq!(
            detector.detect_from_process("claude", None),
            None,
            "detector never matches a foreground process"
        );
    }
}
