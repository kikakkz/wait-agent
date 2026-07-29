use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, RemoteTargetPublicationBinding, TmuxSessionGateway, TmuxSocketName,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::network_state_runtime::recover_network_config_for_socket;
use crate::runtime::remote_publication::remote_target_publication_runtime::{
    bind_publication_on_socket, ensure_publication_owner_process_running,
    ensure_publication_sender_process_running, list_publication_bindings_on_socket,
    live_workspace_socket_names_from_sessions, publication_agent_available,
    publication_server_available, publication_socket_hook_tmux_command, remote_target_exited_args,
    remote_target_publication_agent_args, remote_target_publication_agent_socket_path,
    remote_target_publication_error, remote_target_publication_server_args,
    spawn_socket_chrome_refresh, unbind_publication_on_socket, PUBLICATION_GLOBAL_HOOKS,
    PUBLICATION_SERVER_READY_RETRIES, PUBLICATION_SERVER_READY_SLEEP,
};
use crate::runtime::remote_target_publication_transport_runtime::remote_target_publication_socket_path;
use crate::runtime::remote_workspace_socket_registry_runtime::{
    live_workspace_socket_names_for_network, retain_live_workspace_socket_names_for_network,
    RemoteWorkspaceSocketRegistryRuntime,
};
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use std::fs;
use std::path::Path;
use std::thread;

const WAITAGENT_PANE_ROLE_OPTION: &str = "@waitagent_pane_role";
const WAITAGENT_PANE_ROLE_CONTENT: &str = "content";
const WAITAGENT_PANE_SESSION_INSTANCE_OPTION: &str = "@waitagent_session_instance_id";

/// Backend abstraction for `RemoteTargetPublicationRuntime`.
///
/// The publication runtime is responsible for the remote-target protocol:
/// accepting discovered remote sessions, signalling exits, refreshing local
/// views, and managing the bindings that publish local target hosts. This
/// trait hides the difference between the traditional tmux-backed deployment
/// (where every workspace is a tmux server and sidecars are spawned as
/// separate processes) and the single-process ratatui server model (where
/// sessions live in an in-memory catalog and snapshots are broadcast to TUI
/// clients).
pub trait RemoteTargetPublicationBackend: Clone + Send + Sync + 'static {
    /// Return the names of live local workspaces/sockets that the publication
    /// runtime should refresh when the remote catalog changes.
    fn live_workspace_socket_names(
        &self,
        network: &RemoteNetworkConfig,
    ) -> Result<Vec<String>, LifecycleError>;

    /// Return whether the given workspace/socket is currently live.
    fn socket_is_live(&self, socket_name: &str) -> bool;

    /// List all sessions belonging to a workspace/socket.
    fn list_sessions_on_socket(
        &self,
        socket_name: &str,
    ) -> Result<Vec<ManagedSessionRecord>, LifecycleError>;

    /// Find the remote-publication binding for a specific target session, if
    /// any.
    fn find_publication_binding(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<Option<RemoteTargetPublicationBinding>, LifecycleError>;

    /// List all remote-publication bindings on a workspace/socket.
    fn list_publication_bindings(
        &self,
        socket_name: &str,
    ) -> Result<Vec<RemoteTargetPublicationBinding>, LifecycleError>;

    /// Attach publication metadata to a local target session.
    fn bind_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        transport_session_id: &str,
        selector: Option<&str>,
    ) -> Result<(), LifecycleError>;

    /// Remove publication metadata from a local target session.
    fn unbind_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;

    /// Return true if a session still has a live content pane.
    fn live_content_pane_for_session(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<bool, LifecycleError>;

    /// Ensure that the lifecycle hooks required by the publication runtime are
    /// installed on the workspace/socket.
    fn ensure_publication_hooks(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError>;

    /// Notify a workspace/session that a remote target has exited.
    fn signal_remote_target_exited(
        &self,
        socket_name: &str,
        session_name: &str,
        target: &str,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Notify all workspace chrome sessions on a socket that a remote target
    /// has exited. Returns the number of sessions that were signalled.
    fn signal_remote_target_exited_to_workspace(
        &self,
        socket_name: &str,
        target: &str,
        executable: &Path,
    ) -> Result<usize, LifecycleError>;

    /// Refresh the local workspace chrome after publication changes.
    fn refresh_workspace_socket(
        &self,
        socket_name: &str,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Ensure the publication server sidecar process is running.
    fn ensure_publication_server_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Ensure the publication agent sidecar process is running.
    fn ensure_publication_agent_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Ensure the publication sender sidecar process is running.
    fn ensure_publication_sender_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Ensure the publication owner sidecar process is running for a target
    /// session.
    fn ensure_publication_owner_running(
        &self,
        socket_name: &str,
        target_session_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError>;
}

/// Tmux-backed implementation of `RemoteTargetPublicationBackend`.
///
/// This preserves the legacy behavior: workspace sockets are tmux sockets,
/// bindings are stored as session environment variables, hooks are installed
/// as tmux global hooks, and every publication service runs as a separate
/// waitagent sidecar process.
#[derive(Clone)]
pub struct TmuxRemoteTargetPublicationBackend {
    local_tmux: EmbeddedTmuxBackend,
}

impl TmuxRemoteTargetPublicationBackend {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Ok(Self {
            local_tmux: EmbeddedTmuxBackend::from_build_env()
                .map_err(remote_target_publication_error)?,
        })
    }
}

impl RemoteTargetPublicationBackend for TmuxRemoteTargetPublicationBackend {
    fn live_workspace_socket_names(
        &self,
        network: &RemoteNetworkConfig,
    ) -> Result<Vec<String>, LifecycleError> {
        let registered_sockets = live_workspace_socket_names_for_network(network)?;
        if !registered_sockets.is_empty()
            || RemoteWorkspaceSocketRegistryRuntime::new(network.clone()).registry_exists()
        {
            let live_sockets =
                retain_live_workspace_socket_names_for_network(network, |socket_name| {
                    self.socket_is_live(socket_name)
                })?;
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] publication_live_sockets_registry count={} live={} stage=publication_apply",
                registered_sockets.len(),
                live_sockets.len()
            ));
            return Ok(live_sockets);
        }
        ERROR_LOG.log_exit_latency(
            "[diag-exit] publication_live_sockets_registry_empty fallback=tmux_scan stage=publication_apply"
                .to_string(),
        );

        let mut all_sessions = Vec::new();
        if let Ok(managed_sockets) = self.local_tmux.discover_waitagent_sockets() {
            for socket in &managed_sockets {
                // Only include sockets that belong to this waitagent instance.
                // Each socket stores its owner's network port in a tmux global
                // option, so we filter out sockets created by other waitagent
                // processes on the same machine.
                if let Some(config) =
                    recover_network_config_for_socket(&self.local_tmux, socket.as_str())
                {
                    if config.port != network.port {
                        continue;
                    }
                }
                if let Ok(sessions) = self.local_tmux.list_sessions_on_socket(socket) {
                    all_sessions.extend(sessions);
                }
            }
        }
        Ok(live_workspace_socket_names_from_sessions(&all_sessions))
    }

    fn socket_is_live(&self, socket_name: &str) -> bool {
        self.local_tmux
            .socket_is_live(&TmuxSocketName::new(socket_name))
    }

    fn list_sessions_on_socket(
        &self,
        socket_name: &str,
    ) -> Result<Vec<ManagedSessionRecord>, LifecycleError> {
        self.local_tmux
            .list_sessions_on_socket(&TmuxSocketName::new(socket_name))
            .map_err(remote_target_publication_error)
    }

    fn find_publication_binding(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<Option<RemoteTargetPublicationBinding>, LifecycleError> {
        let bindings = self.list_publication_bindings(socket_name)?;
        Ok(bindings
            .into_iter()
            .find(|binding| binding.target_session_name == target_session_name))
    }

    fn list_publication_bindings(
        &self,
        socket_name: &str,
    ) -> Result<Vec<RemoteTargetPublicationBinding>, LifecycleError> {
        list_publication_bindings_on_socket(&self.local_tmux, &TmuxSocketName::new(socket_name))
            .map_err(remote_target_publication_error)
    }

    fn bind_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        transport_session_id: &str,
        selector: Option<&str>,
    ) -> Result<(), LifecycleError> {
        bind_publication_on_socket(
            &self.local_tmux,
            socket_name,
            target_session_name,
            authority_id,
            transport_session_id,
            selector,
        )
        .map_err(remote_target_publication_error)
    }

    fn unbind_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        unbind_publication_on_socket(&self.local_tmux, socket_name, target_session_name)
            .map_err(remote_target_publication_error)
    }

    fn live_content_pane_for_session(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<bool, LifecycleError> {
        let socket_name = TmuxSocketName::new(socket_name);
        let format = format!(
            "#{{pane_dead}}\t#{{{WAITAGENT_PANE_ROLE_OPTION}}}\t#{{{WAITAGENT_PANE_SESSION_INSTANCE_OPTION}}}"
        );
        let output = self
            .local_tmux
            .run_on_socket(
                &socket_name,
                &[
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    format,
                ],
            )
            .map_err(remote_target_publication_error)?;
        Ok(output.stdout.lines().any(|line| {
            let mut parts = line.split('\t');
            let pane_dead = parts.next().unwrap_or_default();
            let role = parts.next().unwrap_or_default();
            let owner = parts.next().unwrap_or_default();
            pane_dead == "0" && role == WAITAGENT_PANE_ROLE_CONTENT && owner == session_name
        }))
    }

    fn ensure_publication_hooks(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let socket = TmuxSocketName::new(socket_name);
        if !self.local_tmux.socket_is_live(&socket) {
            return Ok(());
        }
        let executable = current_waitagent_executable()?;
        let hook_command = publication_socket_hook_tmux_command(
            executable.to_string_lossy().as_ref(),
            socket_name,
            network,
        );
        for hook_name in PUBLICATION_GLOBAL_HOOKS {
            match self
                .local_tmux
                .set_global_hook_on_socket(socket_name, hook_name, &hook_command)
            {
                Ok(()) => {}
                Err(error)
                    if error.is_command_failure() && !self.local_tmux.socket_is_live(&socket) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(remote_target_publication_error(error)),
            }
        }
        Ok(())
    }

    fn signal_remote_target_exited(
        &self,
        socket_name: &str,
        session_name: &str,
        target: &str,
        executable: &Path,
    ) -> Result<(), LifecycleError> {
        spawn_waitagent_sidecar(
            executable,
            remote_target_exited_args(socket_name, session_name, target),
        )
        .map_err(remote_target_publication_error)
    }

    fn signal_remote_target_exited_to_workspace(
        &self,
        socket_name: &str,
        target: &str,
        executable: &Path,
    ) -> Result<usize, LifecycleError> {
        if !self.socket_is_live(socket_name) {
            return Ok(0);
        }
        let sessions = self.list_sessions_on_socket(socket_name)?;
        let mut signalled = 0;
        for session in sessions
            .into_iter()
            .filter(|session| session.is_workspace_chrome())
        {
            let t_spawn = std::time::Instant::now();
            self.signal_remote_target_exited(
                socket_name,
                session.address.session_id(),
                target,
                executable,
            )?;
            signalled += 1;
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] publication_workspace_exit_spawn socket={} session={} target={} elapsed={:?} stage=publication_apply",
                socket_name,
                session.address.session_id(),
                target,
                t_spawn.elapsed()
            ));
        }
        Ok(signalled)
    }

    fn refresh_workspace_socket(
        &self,
        socket_name: &str,
        executable: &Path,
    ) -> Result<(), LifecycleError> {
        if !self.socket_is_live(socket_name) {
            return Ok(());
        }
        spawn_socket_chrome_refresh(executable, socket_name)
    }

    fn ensure_publication_server_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError> {
        ensure_publication_server_running(socket_name, network, executable)
    }

    fn ensure_publication_agent_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError> {
        ensure_publication_agent_running(socket_name, network, executable)
    }

    fn ensure_publication_sender_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError> {
        ensure_publication_sender_process_running(executable, socket_name, network)
    }

    fn ensure_publication_owner_running(
        &self,
        socket_name: &str,
        target_session_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError> {
        ensure_publication_owner_process_running(
            executable,
            socket_name,
            target_session_name,
            network,
        )
    }
}

fn ensure_publication_server_running(
    socket_name: &str,
    network: &RemoteNetworkConfig,
    executable: &Path,
) -> Result<(), LifecycleError> {
    let socket_path = remote_target_publication_socket_path(socket_name);
    if publication_server_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    spawn_waitagent_sidecar(
        executable,
        remote_target_publication_server_args(socket_name, network),
    )
    .map_err(remote_target_publication_error)?;

    for _ in 0..PUBLICATION_SERVER_READY_RETRIES {
        if publication_server_available(&socket_path) {
            return Ok(());
        }
        thread::sleep(PUBLICATION_SERVER_READY_SLEEP);
    }

    Err(LifecycleError::Protocol(format!(
        "remote target publication server for socket `{socket_name}` did not become ready"
    )))
}

fn ensure_publication_agent_running(
    socket_name: &str,
    network: &RemoteNetworkConfig,
    executable: &Path,
) -> Result<(), LifecycleError> {
    let socket_path = remote_target_publication_agent_socket_path(socket_name);
    if publication_agent_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    spawn_waitagent_sidecar(
        executable,
        remote_target_publication_agent_args(socket_name, network),
    )
    .map_err(remote_target_publication_error)?;

    for _ in 0..PUBLICATION_SERVER_READY_RETRIES {
        if publication_agent_available(&socket_path) {
            return Ok(());
        }
        thread::sleep(PUBLICATION_SERVER_READY_SLEEP);
    }

    Err(LifecycleError::Protocol(format!(
        "remote target publication agent for socket `{socket_name}` did not become ready"
    )))
}
