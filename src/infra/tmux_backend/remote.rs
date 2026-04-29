use super::{EmbeddedTmuxBackend, TmuxError};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::tmux::{RemoteTargetPublicationBinding, TmuxPaneId, TmuxSocketName};
use std::str;

const WAITAGENT_SIDEBAR_PANE_TITLE: &str = "waitagent-sidebar";
const WAITAGENT_FOOTER_PANE_TITLE: &str = "waitagent-footer";

impl EmbeddedTmuxBackend {
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
                TmuxInputChunk::HexByte(byte) => self.run_on_socket(
                    &TmuxSocketName::new(socket_name),
                    &send_hex_key_args(pane, byte),
                )?,
            };
        }
        Ok(())
    }

    pub(crate) fn resize_pane_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &resize_pane_args(pane, cols, rows),
        )?;
        Ok(())
    }

    pub(crate) fn clear_pane_pipe_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &clear_pane_pipe_args(pane),
        )?;
        Ok(())
    }

    pub(crate) fn set_pane_pipe_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        command: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &set_pane_pipe_args(pane, command),
        )?;
        Ok(())
    }

    pub(crate) fn bind_remote_publication_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        transport_session_id: &str,
        selector: Option<&str>,
    ) -> Result<(), TmuxError> {
        let socket_name = TmuxSocketName::new(socket_name);
        self.run_on_socket(
            &socket_name,
            &set_session_environment_args(
                target_session_name,
                super::WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV,
                authority_id,
            ),
        )?;
        self.run_on_socket(
            &socket_name,
            &set_session_environment_args(
                target_session_name,
                super::WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV,
                transport_session_id,
            ),
        )?;
        match selector {
            Some(selector) => self.run_on_socket(
                &socket_name,
                &set_session_environment_args(
                    target_session_name,
                    super::WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV,
                    selector,
                ),
            )?,
            None => self.run_on_socket(
                &socket_name,
                &unset_session_environment_args(
                    target_session_name,
                    super::WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV,
                ),
            )?,
        };
        Ok(())
    }

    pub(crate) fn unbind_remote_publication_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), TmuxError> {
        let socket_name = TmuxSocketName::new(socket_name);
        for key in [
            super::WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV,
            super::WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV,
            super::WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV,
        ] {
            self.run_on_socket(
                &socket_name,
                &unset_session_environment_args(target_session_name, key),
            )?;
        }
        Ok(())
    }

    pub(crate) fn list_remote_publication_bindings(
        &self,
    ) -> Result<Vec<RemoteTargetPublicationBinding>, TmuxError> {
        let mut bindings = Vec::new();
        for socket_name in self.discover_waitagent_sockets()? {
            bindings.extend(self.list_remote_publication_bindings_on_socket(&socket_name)?);
        }
        Ok(bindings)
    }

    pub(crate) fn list_remote_publication_bindings_on_socket(
        &self,
        socket_name: &TmuxSocketName,
    ) -> Result<Vec<RemoteTargetPublicationBinding>, TmuxError> {
        let sessions = self.list_sessions_on_socket(socket_name)?;
        let mut bindings = Vec::new();
        for session in sessions {
            if session.session_role != Some(WorkspaceSessionRole::TargetHost) {
                continue;
            }
            let metadata = self.session_metadata(socket_name, session.address.session_id())?;
            let Some(authority_id) = metadata.remote_publication_authority_id else {
                continue;
            };
            let Some(transport_session_id) = metadata.remote_publication_transport_session_id else {
                continue;
            };
            bindings.push(RemoteTargetPublicationBinding {
                socket_name: socket_name.as_str().to_string(),
                target_session_name: session.address.session_id().to_string(),
                authority_id,
                transport_session_id,
                selector: metadata.remote_publication_selector,
            });
        }
        Ok(bindings)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TmuxInputChunk {
    Literal(String),
    HexByte(u8),
}

fn split_tmux_input(bytes: &[u8]) -> Result<Vec<TmuxInputChunk>, TmuxError> {
    let mut chunks = Vec::new();
    let mut literal = String::new();
    let mut index = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        if is_tmux_hex_byte(byte) {
            flush_literal(&mut chunks, &mut literal);
            chunks.push(TmuxInputChunk::HexByte(byte));
            index += 1;
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

fn send_literal_keys_args(pane: &TmuxPaneId, literal: &str) -> Vec<String> {
    vec![
        "send-keys".to_string(),
        "-l".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        literal.to_string(),
    ]
}

fn send_hex_key_args(pane: &TmuxPaneId, byte: u8) -> Vec<String> {
    vec![
        "send-keys".to_string(),
        "-H".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        format!("{byte:02x}"),
    ]
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
        "-O".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        command.to_string(),
    ]
}

fn set_session_environment_args(session_name: &str, key: &str, value: &str) -> Vec<String> {
    vec![
        "set-environment".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        key.to_string(),
        value.to_string(),
    ]
}

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
        clear_pane_pipe_args, resize_pane_args, send_hex_key_args, send_literal_keys_args,
        set_pane_pipe_args, set_session_environment_args, split_tmux_input,
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
            vec![
                TmuxInputChunk::HexByte(0x1b),
                TmuxInputChunk::Literal("[A".to_string())
            ]
        );
    }

    #[test]
    fn remote_tmux_args_use_native_send_resize_and_pipe_primitives() {
        assert_eq!(
            send_literal_keys_args(&TmuxPaneId::new("%4"), "你好"),
            vec!["send-keys", "-l", "-t", "%4", "你好"]
        );
        assert_eq!(
            send_hex_key_args(&TmuxPaneId::new("%4"), 0x1b),
            vec!["send-keys", "-H", "-t", "%4", "1b"]
        );
        assert_eq!(
            resize_pane_args(&TmuxPaneId::new("%4"), 120, 40),
            vec!["resize-pane", "-t", "%4", "-x", "120", "-y", "40"]
        );
        assert_eq!(
            clear_pane_pipe_args(&TmuxPaneId::new("%4")),
            vec!["pipe-pane", "-t", "%4"]
        );
        assert_eq!(
            set_pane_pipe_args(&TmuxPaneId::new("%4"), "echo bridge"),
            vec!["pipe-pane", "-O", "-t", "%4", "echo bridge"]
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
