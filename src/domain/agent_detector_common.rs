use crate::domain::session_catalog::ManagedSessionTaskState;

/// Agent-specific keyword tables used by the shared prompt/confirm scanner.
#[derive(Debug, Clone, Copy)]
pub struct AgentKeywords {
    /// Lower-cased phrases that indicate a confirmation dialog.
    pub confirm_phrases: &'static [&'static str],
    /// Lower-cased phrases that indicate an input prompt when no prompt
    /// character is visible.
    pub input_phrases: &'static [&'static str],
    /// Characters that mark a prompt line for this agent.
    pub prompt_chars: &'static [char],
    /// If `Some(n)`, only the last `n` lines are scanned for input prompts.
    /// If `None`, all non-empty lines are scanned.
    pub input_window: Option<usize>,
}

/// Returns true when `line` starts with one of the agent's prompt characters.
pub fn looks_like_prompt(line: &str, prompt_chars: &[char]) -> bool {
    let trimmed = line.trim_start();
    prompt_chars.iter().any(|&ch| trimmed.starts_with(ch))
}

/// Returns the portion of `line` after the leading prompt character, if any.
pub fn prompt_body<'a>(line: &'a str, prompt_chars: &[char]) -> &'a str {
    let trimmed = line.trim_start();
    for &ch in prompt_chars {
        if let Some(rest) = trimmed.strip_prefix(ch) {
            return rest;
        }
    }
    trimmed
}

/// Shared confirm detection: scans all non-empty lines for confirm phrases,
/// `[y/n]`/`(y/n)` suffixes, and the `?` + empty-prompt dialog pattern.
pub fn detect_confirm_from_lines(
    lines: &[&str],
    keywords: &AgentKeywords,
) -> Option<ManagedSessionTaskState> {
    for (i, line) in lines.iter().enumerate() {
        let lc = line.to_ascii_lowercase();
        if keywords
            .confirm_phrases
            .iter()
            .any(|phrase| lc.contains(phrase))
        {
            return Some(ManagedSessionTaskState::Confirm);
        }

        let trimmed = lc.trim();
        if trimmed.ends_with("[y/n]") || trimmed.ends_with("(y/n)") {
            return Some(ManagedSessionTaskState::Confirm);
        }

        if line.trim_start().starts_with('?') && i + 1 < lines.len() {
            let next = lines[i + 1];
            if looks_like_prompt(next, keywords.prompt_chars)
                && prompt_body(next, keywords.prompt_chars).trim().is_empty()
            {
                return Some(ManagedSessionTaskState::Confirm);
            }
        }
    }
    None
}

/// Shared input detection: scans prompt characters (optionally within a tail
/// window) and legacy keyword phrases on the last line.
pub fn detect_input_from_lines(
    lines: &[&str],
    keywords: &AgentKeywords,
) -> Option<ManagedSessionTaskState> {
    let window = keywords.input_window.unwrap_or(lines.len());
    let start = lines.len().saturating_sub(window);
    for line in lines.iter().skip(start) {
        if looks_like_prompt(line, keywords.prompt_chars) {
            return Some(ManagedSessionTaskState::Input);
        }
    }

    if let Some(last) = lines.last() {
        let lowered = last.to_ascii_lowercase();
        if keywords
            .input_phrases
            .iter()
            .any(|phrase| lowered.contains(phrase))
            || last.starts_with("> ")
        {
            return Some(ManagedSessionTaskState::Input);
        }
    }

    None
}

/// Convenience helper that returns `Confirm` or `Input` if the shared scanner
/// finds one, otherwise `None`. Each detector can then apply agent-specific
/// running markers before falling back to `Running`.
#[cfg(test)]
pub fn infer_task_state_from_lines(
    lines: &[&str],
    keywords: &AgentKeywords,
) -> Option<ManagedSessionTaskState> {
    detect_confirm_from_lines(lines, keywords).or_else(|| detect_input_from_lines(lines, keywords))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_keywords() -> AgentKeywords {
        AgentKeywords {
            confirm_phrases: &["run this command", "allow this", "approve this"],
            input_phrases: &["ready", "type your message", "send a message"],
            prompt_chars: &['❯', '›'],
            input_window: None,
        }
    }

    #[test]
    fn common_scanner_detects_prompt_characters() {
        assert!(looks_like_prompt("❯ ", &['❯', '›']));
        assert!(looks_like_prompt("  › text", &['❯', '›']));
        assert!(!looks_like_prompt("plain text", &['❯', '›']));
    }

    #[test]
    fn common_scanner_detects_confirm_phrases() {
        let lines = &["Do you want to run this command?"];
        assert_eq!(
            detect_confirm_from_lines(lines, &claude_keywords()),
            Some(ManagedSessionTaskState::Confirm)
        );
    }

    #[test]
    fn common_scanner_detects_yes_no_suffix() {
        let lines = &["Proceed? [y/n]"];
        assert_eq!(
            detect_confirm_from_lines(lines, &claude_keywords()),
            Some(ManagedSessionTaskState::Confirm)
        );
    }

    #[test]
    fn common_scanner_detects_dialog_question_with_empty_prompt() {
        let lines = &["? Delete file?", "❯"];
        assert_eq!(
            detect_confirm_from_lines(lines, &claude_keywords()),
            Some(ManagedSessionTaskState::Confirm)
        );
    }

    #[test]
    fn common_scanner_detects_input_from_prompt() {
        let lines = &["previous output", "❯ "];
        assert_eq!(
            infer_task_state_from_lines(lines, &claude_keywords()),
            Some(ManagedSessionTaskState::Input)
        );
    }

    #[test]
    fn common_scanner_detects_input_from_legacy_phrase() {
        let lines = &["Type your message:"];
        assert_eq!(
            infer_task_state_from_lines(lines, &claude_keywords()),
            Some(ManagedSessionTaskState::Input)
        );
    }

    #[test]
    fn common_scanner_respects_input_window() {
        let keywords = AgentKeywords {
            confirm_phrases: &[],
            input_phrases: &[],
            prompt_chars: &['›'],
            input_window: Some(2),
        };
        let lines = &["› old", "line", "line", "› new"];
        assert_eq!(
            detect_input_from_lines(lines, &keywords),
            Some(ManagedSessionTaskState::Input)
        );

        let lines = &["› old", "line", "line", "line", "plain"];
        assert_eq!(detect_input_from_lines(lines, &keywords), None);
    }
}
