use crate::domain::session_catalog::ManagedSessionTaskState;
use crate::infra::tmux_error::{parse_tmux_id, TmuxError};
use crate::infra::tmux_types::{TmuxPaneId, TmuxPaneInfo, TmuxSocketName};
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use super::EmbeddedTmuxBackend;

// Per-thread cache: `{socket}:{session}` → content signature hash.
// When the content above the prompt separator changes between polls,
// the agent is Running, even if the prompt character is visible.
thread_local! {
    static PREVIOUS_PANE_SIGNATURE: RefCell<HashMap<String, u64>> =
        RefCell::new(HashMap::new());
}

/// Strips ANSI escape sequences from text, returning only visible characters.
fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // CSI sequence: \x1b[...<final>
                i += 2;
                while i < bytes.len() && !bytes[i].is_ascii_alphabetic() && bytes[i] != b'~' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else if i + 1 < bytes.len() {
                // Non-CSI escape: \x1b<intermediates...><final>
                i += 1; // skip \x1b
                i += 1; // skip first byte after \x1b (intermediate or final)
                while i < bytes.len() && bytes[i] >= 0x20 && bytes[i] <= 0x2F {
                    i += 1; // remaining intermediate bytes
                }
                if i < bytes.len() {
                    i += 1; // final byte
                }
            } else {
                i += 1; // trailing bare \x1b
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
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
        let panes = self.list_panes_on_target(socket_name, session_name)?;
        let Some(main_pane) = panes.iter().find(|pane| {
            pane.title != super::WAITAGENT_SIDEBAR_PANE_TITLE
                && pane.title != super::WAITAGENT_FOOTER_PANE_TITLE
        }) else {
            return Ok(TmuxSessionRuntimeMetadata::default());
        };
        let pane_ansi = self.capture_pane_text(socket_name, &main_pane.pane_id)?;
        let pane_text = strip_ansi(&pane_ansi);
        let current_command = main_pane.current_command.as_deref().unwrap_or_default();
        let foreground_argv = super::foreground_process_argv_for_pane_shell(main_pane.pane_pid);
        let command_name = self.registry.detect_command_name(
            current_command,
            foreground_argv.as_deref(),
            &pane_text,
        );
        let task_state = if main_pane.in_mode {
            ManagedSessionTaskState::Running
        } else {
            let mut state = self
                .registry
                .infer_task_state(Some(&command_name), &pane_text);

            // Temporal content-change check: when the detector reports Input
            // but the pane content above the prompt area is actively changing
            // between polls, the agent is actually Running (output streaming).
            // This distinguishes the "awaiting user input" Input state from the
            // "prompt visible during active execution" false positive.
            if state == ManagedSessionTaskState::Input {
                let session_key = format!("{}:{}", socket_name.as_str(), session_name);
                let plain_lines: Vec<&str> = pane_text.lines().map(|l| l.trim_end()).collect();
                let content_end = pane_content_boundary(&plain_lines);
                let current_sig = pane_content_signature_with_boundary(&pane_text, content_end);

                PREVIOUS_PANE_SIGNATURE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    if let Some(prev_sig) = cache.get(&session_key) {
                        if *prev_sig != current_sig {
                            state = ManagedSessionTaskState::Running;
                        }
                    }
                    cache.insert(session_key, current_sig);
                });
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

    pub(super) fn list_panes_on_target(
        &self,
        socket_name: &TmuxSocketName,
        target: &str,
    ) -> Result<Vec<TmuxPaneInfo>, TmuxError> {
        let args = vec![
            "list-panes".to_string(),
            "-t".to_string(),
            target.to_string(),
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
