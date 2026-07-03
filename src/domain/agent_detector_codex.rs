use crate::domain::agent_detector::{AgentDetector, InputStabilityPolicy};
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
                || lc.contains("do you trust the contents of this directory")
                || lc.contains("hooks need review")
                || lc.ends_with("[y/n]")
                || lc.ends_with("(y/n)")
            {
                return Some(ManagedSessionTaskState::Confirm);
            }
            // TUI numbered menu: the selected line starts with `› N.` and
            // another numbered option appears nearby. Options may wrap across
            // lines, so the next option is not always the immediate next line.
            if codex_numbered_menu_selection(&normalized_lines, i) {
                return Some(ManagedSessionTaskState::Confirm);
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

        if codex_has_active_work_marker(&normalized_lines) {
            return Some(ManagedSessionTaskState::Running);
        }

        // Input — find › near the pane tail. The actual prompt › is
        // always near the bottom of the pane. Conversation › lines (user's
        // echoed input) scroll up during execution and won't be near the tail
        // lines after the agent has started producing output.
        //
        // Only count a › line as Input if:
        //   - The line contains ONLY "›" (empty prompt, no user text)
        //   - OR the › is near the pane tail and the next line is not a
        //     numbered option (numbered menus are caught by Confirm above)
        for (i, line) in normalized_lines.iter().enumerate() {
            if line.starts_with('›') && i >= normalized_lines.len().saturating_sub(12) {
                // Empty prompt (just "›") — definitely Input
                if line.trim_start_matches('›').trim().is_empty() {
                    return Some(ManagedSessionTaskState::Input);
                }
                // User has typed text at the prompt.
                if i >= normalized_lines.len().saturating_sub(12) {
                    return Some(ManagedSessionTaskState::Input);
                }
            }
        }
        // Also check for ❯ prompt (shared TUI pattern) and keyword indicators.
        if last_line.starts_with('❯')
            || last_line.starts_with("> ")
            || last_lowered.contains("type your message")
            || last_lowered.contains("send a message")
        // NOT checking `tip` or `ask codex` here — those keywords appear
        // in codex's running output and would cause false Input when the
        // prompt character (`›`) isn't visible. The temporal content-change
        // check in session_metadata.rs distinguishes "awaiting input" from
        // "running with visible output."
        {
            return Some(ManagedSessionTaskState::Input);
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
