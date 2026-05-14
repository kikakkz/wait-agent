use crate::domain::agent_detector::AgentDetector;
use crate::domain::session_catalog::ManagedSessionTaskState;

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
        // check argv for any supported wrapper
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

    fn detect_from_pane_text(
        &self,
        _current_command: &str,
        pane_text: &str,
    ) -> Option<&'static str> {
        let lowered = pane_text.to_ascii_lowercase();
        if lowered.contains("claude") && lowered.contains("type your message") {
            return Some("claude");
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
        let last_line = normalized_lines.last().copied().unwrap_or_default();
        let lowered = last_line.to_ascii_lowercase();

        // Confirm — scan ALL non-empty lines for confirmation indicators.
        //
        // Claude Code's TUI shows a numbered menu when waiting for confirmation:
        //   Do you want to create claude_test_file.txt?
        //    ❯ 1. Yes
        //      2. No
        //
        //   Esc to cancel · Tab to amend
        // The ❯ line contains "1." and is followed by "2." on the next line,
        // distinguishing it from the Input prompt (❯ alone on its line between
        // separator lines).
        //
        // Also check keywords across all lines, since the confirm prompt may be
        // above a footer/instruction line.
        for (i, line) in normalized_lines.iter().enumerate() {
            let lc = line.to_ascii_lowercase();
            if lc.contains("run this command")
                || lc.contains("allow this")
                || lc.contains("approve this")
                || lc.ends_with("[y/n]")
                || lc.ends_with("(y/n)")
            {
                return Some(ManagedSessionTaskState::Confirm);
            }
            // TUI numbered menu: ❯ 1. ..., next non-empty line starts with "2."
            if line.starts_with('❯') && line.contains(" 1.") {
                if let Some(next) = normalized_lines.get(i + 1) {
                    if next.starts_with("2.") || next.starts_with("2 ") {
                        return Some(ManagedSessionTaskState::Confirm);
                    }
                }
            }
        }

        // Input detection.
        //
        // Claude Code's full-screen TUI places the prompt (❯) between two
        // separator lines (───), above a footer/status line. Conversation
        // ❯ lines (user's echoed input) are NOT followed by separators.
        //
        // During active execution the ❯ prompt is still visible in the TUI
        // but NOT actionable — in that case the temporal content-change check
        // in session_metadata.rs will override Input → Running above the
        // detector level.
        for (i, line) in normalized_lines.iter().enumerate() {
            if line.starts_with('❯') {
                if let Some(next) = normalized_lines.get(i + 1) {
                    if next.chars().all(|c| c == '─') {
                        return Some(ManagedSessionTaskState::Input);
                    }
                }
            }
        }
        // Also check `›` and keyword patterns on the last line for
        // non-TUI/legacy modes.
        if last_line.starts_with('›')
            || last_line.starts_with("> ")
            || lowered.contains("ready")
            || lowered.contains("type your message")
            || lowered.contains("send a message")
        {
            return Some(ManagedSessionTaskState::Input);
        }

        Some(ManagedSessionTaskState::Running)
    }
}
