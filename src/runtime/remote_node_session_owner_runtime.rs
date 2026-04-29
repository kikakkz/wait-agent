use crate::cli::RemoteTargetPublicationSenderCommand;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, write_control_plane_envelope,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_session_runtime::{
    RemoteNodeSessionError, RemoteNodeSessionRuntime,
};
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::runtime::remote_target_publication_runtime::{
    drain_pending_publication_sender_commands, read_publication_sender_command,
    remote_target_publication_sender_socket_path, PublicationSenderCommand,
    RemoteTargetPublicationRuntime,
};
use crate::runtime::remote_target_publication_transport_runtime::RemoteTargetPublicationTransportRuntime;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LIVE_AUTHORITY_SERVER_ID: &str = "waitagent-live-authority-owner";

struct LiveSessionRoute {
    session: Arc<RemoteNodeSessionRuntime>,
    socket_path: PathBuf,
    running: Arc<AtomicBool>,
}

pub struct RemoteNodeSessionOwnerRuntime {
    publication_runtime: RemoteTargetPublicationRuntime,
}

impl RemoteNodeSessionOwnerRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Ok(Self {
            publication_runtime: RemoteTargetPublicationRuntime::from_build_env()?,
        })
    }

    pub fn run_publication_sender(
        &self,
        command: RemoteTargetPublicationSenderCommand,
    ) -> Result<(), LifecycleError> {
        self.publication_runtime
            .ensure_publication_server_running(&command.socket_name)?;
        let socket_path = remote_target_publication_sender_socket_path(&command.socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(|error| {
            LifecycleError::Io(
                "failed to start remote node session owner".to_string(),
                error,
            )
        })?;
        let mut transports = HashMap::<String, RemoteTargetPublicationTransportRuntime>::new();
        let mut live_sessions = HashMap::<String, LiveSessionRoute>::new();
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            let Ok(first_command) = read_publication_sender_command(&mut stream) else {
                continue;
            };
            let mut commands: Vec<PublicationSenderCommand> = vec![first_command];
            drain_pending_publication_sender_commands(&listener, &mut commands)?;
            for sender_command in commands {
                match sender_command {
                    PublicationSenderCommand::RegisterLiveSession {
                        target_session_name,
                        authority_id,
                        transport_socket_path,
                    } => {
                        ensure_live_session_route(
                            &command.socket_name,
                            &target_session_name,
                            &authority_id,
                            &transport_socket_path,
                            &mut live_sessions,
                        )?;
                    }
                    PublicationSenderCommand::UnregisterLiveSession {
                        target_session_name,
                    } => {
                        stop_live_session_route(&target_session_name, &mut live_sessions);
                    }
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
                    } => {
                        if let Some(source_session_name) = source_session_name.as_deref() {
                            let live_command = PublicationSenderCommand::PublishTarget {
                                authority_id: authority_id.clone(),
                                transport_session_id: transport_session_id.clone(),
                                source_session_name: Some(source_session_name.to_string()),
                                selector: selector.clone(),
                                availability,
                                session_role,
                                workspace_key: workspace_key.clone(),
                                command_name: command_name.clone(),
                                current_path: current_path.clone(),
                                attached_clients,
                                window_count,
                            };
                            if dispatch_live_publication(
                                source_session_name,
                                &live_command,
                                &mut live_sessions,
                            ) {
                                continue;
                            }
                        }
                        self.publication_runtime
                            .process_publication_sender_command(
                                &command.socket_name,
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
                                },
                                &mut transports,
                            )?;
                    }
                    PublicationSenderCommand::ExitTarget {
                        authority_id,
                        transport_session_id,
                        source_session_name,
                    } => {
                        if let Some(source_session_name) = source_session_name.as_deref() {
                            let live_command = PublicationSenderCommand::ExitTarget {
                                authority_id: authority_id.clone(),
                                transport_session_id: transport_session_id.clone(),
                                source_session_name: Some(source_session_name.to_string()),
                            };
                            if dispatch_live_publication(
                                source_session_name,
                                &live_command,
                                &mut live_sessions,
                            ) {
                                continue;
                            }
                        }
                        self.publication_runtime
                            .process_publication_sender_command(
                                &command.socket_name,
                                PublicationSenderCommand::ExitTarget {
                                    authority_id,
                                    transport_session_id,
                                    source_session_name,
                                },
                                &mut transports,
                            )?;
                    }
                }
            }
        }
        for target_session_name in live_sessions.keys().cloned().collect::<Vec<_>>() {
            stop_live_session_route(&target_session_name, &mut live_sessions);
        }
        Ok(())
    }
}

pub(crate) fn live_authority_session_socket_path(
    socket_name: &str,
    target_session_name: &str,
) -> PathBuf {
    let hash = stable_socket_hash(&[socket_name, target_session_name]);
    std::env::temp_dir().join(format!("waitagent-live-authority-{hash}.sock"))
}

pub(crate) fn spawn_live_authority_session_bridge(
    socket_path: PathBuf,
    session: Arc<RemoteNodeSessionRuntime>,
    running: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let Ok(listener) = bind_live_authority_listener(&socket_path) else {
            return;
        };
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = bridge_live_authority_stream(stream, session.clone(), running.clone());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        let _ = fs::remove_file(&socket_path);
    });
}

fn ensure_live_session_route(
    socket_name: &str,
    target_session_name: &str,
    authority_id: &str,
    transport_socket_path: &str,
    live_sessions: &mut HashMap<String, LiveSessionRoute>,
) -> Result<(), LifecycleError> {
    stop_live_session_route(target_session_name, live_sessions);
    let session = Arc::new(
        RemoteNodeSessionRuntime::connect(transport_socket_path, authority_id).map_err(
            |error| {
                LifecycleError::Io(
                    "failed to connect live authority node session".to_string(),
                    io::Error::new(io::ErrorKind::Other, error.to_string()),
                )
            },
        )?,
    );
    let running = Arc::new(AtomicBool::new(true));
    let socket_path = live_authority_session_socket_path(socket_name, target_session_name);
    spawn_live_authority_session_bridge(socket_path.clone(), session.clone(), running.clone());
    live_sessions.insert(
        target_session_name.to_string(),
        LiveSessionRoute {
            session,
            socket_path,
            running,
        },
    );
    Ok(())
}

fn stop_live_session_route(
    target_session_name: &str,
    live_sessions: &mut HashMap<String, LiveSessionRoute>,
) {
    if let Some(route) = live_sessions.remove(target_session_name) {
        route.running.store(false, Ordering::Relaxed);
        route.session.shutdown();
        let _ = UnixStream::connect(&route.socket_path);
        let _ = fs::remove_file(route.socket_path);
    }
}

fn dispatch_live_publication(
    target_session_name: &str,
    command: &PublicationSenderCommand,
    live_sessions: &mut HashMap<String, LiveSessionRoute>,
) -> bool {
    let Some(route) = live_sessions.get(target_session_name) else {
        return false;
    };
    if route
        .session
        .send_publication_sender_command(command)
        .is_ok()
    {
        return true;
    }
    stop_live_session_route(target_session_name, live_sessions);
    false
}

fn bind_live_authority_listener(socket_path: &Path) -> Result<UnixListener, io::Error> {
    if socket_path.exists() {
        let _ = fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn bridge_live_authority_stream(
    mut host_stream: UnixStream,
    session: Arc<RemoteNodeSessionRuntime>,
    running: Arc<AtomicBool>,
) -> Result<(), RemoteNodeSessionError> {
    let _node_id = read_client_hello(&mut host_stream)?;
    write_server_hello(&mut host_stream, LIVE_AUTHORITY_SERVER_ID)?;
    let host_reader = host_stream.try_clone()?;
    let host_session = session.clone();
    let host_running = running.clone();
    let forward_host = thread::spawn(move || {
        let _ = forward_host_output_to_session(host_reader, host_session, host_running);
    });
    while running.load(Ordering::Relaxed) {
        let command = match session.recv_authority_command() {
            Ok(command) => command,
            Err(error) => {
                let _ = host_stream.shutdown(Shutdown::Both);
                let _ = forward_host.join();
                return Err(error);
            }
        };
        write_control_plane_envelope(&mut host_stream, &authority_command_envelope(command))?;
    }
    let _ = host_stream.shutdown(Shutdown::Both);
    let _ = forward_host.join();
    Ok(())
}

fn forward_host_output_to_session(
    mut host_reader: UnixStream,
    session: Arc<RemoteNodeSessionRuntime>,
    running: Arc<AtomicBool>,
) -> Result<(), RemoteNodeSessionError> {
    while running.load(Ordering::Relaxed) {
        let envelope = read_control_plane_envelope(&mut host_reader)?;
        match envelope.payload {
            ControlPlanePayload::TargetOutput(payload) => {
                session.send_target_output(
                    &payload.target_id,
                    payload.output_seq,
                    payload.stream,
                    payload.bytes_base64,
                )?;
            }
            other => {
                return Err(RemoteNodeSessionError::new(format!(
                    "unexpected live authority host payload `{}`",
                    other.message_type()
                )));
            }
        }
    }
    Ok(())
}

fn authority_command_envelope(
    command: RemoteAuthorityCommand,
) -> ProtocolEnvelope<ControlPlanePayload> {
    let payload = match command {
        RemoteAuthorityCommand::TargetInput(payload) => ControlPlanePayload::TargetInput(payload),
        RemoteAuthorityCommand::ApplyResize(payload) => ControlPlanePayload::ApplyResize(payload),
    };
    ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: format!("live-authority-{}", now_rfc3339_like()),
        message_type: payload.message_type(),
        timestamp: now_rfc3339_like(),
        sender_id: "waitagent-live-authority-owner".to_string(),
        correlation_id: None,
        target_id: None,
        attachment_id: None,
        console_id: None,
        payload,
    }
}

fn stable_socket_hash(values: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for value in values {
        for byte in value.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn now_rfc3339_like() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
}
