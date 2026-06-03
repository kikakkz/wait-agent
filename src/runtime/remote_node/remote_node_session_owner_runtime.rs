use crate::cli::{RemoteNetworkConfig, RemoteTargetPublicationSenderCommand};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_node_ingress_server_runtime::remote_node_ingress_owner_socket_path;
use crate::runtime::remote_target_publication_runtime::{
    drain_pending_publication_sender_commands, read_publication_sender_command,
    remote_target_publication_sender_socket_path, PublicationSenderCommand,
    RemoteTargetPublicationRuntime,
};
use crate::runtime::remote_target_publication_transport_runtime::RemoteTargetPublicationTransportRuntime;
use std::collections::HashMap;
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

mod owner_helpers;
pub(crate) use owner_helpers::*;

pub struct RemoteNodeSessionOwnerRuntime {
    publication_runtime: RemoteTargetPublicationRuntime,
    network: RemoteNetworkConfig,
    current_executable: PathBuf,
}

impl RemoteNodeSessionOwnerRuntime {
    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            publication_runtime: RemoteTargetPublicationRuntime::from_build_env_with_network(
                network.clone(),
            )?,
            network,
            current_executable: std::env::current_exe().map_err(|error| {
                LifecycleError::Io(
                    "failed to locate current waitagent executable".to_string(),
                    error,
                )
            })?,
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
        let publication_socket_path =
            crate::runtime::remote_target_publication_transport_runtime::remote_target_publication_socket_path(
                &command.socket_name,
            );
        let mut transports = HashMap::<String, RemoteTargetPublicationTransportRuntime>::new();
        let mut live_sessions = HashMap::<String, Arc<LiveSessionRoute>>::new();
        let mut authority_sessions = HashMap::<String, SharedAuthoritySession>::new();
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            let Ok(first_command) = read_publication_sender_command(&mut stream) else {
                continue;
            };
            let mut commands: Vec<PublicationSenderCommand> = vec![first_command];
            drain_pending_publication_sender_commands(&listener, &mut commands)?;
            reap_inactive_authority_sessions(&mut authority_sessions);
            for sender_command in commands {
                match sender_command {
                    PublicationSenderCommand::RegisterLiveSession {
                        target_session_name,
                        authority_id,
                        target_id,
                        transport_socket_path,
                    } => {
                        ensure_live_session_route(
                            &self.current_executable,
                            &command.socket_name,
                            &target_session_name,
                            &authority_id,
                            &target_id,
                            &transport_socket_path,
                            &self.network,
                            &self.publication_runtime,
                            &mut live_sessions,
                            &mut authority_sessions,
                        )?;
                    }
                    PublicationSenderCommand::UnregisterLiveSession {
                        target_session_name,
                    } => {
                        stop_live_session_route(
                            &target_session_name,
                            &mut live_sessions,
                            &mut authority_sessions,
                        );
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
                        task_state,
                    } => {
                        if let Some(source_session_name) = source_session_name.as_deref() {
                            let target_id =
                                format!("remote-peer:{authority_id}:{transport_session_id}");
                            let transport_socket_path =
                                remote_node_ingress_owner_socket_path(&self.network)
                                    .to_string_lossy()
                                    .into_owned();
                            ensure_live_session_route(
                                &self.current_executable,
                                &command.socket_name,
                                source_session_name,
                                &authority_id,
                                &target_id,
                                &transport_socket_path,
                                &self.network,
                                &self.publication_runtime,
                                &mut live_sessions,
                                &mut authority_sessions,
                            )?;
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
                                task_state,
                            };
                            if dispatch_live_publication(
                                source_session_name,
                                &live_command,
                                &mut live_sessions,
                                &mut authority_sessions,
                            ) {
                                continue;
                            }
                        }
                        dispatch_publication_sender_command(
                            &publication_socket_path,
                            &mut transports,
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
                            },
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
                                &mut authority_sessions,
                            ) {
                                continue;
                            }
                        }
                        dispatch_publication_sender_command(
                            &publication_socket_path,
                            &mut transports,
                            PublicationSenderCommand::ExitTarget {
                                authority_id,
                                transport_session_id,
                                source_session_name,
                            },
                        )?;
                    }
                }
            }
        }
        for target_session_name in live_sessions.keys().cloned().collect::<Vec<_>>() {
            stop_live_session_route(
                &target_session_name,
                &mut live_sessions,
                &mut authority_sessions,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod remote_node_session_owner_runtime_test;
