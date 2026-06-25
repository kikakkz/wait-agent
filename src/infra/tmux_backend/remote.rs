use super::{
    exact_session_target, EmbeddedTmuxBackend, TmuxError, WAITAGENT_PANE_PIPE_OWNER_OPTION,
};
use crate::infra::tmux::{TmuxPaneId, TmuxSocketName};
use std::str;

const WAITAGENT_SIDEBAR_PANE_TITLE: &str = "waitagent-sidebar";
const WAITAGENT_FOOTER_PANE_TITLE: &str = "waitagent-footer";
const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";
const WAITAGENT_PANE_TARGET_SESSION_OPTION: &str = "@waitagent_target_session_name";

impl EmbeddedTmuxBackend {
    pub(crate) fn target_presentation_pane_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, TmuxError> {
        if let Some(pane) =
            self.configured_target_presentation_pane_on_socket(socket_name, target_session_name)?
        {
            return Ok(pane);
        }

        Err(TmuxError::new(format!(
            "target session `{target_session_name}` on socket `{socket_name}` has no authoritative presentation pane"
        )))
    }

    fn configured_target_presentation_pane_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<Option<TmuxPaneId>, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let output = match self.run_on_socket(
            &socket,
            &[
                "show-options".to_string(),
                "-qv".to_string(),
                "-t".to_string(),
                exact_session_target(target_session_name),
                WAITAGENT_MAIN_PANE_OPTION.to_string(),
            ],
        ) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(None),
            Err(error) => return Err(error),
        };
        let pane = output.stdout.trim();
        if pane.is_empty() {
            return Ok(None);
        }
        if !self.presentation_pane_is_live_on_socket(socket_name, pane)? {
            return Err(TmuxError::new(format!(
                "authoritative presentation pane `{pane}` for target session `{target_session_name}` on socket `{socket_name}` is not live"
            )));
        }
        let pane_target_session = self
            .pane_target_session_name_on_socket(socket_name, pane)?
            .ok_or_else(|| {
                TmuxError::new(format!(
                    "authoritative presentation pane `{pane}` on socket `{socket_name}` is missing `{WAITAGENT_PANE_TARGET_SESSION_OPTION}`"
                ))
            })?;
        if pane_target_session != target_session_name {
            return Err(TmuxError::new(format!(
                "authoritative presentation pane `{pane}` on socket `{socket_name}` belongs to target session `{pane_target_session}`, expected `{target_session_name}`"
            )));
        }
        Ok(Some(TmuxPaneId::new(pane)))
    }

    fn pane_target_session_name_on_socket(
        &self,
        socket_name: &str,
        pane: &str,
    ) -> Result<Option<String>, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let output = match self.run_on_socket(
            &socket,
            &[
                "show-options".to_string(),
                "-pqv".to_string(),
                "-t".to_string(),
                pane.to_string(),
                WAITAGENT_PANE_TARGET_SESSION_OPTION.to_string(),
            ],
        ) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(None),
            Err(error) => return Err(error),
        };
        let value = output.stdout.trim();
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(value.to_string()))
        }
    }

    fn presentation_pane_is_live_on_socket(
        &self,
        socket_name: &str,
        pane: &str,
    ) -> Result<bool, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let output = match self.run_on_socket(
            &socket,
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane.to_string(),
                "#{pane_dead}	#{pane_title}".to_string(),
            ],
        ) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut parts = output.stdout.trim_end().split('\t');
        let pane_dead = parts.next().unwrap_or_default();
        let pane_title = parts.next().unwrap_or_default();
        Ok(pane_dead == "0"
            && pane_title != WAITAGENT_SIDEBAR_PANE_TITLE
            && pane_title != WAITAGENT_FOOTER_PANE_TITLE)
    }

    pub(crate) fn target_main_pane_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, TmuxError> {
        let panes =
            self.list_panes_on_target(&TmuxSocketName::new(socket_name), target_session_name)?;
        panes.into_iter()
            .find(|pane| {
                !pane.is_dead
                    && pane.title != WAITAGENT_SIDEBAR_PANE_TITLE
                    && pane.title != WAITAGENT_FOOTER_PANE_TITLE
            })
            .map(|pane| pane.pane_id)
            .ok_or_else(|| {
                TmuxError::new(format!(
                    "target session `{target_session_name}` on socket `{socket_name}` has no live main pane"
                ))
            })
    }

    #[allow(dead_code)]
    pub(crate) fn send_input_to_pane_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        bytes: &[u8],
    ) -> Result<(), TmuxError> {
        for chunk in split_tmux_input(bytes)? {
            match chunk {
                TmuxInputChunk::Literal(literal) => self.run_on_socket(
                    &TmuxSocketName::new(socket_name),
                    &send_literal_keys_args(pane, &literal),
                )?,
                TmuxInputChunk::HexBytes(bytes) => self.run_on_socket(
                    &TmuxSocketName::new(socket_name),
                    &send_hex_keys_args(pane, &bytes),
                )?,
            };
        }
        Ok(())
    }

    pub(crate) fn pane_session_name_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "#{session_name}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout.trim().to_string())
    }

    #[allow(dead_code)]
    pub(crate) fn pane_tty_path_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "#{pane_tty}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout.trim().to_string())
    }

    pub(crate) fn resize_pane_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(), TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        self.run_on_socket(&socket, &resize_pane_args(pane, cols, rows))?;

        let window = self.run_on_socket(&socket, &pane_window_id_args(pane))?;
        let window = window.stdout.trim();
        if !window.is_empty() {
            self.run_on_socket(&socket, &resize_window_args(window, cols, rows))?;
        }
        Ok(())
    }

    pub(crate) fn clear_pane_pipe_on_socket_if_owner(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        expected_owner: &str,
    ) -> Result<bool, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let owner =
            self.show_pane_option_on_socket(&socket, pane, WAITAGENT_PANE_PIPE_OWNER_OPTION)?;
        if owner.as_deref() != Some(expected_owner) {
            return Ok(false);
        }
        self.run_on_socket(&socket, &clear_pane_pipe_args(pane))?;
        self.unset_pane_option_on_socket(&socket, pane, WAITAGENT_PANE_PIPE_OWNER_OPTION)?;
        Ok(true)
    }

    pub(crate) fn pane_pipe_is_live_on_socket_for_owner(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        expected_owner: &str,
    ) -> Result<bool, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let owner =
            self.show_pane_option_on_socket(&socket, pane, WAITAGENT_PANE_PIPE_OWNER_OPTION)?;
        if owner.as_deref() != Some(expected_owner) {
            return Ok(false);
        }
        let output = self.run_on_socket(
            &socket,
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                "#{pane_pipe}".to_string(),
            ],
        )?;
        Ok(output.stdout.trim() == "1")
    }

    pub(crate) fn set_pane_pipe_on_socket_owned(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        owner: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        self.run_on_socket(&socket, &clear_pane_pipe_args(pane))?;
        self.set_pane_option_on_socket(&socket, pane, WAITAGENT_PANE_PIPE_OWNER_OPTION, owner)?;
        self.run_on_socket(&socket, &set_pane_pipe_args(pane, command))?;
        Ok(())
    }

    pub(crate) fn set_pane_hook_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        hook_name: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &set_pane_hook_args(pane, hook_name, command),
        )?;
        Ok(())
    }

    pub(crate) fn unset_pane_hook_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        hook_name: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &unset_pane_hook_args(pane, hook_name),
        )?;
        Ok(())
    }
}
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum TmuxInputChunk {
    Literal(String),
    HexBytes(Vec<u8>),
}

#[allow(dead_code)]
fn split_tmux_input(bytes: &[u8]) -> Result<Vec<TmuxInputChunk>, TmuxError> {
    let mut chunks = Vec::new();
    let mut literal = String::new();
    let mut index = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        if is_tmux_hex_byte(byte) {
            flush_literal(&mut chunks, &mut literal);
            let start = index;
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii() {
                index += 1;
            }
            chunks.push(TmuxInputChunk::HexBytes(bytes[start..index].to_vec()));
            continue;
        }

        if byte.is_ascii() {
            literal.push(byte as char);
            index += 1;
            continue;
        }

        let width = utf8_char_width(byte).ok_or_else(|| {
            TmuxError::new(format!(
                "remote input contains unsupported byte 0x{byte:02x}"
            ))
        })?;
        if index + width > bytes.len() {
            return Err(TmuxError::new(
                "remote input ended in the middle of a UTF-8 codepoint",
            ));
        }
        let slice = &bytes[index..index + width];
        let value = str::from_utf8(slice).map_err(|_| {
            TmuxError::new("remote input contains invalid UTF-8 outside ASCII control bytes")
        })?;
        literal.push_str(value);
        index += width;
    }

    flush_literal(&mut chunks, &mut literal);
    Ok(chunks)
}

fn flush_literal(chunks: &mut Vec<TmuxInputChunk>, literal: &mut String) {
    if !literal.is_empty() {
        chunks.push(TmuxInputChunk::Literal(std::mem::take(literal)));
    }
}

fn is_tmux_hex_byte(byte: u8) -> bool {
    matches!(byte, 0x00..=0x1f | 0x7f)
}

fn utf8_char_width(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

pub(crate) fn send_literal_keys_args(pane: &TmuxPaneId, literal: &str) -> Vec<String> {
    vec![
        "send-keys".to_string(),
        "-l".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        literal.to_string(),
    ]
}

fn send_hex_keys_args(pane: &TmuxPaneId, bytes: &[u8]) -> Vec<String> {
    let mut args = vec![
        "send-keys".to_string(),
        "-H".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
    ];
    args.extend(bytes.iter().map(|byte| format!("{byte:02x}")));
    args
}

fn resize_pane_args(pane: &TmuxPaneId, cols: usize, rows: usize) -> Vec<String> {
    vec![
        "resize-pane".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        "-x".to_string(),
        cols.to_string(),
        "-y".to_string(),
        rows.to_string(),
    ]
}

fn pane_window_id_args(pane: &TmuxPaneId) -> Vec<String> {
    vec![
        "display-message".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        "#{window_id}".to_string(),
    ]
}

fn resize_window_args(window_id: &str, cols: usize, rows: usize) -> Vec<String> {
    vec![
        "resize-window".to_string(),
        "-t".to_string(),
        window_id.to_string(),
        "-x".to_string(),
        cols.to_string(),
        "-y".to_string(),
        rows.to_string(),
    ]
}

fn clear_pane_pipe_args(pane: &TmuxPaneId) -> Vec<String> {
    vec![
        "pipe-pane".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
    ]
}

fn set_pane_pipe_args(pane: &TmuxPaneId, command: &str) -> Vec<String> {
    vec![
        "pipe-pane".to_string(),
        "-I".to_string(),
        "-O".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        command.to_string(),
    ]
}

fn set_pane_hook_args(pane: &TmuxPaneId, hook_name: &str, command: &str) -> Vec<String> {
    let target = pane.as_str();
    let session_target = target.split(':').next().unwrap_or(target);
    vec![
        "set-hook".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        session_target.to_string(),
        hook_name.to_string(),
        command.to_string(),
    ]
}

fn unset_pane_hook_args(pane: &TmuxPaneId, hook_name: &str) -> Vec<String> {
    let target = pane.as_str();
    let session_target = target.split(':').next().unwrap_or(target);
    vec![
        "set-hook".to_string(),
        "-u".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        session_target.to_string(),
        hook_name.to_string(),
    ]
}

#[allow(dead_code)]
fn set_session_environment_args(session_name: &str, key: &str, value: &str) -> Vec<String> {
    vec![
        "set-environment".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        key.to_string(),
        value.to_string(),
    ]
}

#[allow(dead_code)]
fn unset_session_environment_args(session_name: &str, key: &str) -> Vec<String> {
    vec![
        "set-environment".to_string(),
        "-u".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        key.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        clear_pane_pipe_args, pane_window_id_args, resize_pane_args, resize_window_args,
        send_hex_keys_args, send_literal_keys_args, set_pane_hook_args, set_pane_pipe_args,
        set_session_environment_args, split_tmux_input, unset_pane_hook_args,
        unset_session_environment_args, TmuxInputChunk,
    };
    use crate::infra::tmux::TmuxPaneId;

    #[test]
    fn split_tmux_input_preserves_ascii_controls_and_utf8_text() {
        let chunks =
            split_tmux_input("你a".as_bytes()).expect("valid UTF-8 should split into literals");
        assert_eq!(chunks, vec![TmuxInputChunk::Literal("你a".to_string())]);

        let chunks = split_tmux_input(&[0x1b, b'[', b'A']).expect("escape input should split");
        assert_eq!(
            chunks,
            vec![TmuxInputChunk::HexBytes(vec![0x1b, b'[', b'A'])]
        );
    }

    #[test]
    fn remote_tmux_args_use_native_send_resize_and_pipe_primitives() {
        assert_eq!(
            send_literal_keys_args(&TmuxPaneId::new("%4"), "你好"),
            vec!["send-keys", "-l", "-t", "%4", "你好"]
        );
        assert_eq!(
            send_hex_keys_args(&TmuxPaneId::new("%4"), &[0x1b, b'[', b'B']),
            vec!["send-keys", "-H", "-t", "%4", "1b", "5b", "42"]
        );
        assert_eq!(
            resize_pane_args(&TmuxPaneId::new("%4"), 120, 40),
            vec!["resize-pane", "-t", "%4", "-x", "120", "-y", "40"]
        );
        assert_eq!(
            pane_window_id_args(&TmuxPaneId::new("%4")),
            vec!["display-message", "-p", "-t", "%4", "#{window_id}"]
        );
        assert_eq!(
            resize_window_args("@7", 120, 40),
            vec!["resize-window", "-t", "@7", "-x", "120", "-y", "40"]
        );
        assert_eq!(
            clear_pane_pipe_args(&TmuxPaneId::new("%4")),
            vec!["pipe-pane", "-t", "%4"]
        );
        assert_eq!(
            set_pane_pipe_args(&TmuxPaneId::new("%4"), "echo bridge"),
            vec!["pipe-pane", "-I", "-O", "-t", "%4", "echo bridge"]
        );
        assert_eq!(
            set_pane_hook_args(
                &TmuxPaneId::new("shell-1:0.0"),
                "pane-died",
                "run-shell true"
            ),
            vec![
                "set-hook",
                "-p",
                "-t",
                "shell-1",
                "pane-died",
                "run-shell true"
            ]
        );
        assert_eq!(
            unset_pane_hook_args(&TmuxPaneId::new("shell-1:0.0"), "pane-died"),
            vec!["set-hook", "-u", "-p", "-t", "shell-1", "pane-died"]
        );
        assert_eq!(
            set_session_environment_args("shell-1", "WAITAGENT_X", "value"),
            vec!["set-environment", "-t", "shell-1", "WAITAGENT_X", "value"]
        );
        assert_eq!(
            unset_session_environment_args("shell-1", "WAITAGENT_X"),
            vec!["set-environment", "-u", "-t", "shell-1", "WAITAGENT_X"]
        );
    }
}
