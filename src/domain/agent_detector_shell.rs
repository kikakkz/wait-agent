use crate::domain::agent_detector::{AgentDetector, SHELL_NAMES};
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
}
