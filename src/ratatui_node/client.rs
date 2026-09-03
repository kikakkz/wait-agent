use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::platform::local_ipc::LocalStream;
use base64::{engine::general_purpose, Engine as _};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::client_writer::{ClientWriterHandle, ClientWriterRequest};
use super::runtime::SharedState;
use super::state_event::{ClientCommand, StateEvent};

pub(crate) static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// Lightweight registry entry used only for cleanup signaling.
///
/// Actual socket streams are owned by `ClientWriter`; this handle just lets the
/// server know a client is still attached without holding any I/O resources.
pub(crate) struct ClientHandle {
    pub(crate) id: u64,
    pub(crate) removed: Arc<AtomicBool>,
}

pub(crate) fn handle_client(
    stream: LocalStream,
    client_id: u64,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    shared: Arc<SharedState>,
    client_writer: ClientWriterHandle,
) -> Result<(), LifecycleError> {
    ERROR_LOG.log(format!("[ratatui-node] client {client_id} connected"));
    let removed = Arc::new(AtomicBool::new(false));

    let reader = stream.try_clone().map_err(|error| {
        LifecycleError::Io("failed to clone ratatui client stream".to_string(), error)
    })?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read the first line to decide whether this is a TUI attach or a
    // one-shot control command.
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => {
            return Ok(());
        }
        Ok(_) => {}
    }

    let trimmed = line.trim();
    ERROR_LOG.log(format!(
        "[ratatui-node] client {client_id} first message: {trimmed}"
    ));

    // Register the socket with the single writer thread and, for TUI clients,
    // add a handle to the registry so disconnect can be tracked.
    let is_attach = trimmed == "ATTACH";
    if is_attach {
        client_writer.send(ClientWriterRequest::Register {
            client_id,
            stream,
            broadcast: true,
        });
        register_client_handle(client_id, removed.clone(), &clients);
        let _ = shared
            .state_sender()
            .send(StateEvent::ClientConnected { client_id });
    } else {
        // One-shot commands keep the original stream because the writer thread
        // does not know about this short-lived client yet. They only receive
        // their direct response, never snapshot broadcasts.
        client_writer.send(ClientWriterRequest::Register {
            client_id,
            stream,
            broadcast: false,
        });
    }

    if let Some(command) = parse_command(trimmed) {
        let _ = shared
            .state_sender()
            .send(StateEvent::ClientCommand { client_id, command });
    }

    // One-shot control commands do not join the long-lived client list and do
    // not read further messages.
    if !is_attach {
        return Ok(());
    }

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                ERROR_LOG.log_warn(format!("[ratatui-node] client {client_id} disconnected"));
                break;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed == "DETACH" {
                    break;
                }
                if let Some(command) = parse_command(trimmed) {
                    let _ = shared
                        .state_sender()
                        .send(StateEvent::ClientCommand { client_id, command });
                }
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-node] client {client_id} read error: {error:?}"
                ));
                break;
            }
        }
    }

    remove_client(client_id, &clients);
    let _ = shared
        .state_sender()
        .send(StateEvent::ClientDisconnected { client_id });
    Ok(())
}

fn parse_command(line: &str) -> Option<ClientCommand> {
    let trimmed = line.trim();
    match trimmed {
        "ATTACH" => Some(ClientCommand::Attach),
        "STATUS" => Some(ClientCommand::Status),
        "STOP" => Some(ClientCommand::Stop),
        "LIST_SESSIONS" => Some(ClientCommand::ListSessions),
        "CREATE_LOCAL_SESSION" => Some(ClientCommand::CreateLocalSession),
        "DETACH_ALL" => Some(ClientCommand::DetachAll),
        "CLEAR_PUBLIC" => Some(ClientCommand::SetPublic {
            endpoint: None,
            save: false,
        }),
        "SET_PUBLIC" => Some(ClientCommand::SetPublic {
            endpoint: None,
            save: false,
        }),
        _ => {
            if let Some(args) = trimmed.strip_prefix("ACTIVATE_TARGET ") {
                Some(ClientCommand::ActivateTarget {
                    target_id: args.to_string(),
                })
            } else if let Some(args) = trimmed.strip_prefix("CONNECT_REMOTE_HOST ") {
                Some(ClientCommand::ConnectRemoteHost {
                    profile_name: args.to_string(),
                })
            } else if let Some(args) = trimmed.strip_prefix("RESIZE ") {
                let mut parts = args.split_whitespace();
                let cols = parts.next().and_then(|v| v.parse().ok()).unwrap_or(80);
                let rows = parts.next().and_then(|v| v.parse().ok()).unwrap_or(24);
                Some(ClientCommand::Resize { cols, rows })
            } else if let Some(args) = trimmed.strip_prefix("INPUT ") {
                let mut parts = args.splitn(2, ' ');
                let target_id = parts.next().unwrap_or("").to_string();
                let encoded = parts.next().unwrap_or("");
                general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .map(|key| ClientCommand::Input { target_id, key })
            } else if let Some(args) = trimmed.strip_prefix("PASTE_TEXT ") {
                let mut parts = args.splitn(2, ' ');
                let target_id = parts.next().unwrap_or("").to_string();
                let encoded = parts.next().unwrap_or("");
                general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|text| ClientCommand::PasteText { target_id, text })
            } else if let Some(args) = trimmed.strip_prefix("PASTE_FILE ") {
                let mut parts = args.splitn(3, ' ');
                let target_id = parts.next().unwrap_or("").to_string();
                let filename_hint = parts.next().unwrap_or("paste").to_string();
                let encoded = parts.next().unwrap_or("");
                general_purpose::STANDARD.decode(encoded).ok().map(|bytes| {
                    ClientCommand::PasteFile {
                        target_id,
                        filename_hint,
                        bytes,
                    }
                })
            } else if let Some(args) = trimmed.strip_prefix("GET_HISTORY ") {
                Some(ClientCommand::GetHistory {
                    target_id: args.to_string(),
                })
            } else if let Some(args) = trimmed.strip_prefix("CREATE_REMOTE_SESSION ") {
                Some(ClientCommand::CreateRemoteSession {
                    authority_node_id: args.to_string(),
                })
            } else if let Some(args) = trimmed.strip_prefix("SET_PUBLIC ") {
                let trimmed_args = args.trim();
                let save = trimmed_args.ends_with(" SAVE");
                let endpoint_part = if save {
                    trimmed_args
                        .strip_suffix(" SAVE")
                        .unwrap_or(trimmed_args)
                        .trim()
                } else {
                    trimmed_args
                };
                if endpoint_part.is_empty() {
                    Some(ClientCommand::SetPublic {
                        endpoint: None,
                        save,
                    })
                } else {
                    Some(ClientCommand::SetPublic {
                        endpoint: Some(endpoint_part.to_string()),
                        save,
                    })
                }
            } else {
                trimmed
                    .strip_prefix("CLOSE_SESSION ")
                    .map(|args| ClientCommand::CloseSession {
                        target_id: args.to_string(),
                    })
            }
        }
    }
}

fn register_client_handle(
    client_id: u64,
    removed: Arc<AtomicBool>,
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
) {
    let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    guard.push(ClientHandle {
        id: client_id,
        removed,
    });
}

pub(crate) fn remove_client(client_id: u64, clients: &Arc<Mutex<Vec<ClientHandle>>>) {
    let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = guard.iter().position(|handle| handle.id == client_id) {
        let handle = guard.remove(pos);
        handle.removed.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_paste_text_command() {
        let text = "hello world\nline two";
        let encoded = general_purpose::STANDARD.encode(text.as_bytes());
        let line = format!("PASTE_TEXT local#9999:1 {encoded}");
        let command = parse_command(&line);
        assert!(
            matches!(
                command,
                Some(ClientCommand::PasteText {
                    ref target_id,
                    ref text,
                }) if target_id == "local#9999:1" && text == "hello world\nline two"
            ),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn parse_paste_file_command() {
        let bytes = b"file contents";
        let encoded = general_purpose::STANDARD.encode(bytes);
        let line = format!("PASTE_FILE local#9999:1 report.txt {encoded}");
        let command = parse_command(&line);
        assert!(
            matches!(
                command,
                Some(ClientCommand::PasteFile {
                    ref target_id,
                    ref filename_hint,
                    ref bytes,
                }) if target_id == "local#9999:1" && filename_hint == "report.txt" && bytes == b"file contents"
            ),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn parse_input_command_with_spaced_json() {
        let encoded =
            "eyJjb2RlIjogeyJraW5kIjogIkNoYXIiLCAidmFsdWUiOiAiZSJ9LCAibW9kaWZpZXJzIjoge319";
        let line = format!("INPUT local#9999:1 {encoded}");
        let command = parse_command(&line);
        assert!(
            matches!(
                command,
                Some(ClientCommand::Input {
                    ref target_id,
                    key: super::super::logical_key::LogicalKey { code: super::super::logical_key::KeyCode::Char('e'), .. }
                }) if target_id == "local#9999:1"
            ),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn parse_set_public_command_with_endpoint() {
        let command = parse_command("SET_PUBLIC nat.example:17474");
        assert!(
            matches!(
                command,
                Some(ClientCommand::SetPublic {
                    endpoint: Some(ref endpoint),
                    save: false,
                }) if endpoint == "nat.example:17474"
            ),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn parse_set_public_command_with_save_flag() {
        let command = parse_command("SET_PUBLIC nat.example:17474 SAVE");
        assert!(
            matches!(
                command,
                Some(ClientCommand::SetPublic {
                    endpoint: Some(ref endpoint),
                    save: true,
                }) if endpoint == "nat.example:17474"
            ),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn parse_set_public_command_without_endpoint_clears() {
        let command = parse_command("SET_PUBLIC");
        assert!(
            matches!(
                command,
                Some(ClientCommand::SetPublic {
                    endpoint: None,
                    save: false,
                })
            ),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn parse_clear_public_command() {
        let command = parse_command("CLEAR_PUBLIC");
        assert!(
            matches!(
                command,
                Some(ClientCommand::SetPublic {
                    endpoint: None,
                    save: false,
                })
            ),
            "unexpected command: {command:?}"
        );
    }
}
