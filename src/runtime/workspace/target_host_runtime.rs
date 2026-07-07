use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::application::workspace_service::{BootstrappedWorkspace, WorkspaceService};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxLayoutGateway, TmuxSocketName,
    WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION, WAITAGENT_PANE_ROLE_CONTENT, WAITAGENT_PANE_ROLE_OPTION,
    WAITAGENT_PANE_SESSION_INSTANCE_OPTION, WAITAGENT_PANE_TARGET_ID_OPTION,
    WAITAGENT_PANE_TARGET_SESSION_OPTION,
};
use crate::lifecycle::LifecycleError;
#[cfg(test)]
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::local_target_host_runtime::runtime_event_shell_program;
use crate::runtime::remote_node_session_sync_runtime::{
    LocalCatalogChangeReason, RemoteNodeSessionSyncRuntime,
};
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::workspace_runtime::WorkspaceRuntime;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";

pub struct TargetHostRuntime {
    workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
    backend: EmbeddedTmuxBackend,
    remote_target_publication_runtime: RemoteTargetPublicationRuntime,
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    current_executable: PathBuf,
    network: crate::cli::RemoteNetworkConfig,
}

impl TargetHostRuntime {
    #[cfg(test)]
    pub fn from_build_env(backend: EmbeddedTmuxBackend) -> Result<Self, LifecycleError> {
        let current_executable = current_waitagent_executable()?;
        Self::from_build_env_with_network_and_executable(
            backend,
            crate::cli::RemoteNetworkConfig::default(),
            current_executable,
        )
    }

    pub fn new(
        workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
        backend: EmbeddedTmuxBackend,
        remote_target_publication_runtime: RemoteTargetPublicationRuntime,
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        current_executable: PathBuf,
        network: crate::cli::RemoteNetworkConfig,
    ) -> Self {
        Self {
            workspace_runtime,
            backend,
            remote_target_publication_runtime,
            target_registry,
            current_executable,
            network,
        }
    }

    pub fn from_build_env_with_network_and_executable(
        backend: EmbeddedTmuxBackend,
        network: crate::cli::RemoteNetworkConfig,
        current_executable: PathBuf,
    ) -> Result<Self, LifecycleError> {
        Ok(Self::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            backend,
            RemoteTargetPublicationRuntime::from_build_env_with_network(network.clone())?,
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_network(network.clone())
                    .map_err(target_host_error)?,
            ),
            current_executable,
            network,
        ))
    }

    pub fn ensure_target_host(
        &self,
        mut config: WorkspaceInstanceConfig,
    ) -> Result<BootstrappedWorkspace, TmuxError> {
        let t_total = Instant::now();
        let shell_program = runtime_event_shell_program(
            &self.current_executable,
            &config.socket_name,
            &config.session_name,
            Some(&config.workspace_dir),
            &self.network,
        )
        .map_err(|error| {
            TmuxError::new(format!(
                "failed to prepare target runtime shell hooks: {error}"
            ))
        })?;
        config = config.with_initial_program(
            shell_program.program().program.clone(),
            shell_program.program().args.clone(),
            shell_program.program().environment.clone(),
        );
        let workspace = self.workspace_runtime.ensure_workspace_for_config(config)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] target_host ensure_workspace socket={} session={} elapsed={:?}",
            workspace.workspace_handle.socket_name.as_str(),
            workspace.workspace_handle.session_name.as_str(),
            t_total.elapsed()
        ));
        let t_pane = Instant::now();
        let pane = self.backend.target_main_pane_on_socket(
            workspace.workspace_handle.socket_name.as_str(),
            workspace.workspace_handle.session_name.as_str(),
        )?;
        ERROR_LOG.log(format!(
            "[diag-newhost] target_host target_main_pane socket={} session={} pane={} elapsed={:?} total={:?}",
            workspace.workspace_handle.socket_name.as_str(),
            workspace.workspace_handle.session_name.as_str(),
            pane.as_str(),
            t_pane.elapsed(),
            t_total.elapsed()
        ));
        let t_respawn = Instant::now();
        self.backend
            .respawn_pane(&workspace.workspace_handle, &pane, shell_program.program())?;
        ERROR_LOG.log(format!(
            "[diag-newhost] target_host respawn_pane socket={} session={} pane={} elapsed={:?} total={:?}",
            workspace.workspace_handle.socket_name.as_str(),
            workspace.workspace_handle.session_name.as_str(),
            pane.as_str(),
            t_respawn.elapsed(),
            t_total.elapsed()
        ));
        let t_metadata = Instant::now();
        let qualified_target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            workspace.workspace_handle.session_name.as_str()
        );
        self.backend.set_pane_option(
            &workspace.workspace_handle,
            &pane,
            WAITAGENT_PANE_ROLE_OPTION,
            WAITAGENT_PANE_ROLE_CONTENT,
        )?;
        self.backend.set_pane_option(
            &workspace.workspace_handle,
            &pane,
            WAITAGENT_PANE_SESSION_INSTANCE_OPTION,
            workspace.workspace_handle.session_name.as_str(),
        )?;
        self.backend.set_pane_option(
            &workspace.workspace_handle,
            &pane,
            WAITAGENT_PANE_TARGET_SESSION_OPTION,
            workspace.workspace_handle.session_name.as_str(),
        )?;
        self.backend.set_pane_option(
            &workspace.workspace_handle,
            &pane,
            WAITAGENT_PANE_TARGET_ID_OPTION,
            &qualified_target,
        )?;
        self.backend.set_session_option(
            &workspace.workspace_handle,
            WAITAGENT_MAIN_PANE_OPTION,
            pane.as_str(),
        )?;
        self.backend.set_session_option(
            &workspace.workspace_handle,
            WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION,
            shell_program.agent_signal_token(),
        )?;
        ERROR_LOG.log(format!(
            "[diag-newhost] target_host write_metadata socket={} session={} pane={} elapsed={:?} total={:?}",
            workspace.workspace_handle.socket_name.as_str(),
            workspace.workspace_handle.session_name.as_str(),
            pane.as_str(),
            t_metadata.elapsed(),
            t_total.elapsed()
        ));
        Ok(workspace)
    }

    pub fn refresh_published_target_session(
        &self,
        session: Option<&ManagedSessionRecord>,
    ) -> Result<(), LifecycleError> {
        let Some(session) = session.filter(|session| session.is_target_host()) else {
            return Ok(());
        };
        self.remote_target_publication_runtime
            .signal_source_session_refresh(
                session.address.server_id(),
                session.address.session_id(),
            )
    }

    pub fn close_target_session_identity(
        &self,
        target: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let Some(target) = target else {
            return Ok(());
        };
        if let Some(session) = self
            .target_registry
            .find_target(target)
            .map_err(target_host_error)?
        {
            return self.close_resolved_target_session(&session);
        }
        if let Some((socket_name, session_name)) = split_qualified_target(target) {
            self.remote_target_publication_runtime
                .signal_source_session_closed(socket_name, session_name)?;
            return match self.backend.run_socket_command(
                &TmuxSocketName::new(socket_name),
                &[
                    "kill-session".to_string(),
                    "-t".to_string(),
                    session_name.to_string(),
                ],
            ) {
                Ok(()) => {
                    self.notify_session_sync_local_target_exited(socket_name, session_name);
                    Ok(())
                }
                Err(error) if error.is_command_failure() => {
                    self.notify_session_sync_local_target_exited(socket_name, session_name);
                    Ok(())
                }
                Err(error) => Err(target_host_error(error)),
            };
        }
        Ok(())
    }

    fn notify_session_sync_local_target_exited(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) {
        let t_notify = std::time::Instant::now();
        match RemoteNodeSessionSyncRuntime::notify_local_catalog_changed(
            socket_name,
            &self.network,
            LocalCatalogChangeReason::LocalTargetExited {
                target_session_name: target_session_name.to_string(),
            },
        ) {
            Ok(()) => ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] local_catalog_notify_acked socket={} elapsed={:?} stage=target_host_close",
                socket_name,
                t_notify.elapsed()
            )),
            Err(error) => ERROR_LOG.log(format!(
                "[diag-exit] local_catalog_notify_failed socket={} error={} elapsed={:?} stage=target_host_close",
                socket_name,
                error,
                t_notify.elapsed()
            )),
        }
    }

    fn close_resolved_target_session(
        &self,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        if !session.is_target_host() {
            return Ok(());
        }
        if session.address.transport() == &SessionTransport::RemotePeer {
            self.remote_target_publication_runtime
                .signal_source_session_closed(
                    session.address.server_id(),
                    session.address.session_id(),
                )?;
            return Ok(());
        }
        self.remote_target_publication_runtime
            .signal_source_session_closed(
                session.address.server_id(),
                session.address.session_id(),
            )?;
        match self.backend.run_socket_command(
            &TmuxSocketName::new(session.address.server_id()),
            &[
                "kill-session".to_string(),
                "-t".to_string(),
                session.address.session_id().to_string(),
            ],
        ) {
            Ok(()) => {
                self.notify_session_sync_local_target_exited(
                    session.address.server_id(),
                    session.address.session_id(),
                );
                Ok(())
            }
            Err(error) if error.is_command_failure() => {
                self.notify_session_sync_local_target_exited(
                    session.address.server_id(),
                    session.address.session_id(),
                );
                Ok(())
            }
            Err(error) => Err(target_host_error(error)),
        }
    }
}

fn split_qualified_target(target: &str) -> Option<(&str, &str)> {
    let (socket_name, session_name) = target.rsplit_once(':')?;
    if socket_name.is_empty() || session_name.is_empty() {
        return None;
    }
    Some((socket_name, session_name))
}

fn target_host_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native target-host command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

#[cfg(test)]
mod target_host_runtime_test {
    use super::TargetHostRuntime;
    use crate::application::workspace_service::WorkspaceService;
    use crate::domain::workspace::WorkspaceInstanceConfig;
    use crate::infra::tmux::{
        EmbeddedTmuxBackend, TmuxGateway, TmuxSessionGateway, TmuxSocketName,
    };
    use crate::runtime::current_executable::waitagent_test_executable;
    use crate::runtime::remote_node_session_sync_runtime::remote_session_sync_owner_socket_path;
    use crate::runtime::workspace_runtime::WorkspaceRuntime;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn close_local_target_session_notifies_session_sync_owner() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("target-close-notify");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let workspace = WorkspaceRuntime::new(WorkspaceService::new(backend.clone()))
            .ensure_workspace_for_config(workspace_config.clone())
            .expect("workspace bootstrap should succeed");
        let target_host = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("target host bootstrap should succeed");
        let socket_name = workspace.workspace_handle.socket_name.as_str().to_string();
        let target_session_name = target_host.session_name.as_str().to_string();
        let socket_path = remote_session_sync_owner_socket_path(&socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).expect("fake sync owner should bind");
        let (request_tx, request_rx) = mpsc::channel();
        let owner_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("notify client should connect");
            let mut request = String::new();
            stream
                .read_to_string(&mut request)
                .expect("notify request should read");
            request_tx
                .send(request)
                .expect("request should be recorded");
            stream.write_all(b"ok\n").expect("ack should write");
        });

        let runtime = TargetHostRuntime::from_build_env_with_network_and_executable(
            backend.clone(),
            crate::cli::RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            },
            waitagent_test_executable(),
        )
        .expect("target host runtime should build");
        runtime
            .close_target_session_identity(Some(&format!("{socket_name}:{target_session_name}")))
            .expect("target close should succeed");

        let request = request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("sync owner should receive local catalog change notify");
        assert!(
            request.starts_with("local-catalog-changed local-target-exited\t"),
            "unexpected owner request: {request:?}"
        );
        assert!(
            request.contains(&target_session_name),
            "notify should include closed target session name"
        );
        owner_thread.join().expect("fake owner should join");
        assert!(
            !backend
                .list_sessions_on_socket(&TmuxSocketName::new(&socket_name))
                .expect("sessions should list")
                .iter()
                .any(|session| session.address.session_id() == target_session_name),
            "closed target session should be removed before notify returns"
        );

        let _ = backend.kill_server(&TmuxSocketName::new(&socket_name));
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    fn unique_workspace_config(prefix: &str) -> WorkspaceInstanceConfig {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let workspace_dir = std::env::temp_dir().join(format!("waitagent-{prefix}-{nonce:x}"));
        fs::create_dir_all(&workspace_dir)
            .expect("temporary workspace directory should be created");
        WorkspaceInstanceConfig {
            workspace_dir,
            workspace_key: format!("{prefix}-{nonce:x}"),
            socket_name: format!("wa-test-{nonce:x}"),
            session_name: format!("waitagent-test-{prefix}-{nonce:x}"),
            session_role: crate::domain::workspace::WorkspaceSessionRole::WorkspaceChrome,
            initial_rows: None,
            initial_cols: None,
            initial_program: None,
        }
    }
}
