use crate::domain::agent_signal::AgentStateEffect;
use crate::domain::interpreter_command_name_resolver::InterpreterCommandNameResolver;
use crate::domain::session_catalog::ManagedSessionTaskState;
use serde_json::Value;
use std::fmt;

/// Shared list of shell program names used across agent detection and overlay logic.
pub const SHELL_NAMES: &[&str] = &["bash", "zsh", "fish", "sh"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputStabilityPolicy {
    Immediate,
    StableContent,
}

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

    /// Agent-specific policy for detector-reported Input states.
    ///
    /// Agents with a reliable input boundary can publish Input immediately.
    /// Agents whose prompt is visible while output is still streaming can ask
    /// the tmux metadata layer to wait until content above the prompt is stable.
    fn input_stability_policy(
        &self,
        _command_name: Option<&str>,
        _pane_text: &str,
    ) -> Option<InputStabilityPolicy> {
        None
    }

    /// Agent-specific hook compatibility. Most agents only match their own
    /// command name; wrappers can opt into aliases here.
    fn matches_agent_signal(&self, agent: &str, command_name: &str) -> bool {
        agent == self.name() && command_name == self.name()
    }

    /// Agent-specific hook events that waitagent should install for this agent.
    fn hook_events(&self) -> &'static [&'static str] {
        &[]
    }

    /// Agent-specific hook event reducer.
    fn signal_state_effect(&self, _event: &str, _payload: &Value) -> Option<AgentStateEffect> {
        None
    }
}

/// Registry of all known agent detectors.
/// Dispatches detection queries to registered detectors in priority order.
pub struct DetectorRegistry {
    /// Priority-ordered detectors. Earlier entries take precedence.
    detectors: Vec<Box<dyn AgentDetector>>,
    /// Resolves display names for processes launched through interpreters
    /// or wrapper scripts (e.g. `python script.py`, pip console scripts).
    interpreter_resolver: InterpreterCommandNameResolver,
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
            interpreter_resolver: InterpreterCommandNameResolver::new(),
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
        // 1. Process-level detection for known agents.
        if let Some(name) = self.detect_from_process(current_command, argv) {
            return name.to_string();
        }
        // 2. Interpreter/wrapper script target name extraction.
        if let Some(name) = self.interpreter_resolver.resolve(current_command, argv) {
            return name;
        }
        // 3. Fall back to the foreground command name. Pane scrollback is
        // historical output and must not define the current session identity.
        current_command.to_string()
    }

    pub fn detect_command_name_from_argv_candidates(
        &self,
        current_command: &str,
        argv_candidates: &[Vec<String>],
        pane_text: &str,
    ) -> String {
        for argv in argv_candidates {
            if let Some(name) = self.detect_from_process(current_command, Some(argv.as_slice())) {
                return name.to_string();
            }
            if let Some(name) = self
                .interpreter_resolver
                .resolve(current_command, Some(argv.as_slice()))
            {
                return name;
            }
        }
        self.detect_command_name(
            current_command,
            argv_candidates.first().map(Vec::as_slice),
            pane_text,
        )
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

    pub fn input_stability_policy(
        &self,
        command_name: Option<&str>,
        pane_text: &str,
    ) -> InputStabilityPolicy {
        for detector in &self.detectors {
            if let Some(policy) = detector.input_stability_policy(command_name, pane_text) {
                return policy;
            }
        }
        InputStabilityPolicy::StableContent
    }

    pub fn agent_signal_matches_command(&self, agent: &str, command_name: &str) -> bool {
        if agent == command_name {
            return true;
        }
        self.detectors
            .iter()
            .any(|detector| detector.matches_agent_signal(agent, command_name))
    }

    pub fn hook_events_for_agent(&self, agent: &str) -> Option<&'static [&'static str]> {
        self.detectors
            .iter()
            .find(|detector| detector.name() == agent)
            .map(|detector| detector.hook_events())
    }

    pub fn signal_state_effect(
        &self,
        agent: &str,
        event: &str,
        payload: &Value,
    ) -> Option<AgentStateEffect> {
        self.detectors
            .iter()
            .find(|detector| detector.name() == agent)
            .and_then(|detector| detector.signal_state_effect(event, payload))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_routes_codex_signal_lifecycle_to_codex_detector() {
        let registry = DetectorRegistry::default();

        assert!(registry
            .hook_events_for_agent("codex")
            .is_some_and(|events| events.contains(&"Interrupt")));
        assert_eq!(
            registry.signal_state_effect("codex", "UserPromptSubmit", &Value::Null),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
        assert_eq!(
            registry.signal_state_effect("codex", "PermissionRequest", &Value::Null),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Confirm))
        );
        assert_eq!(
            registry.signal_state_effect("codex", "Interrupt", &Value::Null),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Input))
        );
        assert_eq!(
            registry.signal_state_effect("codex", "SessionStart", &Value::Null),
            None
        );
    }

    #[test]
    fn registry_routes_agent_specific_signal_lifecycle() {
        let registry = DetectorRegistry::default();

        assert_eq!(
            registry.signal_state_effect(
                "kimi",
                "PermissionResult",
                &serde_json::json!({"approved": false}),
            ),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Input))
        );
        assert_eq!(
            registry.signal_state_effect(
                "claude",
                "Notification",
                &serde_json::json!({"message": "needs permission"}),
            ),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Confirm))
        );
        assert_eq!(
            registry.signal_state_effect("claude", "SessionEnd", &Value::Null),
            Some(AgentStateEffect::Clear)
        );
    }

    #[test]
    fn registry_extracts_pip_console_script_name() {
        let registry = DetectorRegistry::default();
        let argv = vec!["/home/user/.local/bin/alter".to_string()];
        assert_eq!(
            registry.detect_command_name("python", Some(&argv), ""),
            "alter"
        );
    }

    #[test]
    fn registry_extracts_python_script_name() {
        let registry = DetectorRegistry::default();
        let argv = vec!["python3".to_string(), "/path/to/script.py".to_string()];
        assert_eq!(
            registry.detect_command_name("python3", Some(&argv), ""),
            "script.py"
        );
    }

    #[test]
    fn registry_extracts_python_module_name() {
        let registry = DetectorRegistry::default();
        let argv = vec![
            "python".to_string(),
            "-m".to_string(),
            "my_module".to_string(),
        ];
        assert_eq!(
            registry.detect_command_name("python", Some(&argv), ""),
            "my_module"
        );
    }

    #[test]
    fn registry_extracts_node_script_name() {
        let registry = DetectorRegistry::default();
        let argv = vec!["node".to_string(), "/path/to/app.js".to_string()];
        assert_eq!(
            registry.detect_command_name("node", Some(&argv), ""),
            "app.js"
        );
    }

    #[test]
    fn registry_falls_back_to_current_command_for_plain_repl() {
        let registry = DetectorRegistry::default();
        let argv = vec!["python3".to_string()];
        assert_eq!(
            registry.detect_command_name("python3", Some(&argv), ""),
            "python3"
        );
    }

    #[test]
    fn registry_extracts_from_argv_candidates() {
        let registry = DetectorRegistry::default();
        let candidates = vec![vec!["/home/user/.local/bin/alter".to_string()]];
        assert_eq!(
            registry.detect_command_name_from_argv_candidates("python", &candidates, ""),
            "alter"
        );
    }
}
