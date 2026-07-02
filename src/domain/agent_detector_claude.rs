use crate::domain::agent_detector::{AgentDetector, InputStabilityPolicy};
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
        // above a footer/instruction line, and the numbered menu may not be
        // rendered yet in some TUI states.
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
            // Dialog question starting with `?` (ratatui dialog marker).
            // On the initial confirmation screen, the numbered menu hasn't
            // rendered yet — only the `?` question and the `❯` prompt are
            // visible. The `?` character at line start is a ratatui convention
            // for dialog/question state and is unlikely in regular output.
            //
            // Only match when `❯`/`›` is empty (no user input yet). Once the
            // user starts typing, Input detection should take over.
            if line.trim_start().starts_with('?') && i + 1 < normalized_lines.len() {
                let next = normalized_lines[i + 1];
                if (next.starts_with('❯') || next.starts_with('›'))
                    && next.trim_start_matches(&['❯', '›'][..]).trim().is_empty()
                {
                    return Some(ManagedSessionTaskState::Confirm);
                }
            }
        }

        // Input detection.
        //
        // Any non-empty line starting with the prompt character (❯ or ›)
        // indicates the agent is awaiting input. During active execution
        // the ❯ prompt is still visible in the TUI, but the temporal
        // content-change check in session_metadata.rs overrides Input →
        // Running above the detector level, so being permissive here is
        // safe — the temporal check will correct it when content is
        // actively changing.
        //
        // Previously this required ❯ to be followed by a separator line
        // (───) or to be the last visible line, but that missed cases where
        // the bottom separator was scrolled off-screen or a footer line
        // appeared below ❯.
        for line in &normalized_lines {
            if line.starts_with('❯') || line.starts_with('›') {
                return Some(ManagedSessionTaskState::Input);
            }
        }
        // Legacy keyword-based fallback (no prompt character visible)
        if last_line.starts_with("> ")
            || lowered.contains("ready")
            || lowered.contains("type your message")
            || lowered.contains("send a message")
        {
            return Some(ManagedSessionTaskState::Input);
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
