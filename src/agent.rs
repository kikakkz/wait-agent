use std::path::Path;

pub fn live_agent_label(command_or_program: &str) -> Option<String> {
    let first = command_or_program
        .split_whitespace()
        .next()
        .unwrap_or_default();
    let name = Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first);
    matches!(name, "codex" | "claude" | "claude-code" | "kilo").then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::live_agent_label;

    #[test]
    fn matches_live_agents_by_program_basename() {
        assert_eq!(
            live_agent_label("/tmp/codex --model gpt-5.4"),
            Some("codex".to_string())
        );
        assert_eq!(
            live_agent_label("claude-code --dangerously-skip-permissions"),
            Some("claude-code".to_string())
        );
        assert_eq!(live_agent_label("bash -lc pwd"), None);
    }
}
