use crate::domain::agent_detector::AgentDetector;
use crate::domain::session_catalog::ManagedSessionTaskState;

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
        // check argv for any supported wrapper
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

    fn detect_from_pane_text(
        &self,
        _current_command: &str,
        pane_text: &str,
    ) -> Option<&'static str> {
        let lowered = pane_text.to_ascii_lowercase();
        if lowered.contains("skip") && lowered.contains("codex") {
            return Some("codex");
        }
        if lowered.contains("type your message")
            || lowered.contains("send a message")
            || lowered.contains("openai codex")
        {
            return Some("codex");
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
        let last_line = normalized_lines.last().copied().unwrap_or_default();
        let last_lowered = last_line.to_ascii_lowercase();

        // Confirm — scan ALL non-empty lines for confirmation indicators.
        //
        // Codex's TUI uses a numbered menu for trust/confirm prompts:
        //   Do you trust the contents of this directory?
        //   › 1. Yes, continue
        //     2. No, quit
        //
        //   Press enter to continue
        // The › line contains "1." and is followed by "2." on the next line.
        //
        // Also check keywords across all lines since the prompt may appear
        // above an instruction or footer line, and the numbered menu may not
        // be rendered yet on the initial confirmation screen.
        for (i, line) in normalized_lines.iter().enumerate() {
            let lc = line.to_ascii_lowercase();
            if lc.contains("run this command")
                || lc.contains("allow this")
                || lc.contains("allow codex")
                || lc.ends_with("[y/n]")
                || lc.ends_with("(y/n)")
            {
                return Some(ManagedSessionTaskState::Confirm);
            }
            // TUI numbered menu: › 1. ..., next non-empty line starts with "2."
            if line.starts_with('›') && line.contains(" 1.") {
                if let Some(next) = normalized_lines.get(i + 1) {
                    if next.starts_with("2.") || next.starts_with("2 ") {
                        return Some(ManagedSessionTaskState::Confirm);
                    }
                }
            }
            // Dialog question starting with `?` (ratatui dialog marker).
            // On the initial confirmation screen, the numbered menu hasn't
            // rendered yet — only the `?` question and the `›` prompt are
            // visible. The `?` character at line start is a ratatui convention
            // for dialog/question state and is unlikely in regular output.
            //
            // Only match when `›` is empty (no user input yet). Once the user
            // starts typing at the prompt, the confirm screen has shifted and
            // Input detection should take over.
            if line.trim_start().starts_with('?') && i + 1 < normalized_lines.len() {
                let next = normalized_lines[i + 1];
                if next.starts_with('›') && next.trim_start_matches('›').trim().is_empty() {
                    return Some(ManagedSessionTaskState::Confirm);
                }
            }
        }

        // Input — find › in the LAST 5 non-empty lines. The actual prompt › is
        // always near the bottom of the pane. Conversation › lines (user's
        // echoed input) scroll up during execution and won't be in the last 5
        // lines after the agent has started producing output.
        //
        // Only count a › line as Input if:
        //   - The line contains ONLY "›" (empty prompt, no user text)
        //   - OR the › is in the last 3 lines and the next line is not a
        //     numbered option (numbered menus are caught by Confirm above)
        for (i, line) in normalized_lines.iter().enumerate() {
            if line.starts_with('›') && i >= normalized_lines.len().saturating_sub(5) {
                // Empty prompt (just "›") — definitely Input
                if line.trim_start_matches('›').trim().is_empty() {
                    return Some(ManagedSessionTaskState::Input);
                }
                // User has typed text at the prompt — must be in last 3 lines
                if i >= normalized_lines.len().saturating_sub(3) {
                    return Some(ManagedSessionTaskState::Input);
                }
            }
        }
        // Also check for ❯ prompt (shared TUI pattern) and keyword indicators.
        if last_line.starts_with('❯')
            || last_line.starts_with("> ")
            || last_lowered.contains("type your message")
            || last_lowered.contains("send a message")
            || last_lowered.contains("tip")
            || last_lowered.contains("ask codex")
        {
            return Some(ManagedSessionTaskState::Input);
        }

        Some(ManagedSessionTaskState::Running)
    }
}
