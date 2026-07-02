use crate::domain::agent_detector::InputStabilityPolicy;
use crate::domain::session_catalog::ManagedSessionTaskState;
use crate::infra::tmux_error::{parse_tmux_id, TmuxError};
use crate::infra::tmux_types::{TmuxPaneId, TmuxPaneInfo, TmuxSocketName};
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use super::EmbeddedTmuxBackend;

// When the content above the prompt separator changes between polls,
// the agent is Running, even if the prompt character is visible.
//
// The stable_count tracks consecutive polls with matching content.
// Content must be stable for STABLE_THRESHOLD polls before the state
// transitions from Running → Input. This adds hysteresis so brief
// pauses during streaming output don't cause I/R flickering.
const STABLE_THRESHOLD: u8 = 3;

struct CacheEntry {
    hash: u64,
    stable_count: u8,
}

thread_local! {
    static PREVIOUS_PANE_SIGNATURE: RefCell<HashMap<String, CacheEntry>> =
        RefCell::new(HashMap::new());
}

/// Strips ANSI escape sequences from text, returning only visible characters.
fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = skip_ansi_escape(bytes, i);
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn skip_ansi_escape(bytes: &[u8], esc_index: usize) -> usize {
    let mut i = esc_index + 1;
    if i >= bytes.len() {
        return i;
    }

    match bytes[i] {
        b'[' => skip_csi_sequence(bytes, i + 1),
        b']' => skip_until_string_terminator(bytes, i + 1),
        b'P' | b'^' | b'_' => skip_until_st(bytes, i + 1),
        _ => {
            i += 1;
            while i < bytes.len() && bytes[i] >= 0x20 && bytes[i] <= 0x2f {
                i += 1;
            }
            if i < bytes.len() {
                i + 1
            } else {
                i
            }
        }
    }
}

fn skip_csi_sequence(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        if (0x40..=0x7e).contains(&bytes[i]) {
            return i + 1;
        }
        i += 1;
    }
    i
}

fn skip_until_string_terminator(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        if bytes[i] == 0x07 {
            return i + 1;
        }
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            return i + 2;
        }
        i += 1;
    }
    i
}

fn skip_until_st(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            return i + 2;
        }
        i += 1;
    }
    i
}

/// Like `pane_content_signature_with_boundary` using the default heuristic
/// boundary (separator or prompt character). Kept for backward-compatible
/// test use.
#[cfg(test)]
fn pane_content_signature(pane_text: &str) -> u64 {
    pane_content_signature_with_boundary(
        pane_text,
        pane_content_boundary(
            &pane_text
                .lines()
                .map(|l| l.trim_end())
                .collect::<Vec<&str>>(),
        ),
    )
}

fn runtime_command_override_name(value: &str) -> String {
    value
        .split_once(':')
        .map(|(_, command_name)| command_name)
        .unwrap_or(value)
        .to_string()
}

fn runtime_command_override_is_prompt(value: &str) -> bool {
    let command_name = value
        .split_once(':')
        .map(|(_, command_name)| command_name)
        .unwrap_or(value);
    command_name == "bash"
}

fn runtime_command_override_is_running(value: &str) -> bool {
    let command_name = value
        .split_once(':')
        .map(|(_, command_name)| command_name)
        .unwrap_or(value);
    command_name == super::WAITAGENT_RUNTIME_RUNNING_OVERRIDE
}

fn apply_temporal_input_hysteresis(
    session_key: &str,
    pane_text: &str,
    policy: InputStabilityPolicy,
    mut state: ManagedSessionTaskState,
) -> ManagedSessionTaskState {
    if state != ManagedSessionTaskState::Input {
        return state;
    }
    if policy == InputStabilityPolicy::Immediate {
        return state;
    }

    let plain_lines: Vec<&str> = pane_text.lines().map(|l| l.trim_end()).collect();
    let content_end = pane_content_boundary(&plain_lines);
    let current_sig = pane_content_signature_with_boundary(pane_text, content_end);

    PREVIOUS_PANE_SIGNATURE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(prev) = cache.get(session_key) {
            if prev.hash == current_sig {
                // Content stable — count consecutive stable polls.
                // Only transition to Input after the content has
                // been stable for STABLE_THRESHOLD polls, so brief
                // pauses during streaming don't cause I/R flicker.
                let new_count = prev.stable_count.saturating_add(1);
                if new_count < STABLE_THRESHOLD {
                    state = ManagedSessionTaskState::Running;
                }
                cache.insert(
                    session_key.to_string(),
                    CacheEntry {
                        hash: current_sig,
                        stable_count: new_count,
                    },
                );
            } else {
                // Content still changing — override to Running.
                state = ManagedSessionTaskState::Running;
                cache.insert(
                    session_key.to_string(),
                    CacheEntry {
                        hash: current_sig,
                        stable_count: 0,
                    },
                );
            }
        } else {
            // First poll for this session — seed the cache but
            // keep the detector's original Input state. This
            // means the first poll after a transition always
            // shows a brief I flash, which the hysteresis on
            // subsequent polls smooths out.
            cache.insert(
                session_key.to_string(),
                CacheEntry {
                    hash: current_sig,
                    stable_count: 0,
                },
            );
        }
    });

    state
}

fn apply_running_override(
    running_override: bool,
    state: ManagedSessionTaskState,
) -> ManagedSessionTaskState {
    if running_override && matches!(state, ManagedSessionTaskState::Unknown) {
        ManagedSessionTaskState::Running
    } else {
        state
    }
}

#[cfg(test)]
fn clear_temporal_input_hysteresis_cache() {
    PREVIOUS_PANE_SIGNATURE.with(|cache| cache.borrow_mut().clear());
}

/// Like `pane_content_signature` but with an explicit content boundary line
/// index, used when ANSI-based background color analysis provides a more
/// accurate boundary than the separator/prompt heuristic.
fn pane_content_signature_with_boundary(pane_text: &str, content_end: usize) -> u64 {
    let lines: Vec<&str> = pane_text.lines().map(|l| l.trim_end()).collect();
    let end = content_end.min(lines.len());

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for line in &lines[..end] {
        line.hash(&mut hasher);
        "\n".hash(&mut hasher);
    }
    hasher.finish()
}

/// Find the line index where the content/input boundary is, using the last
/// prompt character (`›` or `❯`) as the primary boundary. Falls back to a
/// separator line.
///
/// Everything at or above the boundary index is "content" — typing at the
/// prompt is excluded, so the content signature stays stable during input.
fn pane_content_boundary(lines: &[&str]) -> usize {
    // Use the last prompt character as the preferred boundary, so typing
    // at the prompt never changes the content signature.
    if let Some(pos) = lines.iter().rposition(|line| {
        let trimmed = line.trim();
        trimmed.starts_with('›') || trimmed.starts_with('❯')
    }) {
        return pos;
    }

    // Fall back to separator line
    if let Some(pos) = lines.iter().position(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty()
            && trimmed.chars().count() >= 3
            && trimmed.chars().all(|c| c == '─' || c == '━')
    }) {
        return pos;
    }

    0
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TmuxSessionRuntimeMetadata {
    pub(super) command_name: Option<String>,
    pub(super) current_path: Option<PathBuf>,
    pub(super) task_state: ManagedSessionTaskState,
    pub(super) is_dead: bool,
}

impl EmbeddedTmuxBackend {
    pub(super) fn session_runtime_metadata(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<TmuxSessionRuntimeMetadata, TmuxError> {
        let main_pane = match self.session_presentation_pane_info(socket_name, session_name)? {
            Some(pane) => Some(pane),
            None => self.session_main_pane_info(socket_name, session_name)?,
        };
        let Some(main_pane) = main_pane else {
            return Ok(TmuxSessionRuntimeMetadata::default());
        };
        let pane_ansi = self.capture_pane_text(socket_name, &main_pane.pane_id)?;
        let pane_text = strip_ansi(&pane_ansi);
        let current_command = main_pane.current_command.as_deref().unwrap_or_default();
        let foreground_argv = super::foreground_process_argv_for_pane_shell(main_pane.pane_pid);
        let detected_command_name = self.registry.detect_command_name(
            current_command,
            foreground_argv.as_deref(),
            &pane_text,
        );
        let workspace = crate::infra::tmux_types::TmuxWorkspaceHandle {
            workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(session_name),
            socket_name: socket_name.clone(),
            session_name: crate::infra::tmux_types::TmuxSessionName::new(session_name),
        };
        let runtime_override = self
            .show_session_option(&workspace, super::WAITAGENT_RUNTIME_COMMAND_OVERRIDE_OPTION)
            .ok()
            .flatten();
        let prompt_override = runtime_override
            .as_deref()
            .is_some_and(runtime_command_override_is_prompt);
        let running_override = runtime_override
            .as_deref()
            .is_some_and(runtime_command_override_is_running);
        let command_name = runtime_override
            .as_ref()
            .filter(|override_value| !runtime_command_override_is_running(override_value))
            .map(|override_value| runtime_command_override_name(&override_value))
            .unwrap_or(detected_command_name);
        let task_state = if main_pane.in_mode {
            ManagedSessionTaskState::Running
        } else if prompt_override {
            ManagedSessionTaskState::Input
        } else if let Some(hook_state) =
            self.agent_signal_task_state(&workspace, &main_pane.pane_id, &command_name)
        {
            hook_state
        } else {
            let mut state = self
                .registry
                .infer_task_state(Some(&command_name), &pane_text);

            state = apply_running_override(running_override, state);

            // Temporal content-change check: when the detector reports Input
            // but the agent asks for content stability, actively changing
            // content above the prompt means the agent is still Running.
            if state == ManagedSessionTaskState::Input {
                let policy = self
                    .registry
                    .input_stability_policy(Some(&command_name), &pane_text);
                let session_key = format!("{}:{}", socket_name.as_str(), session_name);
                state = apply_temporal_input_hysteresis(&session_key, &pane_text, policy, state);
            }

            state
        };
        Ok(TmuxSessionRuntimeMetadata {
            command_name: Some(command_name.clone()),
            current_path: main_pane.current_path.clone(),
            task_state,
            is_dead: main_pane.is_dead,
        })
    }

    fn agent_signal_task_state(
        &self,
        workspace: &crate::infra::tmux_types::TmuxWorkspaceHandle,
        pane_id: &TmuxPaneId,
        command_name: &str,
    ) -> Option<ManagedSessionTaskState> {
        let agent = self
            .show_session_option(workspace, super::WAITAGENT_AGENT_SIGNAL_AGENT_OPTION)
            .ok()
            .flatten()?;
        if !self
            .registry
            .agent_signal_matches_command(&agent, command_name)
        {
            return None;
        }
        let signal_pane = self
            .show_session_option(workspace, super::WAITAGENT_AGENT_SIGNAL_PANE_OPTION)
            .ok()
            .flatten()?;
        if signal_pane != pane_id.as_str() {
            return None;
        }
        self.show_session_option(workspace, super::WAITAGENT_AGENT_SIGNAL_STATE_OPTION)
            .ok()
            .flatten()
            .and_then(|state| ManagedSessionTaskState::parse(&state))
    }

    fn session_presentation_pane_info(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<Option<TmuxPaneInfo>, TmuxError> {
        let Ok(pane_id) =
            self.target_presentation_pane_on_socket(socket_name.as_str(), session_name)
        else {
            return Ok(None);
        };
        let session_name = self.pane_session_name_on_socket(socket_name.as_str(), &pane_id)?;
        let panes = self.list_panes_on_target(socket_name, &session_name)?;
        Ok(panes.into_iter().find(|pane| pane.pane_id == pane_id))
    }

    fn session_main_pane_info(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<Option<TmuxPaneInfo>, TmuxError> {
        let panes = self.list_panes_on_target(socket_name, session_name)?;
        Ok(panes
            .iter()
            .find(|pane| {
                !pane.is_dead
                    && pane.title != super::WAITAGENT_SIDEBAR_PANE_TITLE
                    && pane.title != super::WAITAGENT_FOOTER_PANE_TITLE
            })
            .or_else(|| {
                panes.iter().find(|pane| {
                    pane.title != super::WAITAGENT_SIDEBAR_PANE_TITLE
                        && pane.title != super::WAITAGENT_FOOTER_PANE_TITLE
                })
            })
            .cloned())
    }

    pub(crate) fn list_panes_on_target(
        &self,
        socket_name: &TmuxSocketName,
        target: &str,
    ) -> Result<Vec<TmuxPaneInfo>, TmuxError> {
        let args = vec![
            "list-panes".to_string(),
            "-t".to_string(),
            super::exact_session_target(target),
            "-F".to_string(),
            "#{pane_id}\t#{pane_pid}\t#{pane_title}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_dead}\t#{pane_in_mode}"
                .to_string(),
        ];
        let output = self.run_on_socket(socket_name, &args)?;
        output
            .stdout
            .lines()
            .map(Self::pane_info_for_line)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Captures pane text with ANSI escape sequences preserved.
    /// Stripped text is used for the detector; raw ANSI is used for
    /// background-color boundary analysis (e.g. Codex TUI input area detection).
    fn capture_pane_text(
        &self,
        socket_name: &TmuxSocketName,
        pane_id: &TmuxPaneId,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "capture-pane".to_string(),
            "-p".to_string(),
            "-e".to_string(),
            "-t".to_string(),
            pane_id.as_str().to_string(),
        ];
        let output = self.run_on_socket(socket_name, &args)?;
        Ok(output.stdout)
    }

    pub(super) fn pane_info_for_line(line: &str) -> Result<TmuxPaneInfo, TmuxError> {
        let mut parts = line.splitn(7, '\t');
        let pane_id = parts.next().unwrap_or_default();
        let pane_pid = parts.next().unwrap_or_default();
        let title = parts.next().unwrap_or_default();
        let current_command = parts.next().unwrap_or_default();
        let current_path = parts.next().unwrap_or_default();
        let dead = parts.next().unwrap_or_default();
        let in_mode = parts.next().unwrap_or_default();

        Ok(TmuxPaneInfo {
            pane_id: TmuxPaneId::new(parse_tmux_id(pane_id, '%', "pane id")?),
            pane_pid: pane_pid.parse::<u32>().ok(),
            title: title.to_string(),
            current_command: (!current_command.is_empty()).then(|| current_command.to_string()),
            current_path: (!current_path.is_empty()).then(|| PathBuf::from(current_path)),
            is_dead: dead == "1",
            in_mode: in_mode == "1",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent_detector::DetectorRegistry;

    #[test]
    fn content_changed_detects_claude_execution_output() {
        // Claude TUI: content above separator changes during execution.
        let pane_t1 = "output line 1\n\
                       output line 2\n\
                       \n\
                       ─────────────────────\n\
                       ❯ \n\
                       ─────────────────────\n\
                       esc to interrupt";
        let pane_t2 = "output line 1\n\
                       output line 2\n\
                       output line 3\n\
                       \n\
                       ─────────────────────\n\
                       ❯ \n\
                       ─────────────────────\n\
                       esc to interrupt";
        let sig1 = pane_content_signature(pane_t1);
        let sig2 = pane_content_signature(pane_t2);
        assert_ne!(
            sig1, sig2,
            "content above separator changed → signatures must differ"
        );
    }

    #[test]
    fn content_stable_detects_claude_idle_at_input() {
        // Claude TUI: content above separator is stable at Input.
        let pane = "output line 1\n\
                    output line 2\n\
                    \n\
                    ─────────────────────\n\
                    ❯ \n\
                    ─────────────────────\n\
                    ? for shortcuts";
        let sig1 = pane_content_signature(pane);
        let sig2 = pane_content_signature(pane);
        assert_eq!(sig1, sig2, "same content → same signature");
    }

    #[test]
    fn content_changed_detects_codex_execution_output() {
        // Codex (no separator): content above prompt area changes.
        let pane_t1 = "User: do something\n\
                       Codex: processing...\n\
                       \n\
                       › \n\
                       tip: press Enter to run";
        let pane_t2 = "User: do something\n\
                       Codex: processing...\n\
                       Codex: result here\n\
                       \n\
                       › \n\
                       tip: press Enter to run";
        let sig1 = pane_content_signature(pane_t1);
        let sig2 = pane_content_signature(pane_t2);
        assert_ne!(
            sig1, sig2,
            "content above › changed → signatures must differ"
        );
    }

    #[test]
    fn content_stable_detects_codex_idle_at_input() {
        // Codex (no separator): stable content at Input.
        let pane = "User: hello\n\
                    Codex: Hi!\n\
                    \n\
                    › \n\
                    tip: use @ to reference";
        let sig1 = pane_content_signature(pane);
        let sig2 = pane_content_signature(pane);
        assert_eq!(sig1, sig2, "same content → same signature");
    }

    #[test]
    fn detector_registry_provides_codex_input_policy() {
        let pane = "╭────────────────────────────────────────────╮\n\
                    │ >_ OpenAI Codex                          │\n\
                    ╰────────────────────────────────────────────╯\n\
                    \n\
                    › Write tests for @filename\n\
                    \n\
                      gpt-5.5 high · ~";

        assert_eq!(
            DetectorRegistry::default().input_stability_policy(Some("codex"), pane),
            InputStabilityPolicy::Immediate
        );
    }

    #[test]
    fn running_override_marker_is_not_a_display_command_name() {
        let marker = format!("42:{}", super::super::WAITAGENT_RUNTIME_RUNNING_OVERRIDE);

        assert!(runtime_command_override_is_running(&marker));
        assert!(!runtime_command_override_is_prompt(&marker));
    }

    #[test]
    fn running_override_does_not_mask_confirm() {
        assert_eq!(
            apply_running_override(true, ManagedSessionTaskState::Confirm),
            ManagedSessionTaskState::Confirm
        );
    }

    #[test]
    fn running_override_does_not_mask_input() {
        assert_eq!(
            apply_running_override(true, ManagedSessionTaskState::Input),
            ManagedSessionTaskState::Input
        );
    }

    #[test]
    fn running_override_fills_unknown() {
        assert_eq!(
            apply_running_override(true, ManagedSessionTaskState::Unknown),
            ManagedSessionTaskState::Running
        );
    }

    #[test]
    fn agent_signal_matches_exact_agent_or_kimi_wrapped_claude() {
        let registry = DetectorRegistry::default();

        assert!(registry.agent_signal_matches_command("codex", "codex"));
        assert!(registry.agent_signal_matches_command("kimi", "kimi"));
        assert!(registry.agent_signal_matches_command("kimi", "claude"));
        assert!(!registry.agent_signal_matches_command("claude", "kimi"));
        assert!(!registry.agent_signal_matches_command("codex", "claude"));
    }

    #[test]
    fn immediate_input_policy_skips_temporal_running_override() {
        clear_temporal_input_hysteresis_cache();
        let session_key = "test:immediate-input";
        let pane_t1 = "• Working\n\
                       └ Searching files\n\
                       \n\
                       › \n\
                       esc to interrupt";
        let pane_t2 = "• Working\n\
                       └ Reading src/main.rs\n\
                       \n\
                       › \n\
                       esc to interrupt";

        assert_eq!(
            apply_temporal_input_hysteresis(
                session_key,
                pane_t1,
                InputStabilityPolicy::Immediate,
                ManagedSessionTaskState::Input,
            ),
            ManagedSessionTaskState::Input
        );
        assert_eq!(
            apply_temporal_input_hysteresis(
                session_key,
                pane_t2,
                InputStabilityPolicy::Immediate,
                ManagedSessionTaskState::Input,
            ),
            ManagedSessionTaskState::Input
        );
    }

    #[test]
    fn immediate_input_policy_does_not_wait_for_stable_polls() {
        clear_temporal_input_hysteresis_cache();
        let session_key = "test:immediate-idle";
        let pane = "Codex: Done.\n\
                    \n\
                    › \n\
                    esc to interrupt";

        assert_eq!(
            apply_temporal_input_hysteresis(
                session_key,
                pane,
                InputStabilityPolicy::Immediate,
                ManagedSessionTaskState::Input,
            ),
            ManagedSessionTaskState::Input
        );
        assert_eq!(
            apply_temporal_input_hysteresis(
                session_key,
                pane,
                InputStabilityPolicy::Immediate,
                ManagedSessionTaskState::Input,
            ),
            ManagedSessionTaskState::Input
        );
        assert_eq!(
            apply_temporal_input_hysteresis(
                session_key,
                pane,
                InputStabilityPolicy::Immediate,
                ManagedSessionTaskState::Input,
            ),
            ManagedSessionTaskState::Input
        );
        assert_eq!(
            apply_temporal_input_hysteresis(
                session_key,
                pane,
                InputStabilityPolicy::Immediate,
                ManagedSessionTaskState::Input,
            ),
            ManagedSessionTaskState::Input
        );
    }

    #[test]
    fn detector_registry_provides_claude_stable_prompt_policy() {
        let pane = "● Bash(echo hello)\n\
                    \n\
                    ─────────────────────\n\
                    ❯ \n\
                    ─────────────────────\n\
                    esc to interrupt";

        assert_eq!(
            DetectorRegistry::default().input_stability_policy(Some("claude"), pane),
            InputStabilityPolicy::Immediate
        );
    }

    #[test]
    fn detector_registry_provides_kimi_stable_prompt_policy() {
        let pane = "Welcome to Kimi Code!\n\
                    ╭─────────────────────────────────────────╮\n\
                    │ >                                       │\n\
                    ╰─────────────────────────────────────────╯\n\
                    K2.7 Code thinking  ~\n\
                    context: 0.0% (0/262.1k)";

        assert_eq!(
            DetectorRegistry::default().input_stability_policy(Some("kimi"), pane),
            InputStabilityPolicy::Immediate
        );
    }

    #[test]
    fn detector_registry_provides_shell_immediate_input_policy() {
        let pane = "root@host:/workspace#\n\n\n";

        assert_eq!(
            DetectorRegistry::default().input_stability_policy(Some("bash"), pane),
            InputStabilityPolicy::Immediate
        );
    }

    #[test]
    fn shell_prompt_input_does_not_enter_temporal_hysteresis() {
        clear_temporal_input_hysteresis_cache();
        let session_key = "test:shell-idle";
        let pane = "root@host:/workspace#\n\n\n";
        let policy = DetectorRegistry::default().input_stability_policy(Some("bash"), pane);

        assert_eq!(
            apply_temporal_input_hysteresis(
                session_key,
                pane,
                policy,
                ManagedSessionTaskState::Input,
            ),
            ManagedSessionTaskState::Input
        );
        assert_eq!(
            apply_temporal_input_hysteresis(
                session_key,
                pane,
                policy,
                ManagedSessionTaskState::Input,
            ),
            ManagedSessionTaskState::Input
        );
    }

    #[test]
    fn very_short_panes_produce_stable_hash() {
        // Very short panes have no content above the prompt area — hash is
        // empty but consistent, so no spurious Running override.
        assert_eq!(
            pane_content_signature(""),
            pane_content_signature(""),
            "empty pane stable"
        );
        // Even though empty and 2-line produce the same signature (end=0),
        // this is acceptable: there is no content above the prompt area
        // to compare, so the temporal check correctly skips the override.
        assert_eq!(
            pane_content_signature(""),
            pane_content_signature("› \ntip: something"),
            "no content above prompt → same empty signature"
        );
    }

    #[test]
    fn three_line_pane_signature_detects_change() {
        // With 3+ raw lines, there IS content above the prompt.
        let idle = "conversation\n\
                    › \n\
                    tip: something";
        let running = "more output\n\
                       › \n\
                       tip: something";
        assert_ne!(
            pane_content_signature(idle),
            pane_content_signature(running),
            "content above › differs → signatures differ"
        );
        assert_eq!(
            pane_content_signature(idle),
            pane_content_signature(idle),
            "same content → same signature"
        );
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        let input = "\x1b[48;5;235mHello\x1b[0m World\x1b[K\n";
        let result = strip_ansi(input);
        assert_eq!(result, "Hello World\n");
    }

    #[test]
    fn strip_ansi_preserves_regular_text() {
        let input = "plain text\nwithout escapes";
        let result = strip_ansi(input);
        assert_eq!(result, "plain text\nwithout escapes");
    }

    #[test]
    fn strip_ansi_handles_non_csi_escapes() {
        // \x1b(B is character set selection (non-CSI)
        let input = "\x1b(BHello\x1b(BWorld";
        let result = strip_ansi(input);
        assert_eq!(result, "HelloWorld");
    }

    #[test]
    fn strip_ansi_removes_osc8_hyperlink_sequences() {
        let input = "See \x1b]8;;https://example.test\x1b\\example\x1b]8;;\x1b\\ now";
        let result = strip_ansi(input);
        assert_eq!(result, "See example now");
    }

    #[test]
    fn codex_update_notice_with_osc8_link_infers_input_after_ansi_strip() {
        let pane = "\x1b[2m╭─────────────────────────────────────────────────╮\x1b[0m\n\
                    \x1b[2m│ \x1b[0m✨ Update available! \x1b[1m0.142.2 -> 0.142.3\x1b[0;2m         │\x1b[0m\n\
                    \x1b[2m│ \x1b[0mRun npm install -g @openai/codex to update.\x1b[2m     │\x1b[0m\n\
                    \x1b[2m│                                                 │\x1b[0m\n\
                    \x1b[2m│ \x1b[0mSee full release notes:\x1b[2m                         │\x1b[0m\n\
                    \x1b[2m│ \x1b[0m\x1b]8;;https://github.com/openai/codex/releases/latest\x1b\\https://github.com/openai/codex/releases/latest\x1b]8;;\x1b\\ │\n\
                    \x1b[2m╰─────────────────────────────────────────────────╯\x1b[0m\n\
                    \n\
                    ⚠ Codex could not find bubblewrap on PATH. Install bubblewrap with your OS package manager. See the sandbox prerequisites:\n\
                    https://developers.openai.com/codex/concepts/sandboxing#prerequisites. Codex will use the bundled bubblewrap in the meantime.\n\
                    \n\
                    \x1b[2m╭────────────────────────────────────────────╮\x1b[0m\n\
                    \x1b[2m│ >_ \x1b[0;1mOpenAI Codex\x1b[0;2m (v0.142.2)                 │\x1b[0m\n\
                    \x1b[2m│                                            │\x1b[0m\n\
                    \x1b[2m│ model:     \x1b[0mgpt-5.5 high\x1b[2m   \x1b[0m/model to change │\n\
                    \x1b[2m│ directory: \x1b[0m~\x1b[2m                               │\x1b[0m\n\
                    \x1b[2m╰────────────────────────────────────────────╯\x1b[0m\n\
                    \n\
                      \x1b[1mTip:\x1b[0m See the Codex keymap documentation for supported actions and examples.\n\
                    \n\
                    \n\
                    \x1b[1m›\x1b[0m \x1b[2mWrite tests for @filename\x1b[0m\n\
                    \n\
                      gpt-5.5 high · ~";
        let stripped = strip_ansi(pane);
        let registry = DetectorRegistry::default();

        assert!(stripped.contains("OpenAI Codex"));
        assert!(stripped.contains("› Write tests for @filename"));
        assert_eq!(
            registry.detect_command_name("node", None, &stripped),
            "node"
        );
        assert_eq!(
            registry.infer_task_state(Some("codex"), &stripped),
            ManagedSessionTaskState::Input
        );
    }

    #[test]
    fn pane_content_signature_with_boundary_uses_explicit_boundary() {
        let pane = "line 1\nline 2\n› \ntip";
        // boundary=2 means exclude the last 2 lines
        let sig1 = pane_content_signature_with_boundary(pane, 2);
        let sig2 = pane_content_signature_with_boundary(pane, 2);
        assert_eq!(sig1, sig2, "same boundary → same signature");

        // Different content but same boundary → different signature
        let pane2 = "changed\nline 2\n› \ntip";
        let sig3 = pane_content_signature_with_boundary(pane2, 2);
        assert_ne!(sig1, sig3, "different content → different signature");
    }

    #[test]
    fn pane_content_signature_with_boundary_clamps_to_lines_len() {
        let sig1 = pane_content_signature_with_boundary("a\nb", 999);
        let sig2 = pane_content_signature_with_boundary("a\nb", 2);
        assert_eq!(sig1, sig2, "boundary clamped to lines.len()");
    }
}
