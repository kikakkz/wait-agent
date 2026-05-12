use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    SessionTransport,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::published_target_store::PublishedTargetStore;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, TargetPublishedPayload,
};
use crate::infra::tmux::{RemoteTargetPublicationBinding, TmuxSessionGateway, TmuxSocketName};
use crate::lifecycle::LifecycleError;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use base64::Engine;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str;
use std::thread;
use std::time::Duration;

pub(super) const PUBLICATION_SERVER_READY_RETRIES: usize = 20;
pub(super) const PUBLICATION_SERVER_READY_SLEEP: Duration = Duration::from_millis(25);
pub(super) const PUBLICATION_OWNER_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub(super) const PUBLICATION_GLOBAL_HOOKS: [&str; 4] = [
    "session-created",
    "session-closed",
    "client-attached",
    "client-detached",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SocketLifecyclePublicationAction {
    TargetedPublish,
    TargetedExit,
    FullReconcile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PublicationAgentCommand {
    FullReconcile,
    PublishSession {
        session_name: String,
    },
    ExitTarget {
        authority_id: String,
        transport_session_id: String,
        source_session_name: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublicationSenderCommand {
    RegisterLiveSession {
        target_session_name: String,
        authority_id: String,
        target_id: String,
        transport_socket_path: String,
    },
    UnregisterLiveSession {
        target_session_name: String,
    },
    PublishTarget {
        authority_id: String,
        transport_session_id: String,
        source_session_name: Option<String>,
        selector: Option<String>,
        availability: &'static str,
        session_role: Option<&'static str>,
        workspace_key: Option<String>,
        command_name: Option<String>,
        current_path: Option<String>,
        attached_clients: usize,
        window_count: usize,
        task_state: &'static str,
    },
    ExitTarget {
        authority_id: String,
        transport_session_id: String,
        source_session_name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicationOwnerCommand {
    Refresh,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct PublicationOwnerDrain {
    pub(super) refresh_requested: bool,
    pub(super) stop_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PublicationOwnerSnapshot {
    pub(super) authority_id: String,
    pub(super) transport_session_id: String,
    pub(super) selector: Option<String>,
    pub(super) availability: SessionAvailability,
    pub(super) workspace_key: Option<String>,
    pub(super) session_role: Option<WorkspaceSessionRole>,
    pub(super) attached_clients: usize,
    pub(super) window_count: usize,
    pub(super) command_name: Option<String>,
    pub(super) current_path: Option<PathBuf>,
}

pub(super) struct DiscoveredRemoteSessionEnvelopeEffect {
    pub(super) published_session: Option<ManagedSessionRecord>,
    pub(super) exited_session: Option<(String, String)>,
}

pub(super) fn remote_target_publication_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to update published remote target catalog".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

pub(super) fn remote_target_publication_server_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-target-publication-server".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

pub(super) fn remote_target_publication_agent_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-target-publication-agent".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

pub(crate) fn remote_target_publication_sender_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-target-publication-sender".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

pub(super) fn remote_target_publication_owner_args(
    socket_name: &str,
    target_session_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-target-publication-owner".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
            "--target-session-name".to_string(),
            target_session_name.to_string(),
        ],
        network,
    )
}

pub(super) fn publication_socket_hook_tmux_command(executable: &str, socket_name: &str) -> String {
    let hook_command = [
        shell_escape(executable),
        shell_escape("__socket-lifecycle-hook"),
        shell_escape("--socket-name"),
        shell_escape(socket_name),
        shell_escape("--hook-name"),
        shell_escape("#{hook}"),
        shell_escape("--session-name"),
        shell_escape("#{hook_session_name}"),
    ]
    .join(" ");
    format!(
        "run-shell -b {}",
        tmux_quote_argument(&format!("{hook_command} >/dev/null 2>&1"))
    )
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_quote_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(super) fn socket_lifecycle_publication_action(
    hook_name: Option<&str>,
) -> SocketLifecyclePublicationAction {
    match hook_name {
        Some("client-attached") | Some("client-detached") | Some("session-created") => {
            SocketLifecyclePublicationAction::TargetedPublish
        }
        Some("session-closed") => SocketLifecyclePublicationAction::TargetedExit,
        Some(_) | None => SocketLifecyclePublicationAction::FullReconcile,
    }
}

pub(super) fn publication_server_available(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

pub(super) fn publication_agent_available(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

pub(super) fn publication_sender_available(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

pub(super) fn publication_owner_available(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

pub(super) fn remote_target_publication_agent_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-publication-agent-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

pub(super) fn remote_target_publication_owner_socket_path(
    socket_name: &str,
    target_session_name: &str,
) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-publication-owner-{}-{}.sock",
        sanitize_path_component(socket_name),
        sanitize_path_component(target_session_name)
    ))
}

pub(crate) fn remote_target_publication_sender_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-publication-sender-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

pub(super) fn render_publication_agent_command(command: &PublicationAgentCommand) -> String {
    match command {
        PublicationAgentCommand::FullReconcile => "full_reconcile\n".to_string(),
        PublicationAgentCommand::PublishSession { session_name } => format!(
            "publish_session\t{}\n",
            base64::engine::general_purpose::STANDARD.encode(session_name.as_bytes())
        ),
        PublicationAgentCommand::ExitTarget {
            authority_id,
            transport_session_id,
            source_session_name,
        } => format!(
            "exit_target\t{}\t{}\t{}\n",
            base64::engine::general_purpose::STANDARD.encode(authority_id.as_bytes()),
            base64::engine::general_purpose::STANDARD.encode(transport_session_id.as_bytes()),
            encode_optional_agent_field(source_session_name.as_deref())
        ),
    }
}

pub(crate) fn render_publication_sender_command(command: &PublicationSenderCommand) -> String {
    match command {
        PublicationSenderCommand::RegisterLiveSession {
            target_session_name,
            authority_id,
            target_id,
            transport_socket_path,
        } => format!(
            "register_live_session\t{}\t{}\t{}\t{}\n",
            base64::engine::general_purpose::STANDARD.encode(target_session_name.as_bytes()),
            base64::engine::general_purpose::STANDARD.encode(authority_id.as_bytes()),
            base64::engine::general_purpose::STANDARD.encode(target_id.as_bytes()),
            base64::engine::general_purpose::STANDARD.encode(transport_socket_path.as_bytes())
        ),
        PublicationSenderCommand::UnregisterLiveSession {
            target_session_name,
        } => format!(
            "unregister_live_session\t{}\n",
            base64::engine::general_purpose::STANDARD.encode(target_session_name.as_bytes())
        ),
        PublicationSenderCommand::PublishTarget {
            authority_id,
            transport_session_id,
            source_session_name,
            selector,
            availability,
            session_role,
            workspace_key,
            command_name,
            current_path,
            attached_clients,
            window_count,
            task_state,
        } => format!(
            "publish_target\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            base64::engine::general_purpose::STANDARD.encode(authority_id.as_bytes()),
            base64::engine::general_purpose::STANDARD.encode(transport_session_id.as_bytes()),
            encode_optional_agent_field(source_session_name.as_deref()),
            encode_optional_agent_field(selector.as_deref()),
            availability,
            encode_optional_static_agent_field(*session_role),
            encode_optional_agent_field(workspace_key.as_deref()),
            encode_optional_agent_field(command_name.as_deref()),
            encode_optional_agent_field(current_path.as_deref()),
            attached_clients,
            window_count,
            task_state,
        ),
        PublicationSenderCommand::ExitTarget {
            authority_id,
            transport_session_id,
            source_session_name,
        } => format!(
            "exit_target\t{}\t{}\t{}\n",
            base64::engine::general_purpose::STANDARD.encode(authority_id.as_bytes()),
            base64::engine::general_purpose::STANDARD.encode(transport_session_id.as_bytes()),
            encode_optional_agent_field(source_session_name.as_deref())
        ),
    }
}

pub(super) fn render_publication_owner_command(command: PublicationOwnerCommand) -> &'static str {
    match command {
        PublicationOwnerCommand::Refresh => "refresh\n",
        PublicationOwnerCommand::Stop => "stop\n",
    }
}

pub(super) fn signal_publication_owner_command(
    socket_name: &str,
    target_session_name: &str,
    command: PublicationOwnerCommand,
) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(remote_target_publication_owner_socket_path(
        socket_name,
        target_session_name,
    ))
    .map_err(remote_target_publication_error)?;
    stream
        .write_all(render_publication_owner_command(command).as_bytes())
        .map_err(remote_target_publication_error)?;
    stream.flush().map_err(remote_target_publication_error)
}

pub(super) fn read_publication_agent_command(
    reader: &mut impl Read,
) -> Result<PublicationAgentCommand, LifecycleError> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(remote_target_publication_error)?;
    let line = str::from_utf8(&bytes)
        .map_err(remote_target_publication_error)?
        .trim();
    parse_publication_agent_command(line)
}

pub(crate) fn read_publication_sender_command(
    reader: &mut impl Read,
) -> Result<PublicationSenderCommand, LifecycleError> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(remote_target_publication_error)?;
    let line = str::from_utf8(&bytes)
        .map_err(remote_target_publication_error)?
        .trim();
    parse_publication_sender_command(line)
}

pub(super) fn parse_publication_agent_command(
    line: &str,
) -> Result<PublicationAgentCommand, LifecycleError> {
    let mut parts = line.split('\t');
    match parts.next().unwrap_or_default() {
        "full_reconcile" => Ok(PublicationAgentCommand::FullReconcile),
        "publish_session" => {
            let session_name =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("publish_session is missing session field".to_string())
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "publish_session contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationAgentCommand::PublishSession { session_name })
        }
        "exit_target" => {
            let authority_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("exit_target is missing authority field".to_string())
                })?)?;
            let transport_session_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("exit_target is missing session field".to_string())
                })?)?;
            let source_session_name =
                decode_optional_agent_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "exit_target is missing source session field".to_string(),
                    )
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "exit_target contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationAgentCommand::ExitTarget {
                authority_id,
                transport_session_id,
                source_session_name,
            })
        }
        other => Err(LifecycleError::Protocol(format!(
            "unsupported remote publication agent command `{other}`"
        ))),
    }
}

pub(super) fn parse_publication_sender_command(
    line: &str,
) -> Result<PublicationSenderCommand, LifecycleError> {
    let mut parts = line.split('\t');
    match parts.next().unwrap_or_default() {
        "register_live_session" => {
            let target_session_name =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "register_live_session is missing target session field".to_string(),
                    )
                })?)?;
            let authority_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "register_live_session is missing authority field".to_string(),
                    )
                })?)?;
            let target_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "register_live_session is missing target id field".to_string(),
                    )
                })?)?;
            let transport_socket_path =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "register_live_session is missing transport socket field".to_string(),
                    )
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "register_live_session contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationSenderCommand::RegisterLiveSession {
                target_session_name,
                authority_id,
                target_id,
                transport_socket_path,
            })
        }
        "unregister_live_session" => {
            let target_session_name =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "unregister_live_session is missing target session field".to_string(),
                    )
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "unregister_live_session contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationSenderCommand::UnregisterLiveSession {
                target_session_name,
            })
        }
        "publish_target" => {
            let authority_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing authority field".to_string(),
                    )
                })?)?;
            let transport_session_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("publish_target is missing session field".to_string())
                })?)?;
            let source_session_name =
                decode_optional_agent_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing source session field".to_string(),
                    )
                })?)?;
            let selector = decode_optional_agent_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing selector field".to_string())
            })?)?;
            let availability = parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing availability field".to_string())
            })?;
            let session_role =
                decode_optional_static_agent_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing session role field".to_string(),
                    )
                })?)?;
            let workspace_key = decode_optional_agent_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol(
                    "publish_target is missing workspace key field".to_string(),
                )
            })?)?;
            let command_name = decode_optional_agent_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing command name field".to_string())
            })?)?;
            let current_path = decode_optional_agent_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing current path field".to_string())
            })?)?;
            let attached_clients = parts
                .next()
                .ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing attached clients field".to_string(),
                    )
                })?
                .parse::<usize>()
                .map_err(remote_target_publication_error)?;
            let window_count = parts
                .next()
                .ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing window count field".to_string(),
                    )
                })?
                .parse::<usize>()
                .map_err(remote_target_publication_error)?;
            let task_state = parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing task state field".to_string())
            })?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "publish_target contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationSenderCommand::PublishTarget {
                authority_id,
                transport_session_id,
                source_session_name,
                selector,
                availability: SessionAvailability::parse(availability)
                    .ok_or_else(|| {
                        LifecycleError::Protocol(format!(
                            "unsupported publication sender availability `{availability}`"
                        ))
                    })?
                    .as_str(),
                session_role,
                workspace_key,
                command_name,
                current_path,
                attached_clients,
                window_count,
                task_state: ManagedSessionTaskState::parse(task_state)
                    .ok_or_else(|| {
                        LifecycleError::Protocol(format!(
                            "unsupported publication sender task state `{task_state}`"
                        ))
                    })?
                    .as_str(),
            })
        }
        "exit_target" => {
            let authority_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("exit_target is missing authority field".to_string())
                })?)?;
            let transport_session_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("exit_target is missing session field".to_string())
                })?)?;
            let source_session_name =
                decode_optional_agent_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "exit_target is missing source session field".to_string(),
                    )
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "exit_target contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationSenderCommand::ExitTarget {
                authority_id,
                transport_session_id,
                source_session_name,
            })
        }
        other => Err(LifecycleError::Protocol(format!(
            "unsupported remote publication sender command `{other}`"
        ))),
    }
}

fn decode_publication_agent_string_field(value: &str) -> Result<String, LifecycleError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(remote_target_publication_error)?;
    String::from_utf8(bytes).map_err(remote_target_publication_error)
}

fn encode_optional_agent_field(value: Option<&str>) -> String {
    value
        .map(|value| base64::engine::general_purpose::STANDARD.encode(value.as_bytes()))
        .unwrap_or_else(|| "~".to_string())
}

fn encode_optional_static_agent_field(value: Option<&'static str>) -> String {
    value
        .map(|value| base64::engine::general_purpose::STANDARD.encode(value.as_bytes()))
        .unwrap_or_else(|| "~".to_string())
}

fn decode_optional_agent_field(value: &str) -> Result<Option<String>, LifecycleError> {
    if value == "~" {
        return Ok(None);
    }
    decode_publication_agent_string_field(value).map(Some)
}

fn decode_optional_static_agent_field(value: &str) -> Result<Option<&'static str>, LifecycleError> {
    decode_optional_agent_field(value)?
        .map(|value| {
            WorkspaceSessionRole::parse(&value)
                .map(|role| role.as_str())
                .ok_or_else(|| {
                    LifecycleError::Protocol(format!(
                        "unsupported publication sender session role `{value}`"
                    ))
                })
        })
        .transpose()
}

pub(super) fn drain_pending_publication_agent_commands(
    listener: &UnixListener,
    commands: &mut Vec<PublicationAgentCommand>,
) -> Result<(), LifecycleError> {
    listener
        .set_nonblocking(true)
        .map_err(remote_target_publication_error)?;
    let result = drain_pending_publication_agent_commands_nonblocking(listener, commands);
    let reset = listener
        .set_nonblocking(false)
        .map_err(remote_target_publication_error);
    result?;
    reset
}

fn drain_pending_publication_agent_commands_nonblocking(
    listener: &UnixListener,
    commands: &mut Vec<PublicationAgentCommand>,
) -> Result<(), LifecycleError> {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Ok(command) = read_publication_agent_command(&mut stream) {
                    commands.push(command);
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(remote_target_publication_error(error)),
        }
    }
}

pub(crate) fn drain_pending_publication_sender_commands(
    listener: &UnixListener,
    commands: &mut Vec<PublicationSenderCommand>,
) -> Result<(), LifecycleError> {
    listener
        .set_nonblocking(true)
        .map_err(remote_target_publication_error)?;
    let result = drain_pending_publication_sender_commands_nonblocking(listener, commands);
    let reset = listener
        .set_nonblocking(false)
        .map_err(remote_target_publication_error);
    result?;
    reset
}

fn drain_pending_publication_sender_commands_nonblocking(
    listener: &UnixListener,
    commands: &mut Vec<PublicationSenderCommand>,
) -> Result<(), LifecycleError> {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Ok(command) = read_publication_sender_command(&mut stream) {
                    commands.push(command);
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(remote_target_publication_error(error)),
        }
    }
}

pub(super) fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn publication_owner_snapshot(
    binding: &RemoteTargetPublicationBinding,
    local_target: &ManagedSessionRecord,
) -> PublicationOwnerSnapshot {
    PublicationOwnerSnapshot {
        authority_id: binding.authority_id.clone(),
        transport_session_id: binding.transport_session_id.clone(),
        selector: binding
            .selector
            .clone()
            .or_else(|| local_target.selector.clone()),
        availability: local_target.availability,
        workspace_key: local_target.workspace_key.clone(),
        session_role: local_target.session_role,
        attached_clients: local_target.attached_clients,
        window_count: local_target.window_count,
        command_name: local_target.command_name.clone(),
        current_path: local_target.current_path.clone(),
    }
}

pub(super) fn publication_target_identity_changed(
    previous: &PublicationOwnerSnapshot,
    current: &PublicationOwnerSnapshot,
) -> bool {
    previous.authority_id != current.authority_id
        || previous.transport_session_id != current.transport_session_id
}

pub(super) fn published_remote_target_from_local(
    binding: &RemoteTargetPublicationBinding,
    local_target: &ManagedSessionRecord,
) -> ManagedSessionRecord {
    ManagedSessionRecord {
        address: ManagedSessionAddress::remote_peer(
            binding.authority_id.clone(),
            binding.transport_session_id.clone(),
        ),
        selector: binding
            .selector
            .clone()
            .or_else(|| local_target.selector.clone()),
        availability: local_target.availability,
        workspace_dir: None,
        workspace_key: local_target.workspace_key.clone(),
        session_role: local_target.session_role,
        opened_by: Vec::new(),
        attached_clients: local_target.attached_clients,
        window_count: local_target.window_count,
        command_name: local_target.command_name.clone(),
        current_path: local_target.current_path.clone(),
        task_state: local_target.task_state,
    }
}

pub(super) fn parse_publication_owner_command(
    line: &str,
) -> Result<Option<PublicationOwnerCommand>, LifecycleError> {
    match line.trim() {
        "" => Ok(None),
        "refresh" => Ok(Some(PublicationOwnerCommand::Refresh)),
        "stop" => Ok(Some(PublicationOwnerCommand::Stop)),
        other => Err(LifecycleError::Protocol(format!(
            "unsupported remote publication owner command `{other}`"
        ))),
    }
}

pub(super) fn drain_publication_owner_commands(
    listener: &UnixListener,
) -> Result<PublicationOwnerDrain, LifecycleError> {
    let mut drain = PublicationOwnerDrain::default();
    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                let mut buffer = String::new();
                stream
                    .read_to_string(&mut buffer)
                    .map_err(remote_target_publication_error)?;
                match parse_publication_owner_command(&buffer)? {
                    Some(PublicationOwnerCommand::Refresh) => drain.refresh_requested = true,
                    Some(PublicationOwnerCommand::Stop) => drain.stop_requested = true,
                    None => {}
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(drain),
            Err(error) => return Err(remote_target_publication_error(error)),
        }
    }
}

pub(super) fn apply_publication_envelope(
    store: &PublishedTargetStore,
    source_socket_name: &str,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<bool, LifecycleError> {
    match &envelope.payload {
        ControlPlanePayload::TargetPublished(payload) => {
            let target = published_remote_target_record_from_payload(&envelope.sender_id, payload)?;
            store
                .upsert_target_from_source(
                    source_socket_name,
                    payload.source_session_name.as_deref(),
                    &target,
                )
                .map_err(remote_target_publication_error)
        }
        ControlPlanePayload::TargetExited(payload) => store
            .remove_target_from_source(
                source_socket_name,
                payload.source_session_name.as_deref(),
                &envelope.sender_id,
                &payload.transport_session_id,
            )
            .map_err(remote_target_publication_error),
        other => Err(LifecycleError::Protocol(format!(
            "unexpected remote target publication payload `{}`",
            other.message_type()
        ))),
    }
}

pub(super) fn discovered_remote_session_from_envelope(
    authority_id: &str,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<DiscoveredRemoteSessionEnvelopeEffect, LifecycleError> {
    match &envelope.payload {
        ControlPlanePayload::TargetPublished(payload) => {
            Ok(DiscoveredRemoteSessionEnvelopeEffect {
                published_session: Some(published_remote_target_record_from_payload(
                    &envelope.sender_id,
                    payload,
                )?),
                exited_session: None,
            })
        }
        ControlPlanePayload::TargetExited(payload) => Ok(DiscoveredRemoteSessionEnvelopeEffect {
            published_session: None,
            exited_session: Some((
                authority_id.to_string(),
                payload.transport_session_id.clone(),
            )),
        }),
        _ => Ok(DiscoveredRemoteSessionEnvelopeEffect {
            published_session: None,
            exited_session: None,
        }),
    }
}

pub(super) fn mark_target_offline_in_store(
    store: &PublishedTargetStore,
    socket_name: &str,
    session_name: &str,
    target_id: &str,
) -> Result<bool, LifecycleError> {
    let records = store
        .list_records_for_source_binding(socket_name, session_name)
        .map_err(remote_target_publication_error)?;
    let mut changed = false;
    for record in records {
        if record.target.address.id().as_str() != target_id {
            continue;
        }
        let mut offline_target = record.target.clone();
        offline_target.availability = SessionAvailability::Offline;
        changed |= store
            .upsert_target_from_source(socket_name, Some(session_name), &offline_target)
            .map_err(remote_target_publication_error)?;
    }
    Ok(changed)
}

pub(super) fn spawn_socket_chrome_refresh(
    current_executable: &std::path::Path,
    socket_name: &str,
) -> Result<(), LifecycleError> {
    Command::new(current_executable)
        .args(chrome_refresh_socket_args(socket_name))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(remote_target_publication_error)
}

pub(super) fn live_workspace_socket_names_from_sessions(
    sessions: &[ManagedSessionRecord],
) -> Vec<String> {
    let mut socket_names = BTreeSet::new();
    for session in sessions {
        if session.address.transport() != &SessionTransport::LocalTmux
            || !session.is_workspace_chrome()
        {
            continue;
        }
        socket_names.insert(session.address.server_id().to_string());
    }
    socket_names.into_iter().collect()
}

pub(super) fn is_publishable_discovered_remote_session(session: &ManagedSessionRecord) -> bool {
    session.address.transport() == &SessionTransport::RemotePeer && session.is_target_host()
}

pub(super) fn chrome_refresh_socket_args(socket_name: &str) -> Vec<String> {
    vec![
        "__chrome-refresh-socket".to_string(),
        "--socket-name".to_string(),
        socket_name.to_string(),
    ]
}

pub(super) fn published_remote_target_record_from_payload(
    authority_id: &str,
    payload: &TargetPublishedPayload,
) -> Result<ManagedSessionRecord, LifecycleError> {
    let availability = SessionAvailability::parse(payload.availability).ok_or_else(|| {
        LifecycleError::Protocol(format!(
            "unsupported remote target availability `{}`",
            payload.availability
        ))
    })?;
    let session_role = payload
        .session_role
        .map(|value| {
            WorkspaceSessionRole::parse(value).ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "unsupported remote target session role `{value}`"
                ))
            })
        })
        .transpose()?;

    Ok(ManagedSessionRecord {
        address: ManagedSessionAddress::remote_peer(
            authority_id,
            payload.transport_session_id.clone(),
        ),
        selector: payload.selector.clone(),
        availability,
        workspace_dir: None,
        workspace_key: payload.workspace_key.clone(),
        session_role,
        opened_by: Vec::new(),
        attached_clients: payload.attached_clients,
        window_count: payload.window_count,
        command_name: payload.command_name.clone(),
        current_path: payload.current_path.as_ref().map(PathBuf::from),
        task_state: ManagedSessionTaskState::parse(payload.task_state).ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "unsupported remote target task state `{}`",
                payload.task_state
            ))
        })?,
    })
}

pub(crate) fn ensure_publication_owner_process_running(
    current_executable: &std::path::Path,
    socket_name: &str,
    target_session_name: &str,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    let socket_path = remote_target_publication_owner_socket_path(socket_name, target_session_name);
    if publication_owner_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    spawn_waitagent_sidecar(
        current_executable,
        remote_target_publication_owner_args(socket_name, target_session_name, network),
    )
    .map_err(remote_target_publication_error)?;

    for _ in 0..PUBLICATION_SERVER_READY_RETRIES {
        if publication_owner_available(&socket_path) {
            return Ok(());
        }
        thread::sleep(PUBLICATION_SERVER_READY_SLEEP);
    }

    Err(LifecycleError::Protocol(format!(
        "remote target publication owner for socket `{socket_name}` session `{target_session_name}` did not become ready"
    )))
}

pub(crate) fn ensure_publication_sender_process_running(
    current_executable: &std::path::Path,
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    let socket_path = remote_target_publication_sender_socket_path(socket_name);
    if publication_sender_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    spawn_waitagent_sidecar(
        current_executable,
        remote_target_publication_sender_args(socket_name, network),
    )
    .map_err(remote_target_publication_error)?;

    for _ in 0..PUBLICATION_SERVER_READY_RETRIES {
        if publication_sender_available(&socket_path) {
            return Ok(());
        }
        thread::sleep(PUBLICATION_SERVER_READY_SLEEP);
    }

    Err(LifecycleError::Protocol(format!(
        "remote target publication sender for socket `{socket_name}` did not become ready"
    )))
}

pub(crate) fn signal_publication_sender_live_session_registered(
    socket_name: &str,
    target_session_name: &str,
    authority_id: &str,
    target_id: &str,
    transport_socket_path: &str,
) -> Result<(), LifecycleError> {
    signal_publication_sender_command(
        socket_name,
        PublicationSenderCommand::RegisterLiveSession {
            target_session_name: target_session_name.to_string(),
            authority_id: authority_id.to_string(),
            target_id: target_id.to_string(),
            transport_socket_path: transport_socket_path.to_string(),
        },
    )
}

pub(crate) fn signal_publication_sender_live_session_unregistered(
    socket_name: &str,
    target_session_name: &str,
) -> Result<(), LifecycleError> {
    signal_publication_sender_command(
        socket_name,
        PublicationSenderCommand::UnregisterLiveSession {
            target_session_name: target_session_name.to_string(),
        },
    )
}

pub(crate) fn signal_publication_target_published(
    socket_name: &str,
    authority_id: &str,
    target: &ManagedSessionRecord,
    source_session_name: Option<&str>,
) -> Result<(), LifecycleError> {
    signal_publication_sender_command(
        socket_name,
        PublicationSenderCommand::PublishTarget {
            authority_id: authority_id.to_string(),
            transport_session_id: target.address.session_id().to_string(),
            source_session_name: source_session_name.map(str::to_string),
            selector: target.selector.clone(),
            availability: target.availability.as_str(),
            session_role: target.session_role.map(|role| role.as_str()),
            workspace_key: target.workspace_key.clone(),
            command_name: target.command_name.clone(),
            current_path: target
                .current_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            attached_clients: target.attached_clients,
            window_count: target.window_count,
            task_state: target.task_state.as_str(),
        },
    )
}

pub(crate) fn signal_publication_target_exited(
    socket_name: &str,
    authority_id: &str,
    transport_session_id: &str,
    source_session_name: Option<&str>,
) -> Result<(), LifecycleError> {
    signal_publication_sender_command(
        socket_name,
        PublicationSenderCommand::ExitTarget {
            authority_id: authority_id.to_string(),
            transport_session_id: transport_session_id.to_string(),
            source_session_name: source_session_name.map(str::to_string),
        },
    )
}

pub(crate) fn signal_publication_sender_command(
    socket_name: &str,
    command: PublicationSenderCommand,
) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(remote_target_publication_sender_socket_path(socket_name))
        .map_err(remote_target_publication_error)?;
    stream
        .write_all(render_publication_sender_command(&command).as_bytes())
        .map_err(remote_target_publication_error)?;
    stream.flush().map_err(remote_target_publication_error)
}

/// Decoupled publication operations using only TmuxSessionGateway trait methods.
/// These replace the methods that were previously on EmbeddedTmuxBackend.

pub(crate) fn bind_publication_on_socket(
    gateway: &impl TmuxSessionGateway<Error = crate::infra::tmux::TmuxError>,
    socket_name: &str,
    target_session_name: &str,
    authority_id: &str,
    transport_session_id: &str,
    selector: Option<&str>,
) -> Result<(), crate::infra::tmux::TmuxError> {
    let socket = TmuxSocketName::new(socket_name);
    gateway.set_session_environment(
        &socket,
        target_session_name,
        crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV,
        authority_id,
    )?;
    gateway.set_session_environment(
        &socket,
        target_session_name,
        crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV,
        transport_session_id,
    )?;
    match selector {
        Some(selector) => gateway.set_session_environment(
            &socket,
            target_session_name,
            crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV,
            selector,
        )?,
        None => gateway.unset_session_environment(
            &socket,
            target_session_name,
            crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV,
        )?,
    }
    Ok(())
}

pub(crate) fn unbind_publication_on_socket(
    gateway: &impl TmuxSessionGateway<Error = crate::infra::tmux::TmuxError>,
    socket_name: &str,
    target_session_name: &str,
) -> Result<(), crate::infra::tmux::TmuxError> {
    let socket = TmuxSocketName::new(socket_name);
    for key in [
        crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV,
        crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV,
        crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV,
    ] {
        gateway.unset_session_environment(&socket, target_session_name, key)?;
    }
    Ok(())
}

pub(crate) fn list_publication_bindings_on_socket(
    gateway: &impl TmuxSessionGateway<Error = crate::infra::tmux::TmuxError>,
    socket_name: &TmuxSocketName,
) -> Result<Vec<RemoteTargetPublicationBinding>, crate::infra::tmux::TmuxError> {
    let sessions = gateway.list_sessions_on_socket(socket_name)?;
    let mut bindings = Vec::new();
    for session in sessions {
        if session.session_role != Some(WorkspaceSessionRole::TargetHost) {
            continue;
        }
        let env_vars =
            gateway.show_session_environment(socket_name, session.address.session_id())?;
        let mut authority_id = None;
        let mut transport_session_id = None;
        let mut selector = None;
        for (key, value) in &env_vars {
            if key.as_str() == crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV {
                authority_id = Some(value.clone());
            } else if key.as_str()
                == crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV
            {
                transport_session_id = Some(value.clone());
            } else if key.as_str() == crate::infra::tmux::WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV
            {
                selector = Some(value.clone());
            }
        }
        let Some(authority_id) = authority_id else {
            continue;
        };
        let Some(transport_session_id) = transport_session_id else {
            continue;
        };
        bindings.push(RemoteTargetPublicationBinding {
            socket_name: socket_name.as_str().to_string(),
            target_session_name: session.address.session_id().to_string(),
            authority_id,
            transport_session_id,
            selector,
        });
    }
    Ok(bindings)
}
