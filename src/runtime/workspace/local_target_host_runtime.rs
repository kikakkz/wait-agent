use crate::cli::{
    prepend_global_network_args, LocalTargetExitedCommand, LocalTargetHostCommand,
    RemoteNetworkConfig,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::TmuxProgram;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxPaneId, TmuxSessionGateway, TmuxSessionName, TmuxSocketName,
    TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_node_ingress_server_runtime::RemoteNodeIngressServerRuntime;
use crate::runtime::remote_node_session_sync_runtime::{
    shutdown_remote_session_sync_owner, LocalCatalogChangeReason, RemoteNodeSessionSyncRuntime,
};
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::remote_workspace_socket_registry_runtime::RemoteWorkspaceSocketRegistryRuntime;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const WAITAGENT_PANE_TARGET_SESSION_OPTION: &str = "@waitagent_target_session_name";
const WAITAGENT_ACTIVE_TARGET_OPTION: &str = "@waitagent_active_target";
const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";

pub struct LocalTargetHostRuntime {
    backend: EmbeddedTmuxBackend,
    remote_target_publication_runtime: RemoteTargetPublicationRuntime,
    current_executable: PathBuf,
    network: RemoteNetworkConfig,
}

impl LocalTargetHostRuntime {
    pub fn new(
        backend: EmbeddedTmuxBackend,
        remote_target_publication_runtime: RemoteTargetPublicationRuntime,
        current_executable: PathBuf,
        network: RemoteNetworkConfig,
    ) -> Self {
        Self {
            backend,
            remote_target_publication_runtime,
            current_executable,
            network,
        }
    }

    pub fn run_host(&self, command: LocalTargetHostCommand) -> Result<(), LifecycleError> {
        let shell_program = runtime_event_shell_program(
            &self.current_executable,
            &command.socket_name,
            &command.target_session_name,
            None,
            &self.network,
        )?;
        let program = shell_program.program();
        let mut child_command = Command::new(&program.program);
        child_command
            .args(&program.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = child_command.spawn().map_err(|error| {
            LifecycleError::Io("failed to spawn local target shell".to_string(), error)
        })?;
        let status = child.wait().map_err(|error| {
            LifecycleError::Io("failed to wait for local target shell".to_string(), error)
        })?;
        ERROR_LOG.log(format!(
            "[diag-local-host] shell exited: socket={} target={} status={status}",
            command.socket_name, command.target_session_name
        ));

        let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
        let resolved_target_session_name = self
            .resolve_target_session_name(
                &command.socket_name,
                &command.target_session_name,
                &pane_id,
            )
            .unwrap_or_else(|| command.target_session_name.clone());
        ERROR_LOG.log(format!(
            "[diag-local-host] shell exit resolved session: pane={} requested_target={} resolved_target={}",
            pane_id, command.target_session_name, resolved_target_session_name
        ));
        let args = prepend_global_network_args(
            vec![
                "__local-target-exited".to_string(),
                "--socket-name".to_string(),
                command.socket_name,
                "--target-session-name".to_string(),
                resolved_target_session_name,
                "--pane-id".to_string(),
                pane_id,
            ],
            &self.network,
        );
        spawn_waitagent_sidecar(&self.current_executable, args).map_err(|error| {
            LifecycleError::Io(
                "failed to spawn local-target-exited sidecar".to_string(),
                error,
            )
        })?;
        Ok(())
    }

    pub fn run_target_exited(
        &self,
        command: LocalTargetExitedCommand,
    ) -> Result<(), LifecycleError> {
        ERROR_LOG.log(format!(
            "[diag-native] run_local_target_exited: socket={} target={} pane={}",
            command.socket_name, command.target_session_name, command.pane_id
        ));
        self.remote_target_publication_runtime
            .signal_source_session_closed(&command.socket_name, &command.target_session_name)?;

        if self.exit_is_owned_by_workspace_main_pane(&command)? {
            ERROR_LOG.log(format!(
                "[diag-native] run_local_target_exited: deferring active main pane exit to main-pane-died socket={} target={} pane={}",
                command.socket_name, command.target_session_name, command.pane_id
            ));
            return Ok(());
        }

        if self.should_stop_socket_after_target_exit(
            &command.socket_name,
            &command.target_session_name,
        )? {
            ERROR_LOG.log(format!(
                "[diag-native] run_local_target_exited: stopping socket={} after last connect target exited",
                command.socket_name
            ));
            return match self
                .backend
                .kill_server(&TmuxSocketName::new(&command.socket_name))
            {
                Ok(()) => {
                    self.unregister_live_workspace_socket(&command.socket_name);
                    self.notify_session_sync_local_target_exited(
                        &command.socket_name,
                        &command.target_session_name,
                    );
                    Ok(())
                }
                Err(error) if error.is_command_failure() => {
                    self.unregister_live_workspace_socket(&command.socket_name);
                    self.notify_session_sync_local_target_exited(
                        &command.socket_name,
                        &command.target_session_name,
                    );
                    Ok(())
                }
                Err(error) => Err(local_target_host_error(error)),
            };
        }

        let target_session_name = command.target_session_name.clone();
        match self.backend.run_socket_command(
            &TmuxSocketName::new(&command.socket_name),
            &[
                "kill-session".to_string(),
                "-t".to_string(),
                target_session_name,
            ],
        ) {
            Ok(()) => {
                self.notify_session_sync_local_target_exited(
                    &command.socket_name,
                    &command.target_session_name,
                );
                Ok(())
            }
            Err(error) if error.is_command_failure() => {
                self.notify_session_sync_local_target_exited(
                    &command.socket_name,
                    &command.target_session_name,
                );
                Ok(())
            }
            Err(error) => Err(local_target_host_error(error)),
        }
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
                "[diag-exit] local_catalog_notify_acked socket={} elapsed={:?} stage=local_target_exit",
                socket_name,
                t_notify.elapsed()
            )),
            Err(error) => ERROR_LOG.log(format!(
                "[diag-exit] local_catalog_notify_failed socket={} error={} elapsed={:?} stage=local_target_exit",
                socket_name,
                error,
                t_notify.elapsed()
            )),
        }
    }

    fn unregister_live_workspace_socket(&self, socket_name: &str) {
        let _ = shutdown_remote_session_sync_owner(
            &crate::runtime::remote_node_session_sync_runtime::remote_session_sync_owner_socket_path(
                socket_name,
            ),
        );
        if let Err(error) = RemoteWorkspaceSocketRegistryRuntime::new(self.network.clone())
            .unregister_workspace_socket(socket_name)
        {
            ERROR_LOG.log(format!(
                "[diag-exit] local_target_registry_unregister_failed socket={} error={}",
                socket_name, error
            ));
        }
        if let Err(error) = RemoteNodeIngressServerRuntime::unregister_owner_workspace_socket(
            socket_name,
            &self.network,
        ) {
            ERROR_LOG.log(format!(
                "[diag-exit] local_target_ingress_unregister_failed socket={} error={}",
                socket_name, error
            ));
        }
        let _ = RemoteNodeIngressServerRuntime::shutdown_owner(&self.network);
    }

    fn exit_is_owned_by_workspace_main_pane(
        &self,
        command: &LocalTargetExitedCommand,
    ) -> Result<bool, LifecycleError> {
        if command.pane_id.is_empty() {
            return Ok(false);
        }
        let pane = TmuxPaneId::new(command.pane_id.clone());
        let pane_session_name = match self
            .backend
            .pane_session_name_on_socket(&command.socket_name, &pane)
        {
            Ok(session_name) if !session_name.is_empty() => session_name,
            Ok(_) => return Ok(false),
            Err(error) if error.is_command_failure() => return Ok(false),
            Err(error) => return Err(local_target_host_error(error)),
        };
        let workspace = TmuxWorkspaceHandle {
            workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(
                pane_session_name.clone(),
            ),
            socket_name: TmuxSocketName::new(&command.socket_name),
            session_name: TmuxSessionName::new(pane_session_name),
        };
        let active_target = self
            .backend
            .show_session_option(&workspace, WAITAGENT_ACTIVE_TARGET_OPTION)
            .map_err(local_target_host_error)?
            .filter(|target| !target.is_empty());
        if active_target.as_deref()
            != Some(format!("{}:{}", command.socket_name, command.target_session_name).as_str())
        {
            return Ok(false);
        }
        let main_pane = self
            .backend
            .show_session_option(&workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(local_target_host_error)?
            .filter(|pane_id| !pane_id.is_empty());
        Ok(main_pane.as_deref() == Some(command.pane_id.as_str()))
    }

    fn resolve_target_session_name(
        &self,
        socket_name: &str,
        requested_target_session_name: &str,
        pane_id: &str,
    ) -> Option<String> {
        if pane_id.is_empty() {
            return Some(requested_target_session_name.to_string());
        }
        self.pane_target_session_name(socket_name, pane_id)
            .filter(|session_name| !session_name.is_empty())
            .or_else(|| {
                self.backend
                    .pane_session_name_on_socket(socket_name, &TmuxPaneId::new(pane_id))
                    .ok()
                    .filter(|session_name| !session_name.is_empty())
            })
            .or_else(|| Some(requested_target_session_name.to_string()))
    }

    fn pane_target_session_name(&self, socket_name: &str, pane_id: &str) -> Option<String> {
        let session_name = self
            .backend
            .pane_session_name_on_socket(socket_name, &TmuxPaneId::new(pane_id))
            .ok()?;
        if session_name.is_empty() {
            return None;
        }
        let workspace = TmuxWorkspaceHandle {
            workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(session_name.clone()),
            socket_name: TmuxSocketName::new(socket_name),
            session_name: TmuxSessionName::new(session_name),
        };
        let value = pane_option(
            &self.backend,
            &workspace,
            &TmuxPaneId::new(pane_id),
            WAITAGENT_PANE_TARGET_SESSION_OPTION,
        )
        .ok()
        .flatten()?;
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    fn should_stop_socket_after_target_exit(
        &self,
        socket_name: &str,
        exited_target_session_name: &str,
    ) -> Result<bool, LifecycleError> {
        let remaining_target_hosts = self
            .backend
            .list_sessions_on_socket(&TmuxSocketName::new(socket_name))
            .map_err(local_target_host_error)?
            .into_iter()
            .filter(|session| {
                session.is_target_host()
                    && session.address.session_id() != exited_target_session_name
            })
            .count();

        Ok(remaining_target_hosts == 0)
    }
}

pub(crate) struct RuntimeEventShellProgram {
    program: TmuxProgram,
    _hooks: ShellRuntimeHooks,
}

impl RuntimeEventShellProgram {
    pub(crate) fn program(&self) -> &TmuxProgram {
        &self.program
    }
}

struct ShellRuntimeHooks {
    rcfile: Option<std::path::PathBuf>,
}

impl ShellRuntimeHooks {
    fn for_shell(shell: &str, signal_command: &str) -> Result<Self, LifecycleError> {
        let shell_name = std::path::Path::new(shell)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default();
        if shell_name != "bash" {
            return Ok(Self { rcfile: None });
        }
        let path = std::env::temp_dir().join(format!(
            "waitagent-bash-runtime-hooks-{}-{}.bashrc",
            std::process::id(),
            next_hook_nonce()
        ));
        let mut file = std::fs::File::create(&path).map_err(|error| {
            LifecycleError::Io(
                "failed to create waitagent bash runtime hook".to_string(),
                error,
            )
        })?;
        file.write_all(b"if [ -r ~/.bashrc ]; then . ~/.bashrc; fi\n")
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to write waitagent bash runtime hook".to_string(),
                    error,
                )
            })?;
        writeln!(
            file,
            "__waitagent_signal_runtime() {{ if [ -n \"${{__WAITAGENT_RUNTIME_SIGNALING:-}}\" ]; then return 0; fi; local __waitagent_cmd=\"$1\"; if [ -z \"$__waitagent_cmd\" ]; then __waitagent_cmd=bash; fi; __WAITAGENT_RUNTIME_EVENT_SEQ=$(( ${{__WAITAGENT_RUNTIME_EVENT_SEQ:-0}} + 1 )); local __waitagent_seq=$__WAITAGENT_RUNTIME_EVENT_SEQ; __WAITAGENT_RUNTIME_SIGNALING=1; ({} --command-name \"$__waitagent_cmd\" --event-seq \"$__waitagent_seq\") >/dev/null 2>&1 & __WAITAGENT_RUNTIME_SIGNALING=; }}",
            signal_command
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"__waitagent_command_name() { local __waitagent_line=\"$1\"; __waitagent_line=${__waitagent_line#${__waitagent_line%%[![:space:]]*}}; local __waitagent_cmd=${__waitagent_line%%[[:space:];|&<>]*}; __waitagent_cmd=${__waitagent_cmd##*/}; printf '%s' \"$__waitagent_cmd\"; }\n",
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"__waitagent_preexec() { local __waitagent_command_line=\"$1\"; case \"$__waitagent_command_line\" in ''|__waitagent_*|*__chrome-refresh*|*__remote-session-sync-owner*|*__remote-node-ingress-server*) return 0;; esac; local __waitagent_trap; __waitagent_trap=$(trap -p DEBUG); trap - DEBUG; local __waitagent_cmd; __waitagent_cmd=$(__waitagent_command_name \"$__waitagent_command_line\"); if [ -n \"$__waitagent_cmd\" ]; then __WAITAGENT_COMMAND_RUNNING=1; __waitagent_signal_runtime \"$__waitagent_cmd\"; fi; eval \"$__waitagent_trap\"; }\n",
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"__WAITAGENT_ORIGINAL_PROMPT_COMMAND=${PROMPT_COMMAND-}\n__waitagent_prompt_command() { local __waitagent_trap; __waitagent_trap=$(trap -p DEBUG); trap - DEBUG; if [ \"${__WAITAGENT_COMMAND_RUNNING:-}\" = 1 ]; then __WAITAGENT_COMMAND_RUNNING=; __waitagent_signal_runtime bash; fi; if [ -n \"${__WAITAGENT_ORIGINAL_PROMPT_COMMAND:-}\" ]; then eval \"$__WAITAGENT_ORIGINAL_PROMPT_COMMAND\"; fi; eval \"$__waitagent_trap\"; }\n",
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"PROMPT_COMMAND=__waitagent_prompt_command\ntrap '__waitagent_preexec \"$BASH_COMMAND\"' DEBUG\n__waitagent_signal_runtime bash\n",
        )
        .map_err(|error| {
            LifecycleError::Io(
                "failed to write waitagent bash runtime hook".to_string(),
                error,
            )
        })?;
        Ok(Self { rcfile: Some(path) })
    }

    fn shell_args(&self) -> Vec<String> {
        self.rcfile.as_ref().map_or_else(Vec::new, |path| {
            vec![
                "--rcfile".to_string(),
                path.display().to_string(),
                "-i".to_string(),
            ]
        })
    }
}

impl Drop for ShellRuntimeHooks {
    fn drop(&mut self) {
        // The rcfile is consumed asynchronously by the shell started through
        // tmux. Removing it when the spawn/respawn command returns races with
        // bash opening `--rcfile`, so stale hook files are cleaned by the
        // startup/test cleanup path instead.
    }
}

fn next_hook_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn shell_command(executable: &std::path::Path, args: Vec<String>) -> String {
    let mut parts = vec![shell_escape(&executable.display().to_string())];
    parts.extend(args.iter().map(|arg| shell_escape(arg)));
    parts.join(" ")
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn runtime_event_shell_program(
    executable: &std::path::Path,
    socket_name: &str,
    target_session_name: &str,
    workspace_dir: Option<&std::path::Path>,
    network: &RemoteNetworkConfig,
) -> Result<RuntimeEventShellProgram, LifecycleError> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let signal_command = shell_command(
        executable,
        prepend_global_network_args(
            vec![
                "__chrome-refresh-socket-signal".to_string(),
                "--socket-name".to_string(),
                socket_name.to_string(),
                "--target-session-name".to_string(),
                target_session_name.to_string(),
            ],
            network,
        ),
    );
    let shell_hooks = ShellRuntimeHooks::for_shell(&shell, &signal_command)?;
    let mut program = TmuxProgram::new(shell).with_args(shell_hooks.shell_args());
    if let Some(workspace_dir) = workspace_dir {
        program = program.with_start_directory(workspace_dir);
    }
    Ok(RuntimeEventShellProgram {
        program,
        _hooks: shell_hooks,
    })
}

fn local_target_host_error(error: crate::infra::tmux::TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux local-target-host command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{shell_command, LocalTargetHostRuntime, ShellRuntimeHooks};
    use crate::application::workspace_service::WorkspaceService;
    use crate::cli::{LocalTargetExitedCommand, RemoteNetworkConfig};
    use crate::domain::workspace::WorkspaceInstanceConfig;
    use crate::infra::tmux::{
        EmbeddedTmuxBackend, TmuxGateway, TmuxSessionGateway, TmuxSocketName,
    };
    use crate::runtime::current_executable::waitagent_test_executable;
    use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
    use crate::runtime::workspace_runtime::WorkspaceRuntime;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn bash_runtime_hooks_use_rcfile_and_socket_signal_command() {
        let command = shell_command(
            std::path::Path::new("/tmp/wait agent"),
            vec![
                "__chrome-refresh-socket-signal".to_string(),
                "--socket-name".to_string(),
                "wa-1".to_string(),
                "--target-session-name".to_string(),
                "target-1".to_string(),
            ],
        );
        let hooks = ShellRuntimeHooks::for_shell("/bin/bash", &command)
            .expect("bash runtime hooks should be created");
        let args = hooks.shell_args();
        assert_eq!(args.first().map(String::as_str), Some("--rcfile"));
        assert_eq!(args.last().map(String::as_str), Some("-i"));
        let rcfile = args.get(1).expect("rcfile path should exist");
        let content = std::fs::read_to_string(rcfile).expect("rcfile should be readable");
        assert!(content.contains("__chrome-refresh-socket-signal"));
        assert!(content.contains("--target-session-name"));
        assert!(content.contains("--command-name"));
        assert!(content.contains("__waitagent_preexec"));
        assert!(content.contains("__waitagent_prompt_command"));
        assert!(content.contains("trap '__waitagent_preexec \"$BASH_COMMAND\"' DEBUG"));
        assert!(content.contains("PROMPT_COMMAND"));
    }

    #[test]
    fn connect_workspace_stops_server_when_last_target_host_exits() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("connect-last-target-exit");
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

        let runtime = LocalTargetHostRuntime::new(
            backend.clone(),
            RemoteTargetPublicationRuntime::from_build_env_with_network(RemoteNetworkConfig {
                port: 7474,
                connect: Some("10.1.29.130:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            })
            .expect("publication runtime should build"),
            waitagent_test_executable(),
            RemoteNetworkConfig {
                port: 7474,
                connect: Some("10.1.29.130:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            },
        );

        runtime
            .run_target_exited(LocalTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                target_session_name: target_host.session_name.as_str().to_string(),
                pane_id: "%1".to_string(),
            })
            .expect("last connect target exit should stop workspace socket");

        assert!(
            !backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str()
            )),
            "workspace socket should be stopped after the last connect target exits"
        );

        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn local_workspace_stops_server_when_last_detached_target_host_exits() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("local-last-detached-target-exit");
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

        let runtime = LocalTargetHostRuntime::new(
            backend.clone(),
            RemoteTargetPublicationRuntime::from_build_env_with_network(
                RemoteNetworkConfig::default(),
            )
            .expect("publication runtime should build"),
            waitagent_test_executable(),
            RemoteNetworkConfig::default(),
        );

        runtime
            .run_target_exited(LocalTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                target_session_name: target_host.session_name.as_str().to_string(),
                pane_id: "%1".to_string(),
            })
            .expect("last detached local target exit should stop workspace socket");

        assert!(
            !backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str()
            )),
            "workspace socket should be stopped after the last detached local target exits"
        );

        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn active_main_pane_local_exit_defers_socket_shutdown_to_main_pane_died() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("local-active-main-pane-exit");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime::new(
            crate::runtime::workspace_runtime::WorkspaceRuntime::new(WorkspaceService::new(
                backend.clone(),
            )),
            crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
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

        let main_slot_runtime = crate::runtime::main_slot_runtime::MainSlotRuntime::new(
            backend.clone(),
            crate::runtime::target_host_runtime::TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
            crate::application::target_registry_service::TargetRegistryService::new(
                crate::application::target_registry_service::DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );

        let target_name = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        main_slot_runtime
            .run_activate_target(crate::cli::ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: target_name,
            })
            .expect("local target activation should succeed");

        let main_pane = backend
            .show_session_option(&workspace.workspace_handle, "@waitagent_main_pane_id")
            .expect("main pane option should read")
            .expect("main pane should be populated");
        let runtime = LocalTargetHostRuntime::new(
            backend.clone(),
            RemoteTargetPublicationRuntime::from_build_env_with_network(
                RemoteNetworkConfig::default(),
            )
            .expect("publication runtime should build"),
            waitagent_executable,
            RemoteNetworkConfig::default(),
        );

        runtime
            .run_target_exited(LocalTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                target_session_name: target_host.session_name.as_str().to_string(),
                pane_id: main_pane,
            })
            .expect("local active main pane exit sidecar should defer shutdown");

        assert!(
            backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str()
            )),
            "local-target-exited must not close the socket for an active workspace main pane"
        );

        let _ = backend.kill_server(&TmuxSocketName::new(
            workspace.workspace_handle.socket_name.as_str(),
        ));
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn local_exit_uses_pane_target_identity_instead_of_workspace_session() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("local-exit-pane-identity");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let waitagent_executable = waitagent_test_executable();

        let entry_runtime = crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime::new(
            crate::runtime::workspace_runtime::WorkspaceRuntime::new(WorkspaceService::new(
                backend.clone(),
            )),
            crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");

        let first_target = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("first target host bootstrap should succeed");
        let second_target = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("second target host bootstrap should succeed");
        let _third_target = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("third target host bootstrap should succeed");

        let main_slot_runtime = crate::runtime::main_slot_runtime::MainSlotRuntime::new(
            backend.clone(),
            crate::runtime::target_host_runtime::TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
            crate::application::target_registry_service::TargetRegistryService::new(
                crate::application::target_registry_service::DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );

        let first_target_name = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            first_target.session_name.as_str()
        );
        main_slot_runtime
            .run_activate_target(crate::cli::ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: first_target_name,
            })
            .expect("local target activation should succeed");

        let main_pane = backend
            .show_session_option(&workspace.workspace_handle, "@waitagent_main_pane_id")
            .expect("main pane option should read")
            .expect("main pane should be populated");

        let workspace_session_name = backend
            .pane_session_name_on_socket(
                workspace.workspace_handle.socket_name.as_str(),
                &crate::infra::tmux::TmuxPaneId::new(main_pane.clone()),
            )
            .expect("pane session should resolve");
        assert_eq!(
            workspace_session_name,
            workspace.workspace_handle.session_name.as_str(),
            "activated local target should now live in the workspace session pane"
        );

        let runtime = LocalTargetHostRuntime::new(
            backend.clone(),
            RemoteTargetPublicationRuntime::from_build_env_with_network(
                RemoteNetworkConfig::default(),
            )
            .expect("publication runtime should build"),
            waitagent_executable,
            RemoteNetworkConfig::default(),
        );

        runtime
            .run_target_exited(LocalTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                target_session_name: first_target.session_name.as_str().to_string(),
                pane_id: main_pane.clone(),
            })
            .expect("local target exit sidecar should defer active main pane cleanup");

        assert!(
            backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str()
            )),
            "workspace socket should remain live after the local exit sidecar defers"
        );
        let sessions_after_sidecar = backend
            .list_sessions_on_socket(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str(),
            ))
            .expect("sessions should list after sidecar defer");
        assert!(
            sessions_after_sidecar
                .iter()
                .any(|session| session.address.session_id() == first_target.session_name.as_str()),
            "active main pane target session is removed by main-pane-died, not local-target-exited"
        );

        main_slot_runtime
            .run_main_pane_died(crate::cli::MainPaneDiedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                pane_id: main_pane,
                pane_generation: None,
            })
            .expect("main-pane-died should clean up active exited target and recover");

        assert!(
            backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str()
            )),
            "workspace socket should remain live after one local target exits"
        );
        let sessions = backend
            .list_sessions_on_socket(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str(),
            ))
            .expect("sessions should list");
        assert!(
            sessions.iter().any(|session| session.address.session_id()
                == workspace.workspace_handle.session_name.as_str()),
            "workspace session should still exist"
        );
        assert!(
            sessions
                .iter()
                .any(|session| session.address.session_id() == second_target.session_name.as_str()),
            "other local target sessions should remain"
        );
        assert!(
            !sessions
                .iter()
                .any(|session| session.address.session_id() == first_target.session_name.as_str()),
            "exited target session should be gone after main-pane-died recovery"
        );

        let _ = backend.kill_server(&TmuxSocketName::new(
            workspace.workspace_handle.socket_name.as_str(),
        ));
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn connect_workspace_keeps_server_running_when_other_target_hosts_remain() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("connect-other-target-remains");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let workspace = WorkspaceRuntime::new(WorkspaceService::new(backend.clone()))
            .ensure_workspace_for_config(workspace_config.clone())
            .expect("workspace bootstrap should succeed");
        let first_target = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("first target host bootstrap should succeed");
        let second_target = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("second target host bootstrap should succeed");

        let runtime = LocalTargetHostRuntime::new(
            backend.clone(),
            RemoteTargetPublicationRuntime::from_build_env_with_network(RemoteNetworkConfig {
                port: 7474,
                connect: Some("10.1.29.130:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            })
            .expect("publication runtime should build"),
            waitagent_test_executable(),
            RemoteNetworkConfig {
                port: 7474,
                connect: Some("10.1.29.130:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            },
        );

        runtime
            .run_target_exited(LocalTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                target_session_name: first_target.session_name.as_str().to_string(),
                pane_id: "%1".to_string(),
            })
            .expect("target exit with remaining peers should succeed");

        assert!(
            backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str()
            )),
            "workspace socket should remain live while another target host exists"
        );
        let remaining_sessions = backend
            .list_sessions_on_socket(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str(),
            ))
            .expect("sessions should list after partial target exit");
        assert!(
            remaining_sessions
                .iter()
                .any(|session| session.address.session_id() == second_target.session_name.as_str()),
            "another target host should still remain on the socket"
        );

        let _ = backend.kill_server(&TmuxSocketName::new(
            workspace.workspace_handle.socket_name.as_str(),
        ));
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

fn pane_option(
    backend: &EmbeddedTmuxBackend,
    workspace: &TmuxWorkspaceHandle,
    pane: &TmuxPaneId,
    option_name: &str,
) -> Result<Option<String>, crate::infra::tmux::TmuxError> {
    let output = backend.run_on_socket(
        &workspace.socket_name,
        &[
            "show-options".to_string(),
            "-pqv".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            option_name.to_string(),
        ],
    )?;
    let value = output.stdout.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.to_string()))
    }
}
