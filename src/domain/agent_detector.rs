use crate::domain::session_catalog::ManagedSessionTaskState;
use std::fmt;

/// Shared list of shell program names used across agent detection and overlay logic.
pub const SHELL_NAMES: &[&str] = &["bash", "zsh", "fish", "sh"];

/// A detector that identifies a specific agent (Claude, Codex, etc.)
/// and infers its task state from process info and pane text.
pub trait AgentDetector: Send + Sync {
    /// Human-readable agent name (e.g. "claude", "codex", "shell").
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    /// Agent-specific process-name detection.
    /// Returns Some(agent_name) if the running process belongs to this agent.
    fn detect_from_process(
        &self,
        current_command: &str,
        argv: Option<&[String]>,
    ) -> Option<&'static str>;

    /// Infer the agent-specific task state from command name and pane text.
    /// Returns None if this detector does not handle the given context.
    fn infer_task_state(
        &self,
        command_name: Option<&str>,
        pane_text: &str,
    ) -> Option<ManagedSessionTaskState>;
}

/// Registry of all known agent detectors.
/// Dispatches detection queries to registered detectors in priority order.
pub struct DetectorRegistry {
    /// Priority-ordered detectors. Earlier entries take precedence.
    detectors: Vec<Box<dyn AgentDetector>>,
}

impl fmt::Debug for DetectorRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DetectorRegistry")
            .field("detectors", &self.detectors.len())
            .finish()
    }
}

impl PartialEq for DetectorRegistry {
    fn eq(&self, other: &Self) -> bool {
        self.detectors.len() == other.detectors.len()
    }
}

impl Eq for DetectorRegistry {}

impl DetectorRegistry {
    pub fn new() -> Self {
        Self {
            detectors: Vec::new(),
        }
    }

    pub fn register(&mut self, detector: Box<dyn AgentDetector>) {
        self.detectors.push(detector);
    }

    /// Runs process-level detection across all registered detectors.
    /// Highest-priority (first) match wins.
    pub fn detect_from_process(
        &self,
        current_command: &str,
        argv: Option<&[String]>,
    ) -> Option<&'static str> {
        for detector in &self.detectors {
            if let Some(name) = detector.detect_from_process(current_command, argv) {
                return Some(name);
            }
        }
        None
    }

    /// Full detection: process identity only, then normalize.
    /// Returns the normalized command name.
    pub fn detect_command_name(
        &self,
        current_command: &str,
        argv: Option<&[String]>,
        _pane_text: &str,
    ) -> String {
        // 1. Process-level detection
        if let Some(name) = self.detect_from_process(current_command, argv) {
            return name.to_string();
        }
        // 2. Fall back to the foreground command name. Pane scrollback is
        // historical output and must not define the current session identity.
        current_command.to_string()
    }

    /// Infer task state using registered detectors.
    /// Process/text detection is not re-run here; the caller provides the resolved command_name.
    pub fn infer_task_state(
        &self,
        command_name: Option<&str>,
        pane_text: &str,
    ) -> ManagedSessionTaskState {
        let normalized_lines: Vec<&str> = pane_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();

        if normalized_lines.is_empty() {
            return ManagedSessionTaskState::Unknown;
        }

        // 1. Check Confirm on the last non-empty line only, using structural
        //    choice indicators. Generic keywords (approve, confirm, allow) are
        //    too broad for automated pane-text scanning and we avoid them here.
        //    Agent-specific confirm detection lives in each detector's infer_task_state.
        {
            let last = normalized_lines
                .last()
                .copied()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let trimmed = last.trim();
            if trimmed.ends_with("[y/n]")
                || trimmed.ends_with("(y/n)")
                || trimmed.ends_with("[yes/no]")
                || trimmed.ends_with("(yes/no)")
                || trimmed.ends_with("[y/n]?")
                || trimmed.ends_with("(y/n)?")
            {
                return ManagedSessionTaskState::Confirm;
            }
        }

        let command_name = command_name.unwrap_or_default();

        // 2. Try each detector's task state inference
        for detector in &self.detectors {
            if let Some(state) = detector.infer_task_state(command_name.into(), pane_text) {
                return state;
            }
        }

        // 3. Fallback to Running
        ManagedSessionTaskState::Running
    }

    /// Returns true if the given name is a known shell program.
    #[allow(dead_code)]
    pub fn is_shell_name(&self, name: &str) -> bool {
        SHELL_NAMES.contains(&name)
    }
}

impl Default for DetectorRegistry {
    fn default() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(super::agent_detector_claude::ClaudeDetector));
        registry.register(Box::new(super::agent_detector_codex::CodexDetector));
        registry.register(Box::new(super::agent_detector_kimi::KimiDetector));
        registry.register(Box::new(super::agent_detector_shell::ShellDetector));
        registry
    }
}
