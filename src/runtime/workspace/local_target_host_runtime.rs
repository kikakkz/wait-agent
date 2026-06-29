use crate::cli::{
    prepend_global_network_args, LocalTargetExitedCommand, LocalTargetHostCommand,
    RemoteNetworkConfig,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::TmuxProgram;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxLayoutGateway, TmuxPaneId, TmuxSessionGateway, TmuxSessionName,
    TmuxSocketName, TmuxWorkspaceHandle, WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::agent_signal_runtime::{agent_signal_socket_path, generate_agent_signal_token};
use crate::runtime::remote_node_ingress_server_runtime::RemoteNodeIngressServerRuntime;
use crate::runtime::remote_node_session_sync_runtime::{
    shutdown_remote_session_sync_owner, LocalCatalogChangeReason, RemoteNodeSessionSyncRuntime,
};
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
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
        let workspace = TmuxWorkspaceHandle {
            workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(
                command.target_session_name.clone(),
            ),
            socket_name: TmuxSocketName::new(&command.socket_name),
            session_name: TmuxSessionName::new(command.target_session_name.clone()),
        };
        self.backend
            .set_session_option(
                &workspace,
                WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION,
                shell_program.agent_signal_token(),
            )
            .map_err(local_target_host_error)?;
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
        if let Err(error) = RemoteRuntimeOwnerRuntime::shutdown_owner_if_unused(&self.network) {
            ERROR_LOG.log(format!(
                "[diag-exit] local_target_remote_runtime_owner_shutdown_failed socket={} error={}",
                socket_name, error
            ));
        }
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
    agent_signal_token: String,
    _hooks: ShellRuntimeHooks,
}

impl RuntimeEventShellProgram {
    pub(crate) fn program(&self) -> &TmuxProgram {
        &self.program
    }

    pub(crate) fn agent_signal_token(&self) -> &str {
        &self.agent_signal_token
    }
}

struct ShellRuntimeHooks {
    rcfile: Option<std::path::PathBuf>,
}

struct AgentSignalShellEnv {
    signal_socket: String,
    socket_name: String,
    target_session_name: String,
    token: String,
}

impl ShellRuntimeHooks {
    fn for_shell(
        shell: &str,
        signal_command: &str,
        agent_signal_env: &AgentSignalShellEnv,
    ) -> Result<Self, LifecycleError> {
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
            "export WAITAGENT_SIGNAL_SOCKET={}\nexport WAITAGENT_SOCKET_NAME={}\nexport WAITAGENT_TARGET_SESSION_NAME={}\nexport WAITAGENT_AGENT_SIGNAL_TOKEN={}\nexport WAITAGENT_PANE_ID=\"${{TMUX_PANE:-}}\"",
            shell_escape(agent_signal_env.signal_socket.as_str()),
            shell_escape(agent_signal_env.socket_name.as_str()),
            shell_escape(agent_signal_env.target_session_name.as_str()),
            shell_escape(agent_signal_env.token.as_str())
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"__waitagent_agent_exec() { local __waitagent_agent_name=\"$1\"; shift; WAITAGENT_AGENT_NAME=\"$__waitagent_agent_name\" command \"$__waitagent_agent_name\" \"$@\"; }\n",
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"codex() { __waitagent_agent_exec codex \"$@\"; }\nclaude() { __waitagent_agent_exec claude \"$@\"; }\nkimi() { __waitagent_agent_exec kimi \"$@\"; }\n",
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        writeln!(
            file,
            "__waitagent_signal_runtime() {{ if [ -n \"${{__WAITAGENT_RUNTIME_SIGNALING:-}}\" ]; then return 0; fi; local __waitagent_mode=\"${{1:-prompt}}\"; __WAITAGENT_RUNTIME_EVENT_SEQ=$(( ${{__WAITAGENT_RUNTIME_EVENT_SEQ:-0}} + 1 )); local __waitagent_seq=$__WAITAGENT_RUNTIME_EVENT_SEQ; __WAITAGENT_RUNTIME_SIGNALING=1; if [ \"$__waitagent_mode\" = running ]; then ({} --running --event-seq \"$__waitagent_seq\") >/dev/null 2>&1 & disown; else ({} --event-seq \"$__waitagent_seq\") >/dev/null 2>&1 & disown; fi; __WAITAGENT_RUNTIME_SIGNALING=; }}",
            signal_command,
            signal_command
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"__waitagent_preexec() { local __waitagent_command_line=\"$1\"; case \"$__waitagent_command_line\" in ''|__waitagent_*|*__chrome-refresh*|*__remote-session-sync-owner*|*__remote-node-ingress-server*) return 0;; esac; local __waitagent_trap; __waitagent_trap=$(trap -p DEBUG); trap - DEBUG; __WAITAGENT_COMMAND_RUNNING=1; __waitagent_signal_runtime running; eval \"$__waitagent_trap\"; }\n",
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"__WAITAGENT_ORIGINAL_PROMPT_COMMAND=${PROMPT_COMMAND-}\n__waitagent_prompt_command() { local __waitagent_trap; __waitagent_trap=$(trap -p DEBUG); trap - DEBUG; if [ \"${__WAITAGENT_COMMAND_RUNNING:-}\" = 1 ]; then __WAITAGENT_COMMAND_RUNNING=; __waitagent_signal_runtime prompt; fi; if [ -n \"${__WAITAGENT_ORIGINAL_PROMPT_COMMAND:-}\" ]; then eval \"$__WAITAGENT_ORIGINAL_PROMPT_COMMAND\"; fi; eval \"$__waitagent_trap\"; }\n",
        )
        .map_err(|error| {
            LifecycleError::Io("failed to write waitagent bash runtime hook".to_string(), error)
        })?;
        file.write_all(
            b"PROMPT_COMMAND=__waitagent_prompt_command\ntrap '__waitagent_preexec \"$BASH_COMMAND\"' DEBUG\n__waitagent_signal_runtime prompt\n",
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

pub(crate) fn main_pane_output_event_bridge_command(
    executable: &std::path::Path,
    socket_name: &str,
    target_session_name: Option<&str>,
    network: &RemoteNetworkConfig,
) -> String {
    let mut args = vec![
        "__main-pane-output-event-bridge".to_string(),
        "--socket-name".to_string(),
        socket_name.to_string(),
    ];
    if let Some(target_session_name) = target_session_name {
        args.push("--target-session-name".to_string());
        args.push(target_session_name.to_string());
    }
    shell_command(executable, prepend_global_network_args(args, network))
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
    let agent_signal_env = AgentSignalShellEnv {
        signal_socket: agent_signal_socket_path(socket_name)
            .to_string_lossy()
            .into_owned(),
        socket_name: socket_name.to_string(),
        target_session_name: target_session_name.to_string(),
        token: generate_agent_signal_token(),
    };
    let shell_hooks = ShellRuntimeHooks::for_shell(&shell, &signal_command, &agent_signal_env)?;
    let mut program = TmuxProgram::new(shell)
        .with_args(shell_hooks.shell_args())
        .with_environment([
            (
                "WAITAGENT_SIGNAL_SOCKET".to_string(),
                agent_signal_env.signal_socket.clone(),
            ),
            (
                "WAITAGENT_SOCKET_NAME".to_string(),
                agent_signal_env.socket_name.clone(),
            ),
            (
                "WAITAGENT_TARGET_SESSION_NAME".to_string(),
                agent_signal_env.target_session_name.clone(),
            ),
            (
                "WAITAGENT_AGENT_SIGNAL_TOKEN".to_string(),
                agent_signal_env.token.clone(),
            ),
            ("WAITAGENT_AGENT_NAME".to_string(), String::new()),
        ]);
    if let Some(workspace_dir) = workspace_dir {
        program = program.with_start_directory(workspace_dir);
    }
    Ok(RuntimeEventShellProgram {
        program,
        agent_signal_token: agent_signal_env.token,
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
    use super::{shell_command, AgentSignalShellEnv, LocalTargetHostRuntime, ShellRuntimeHooks};
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
        let env = AgentSignalShellEnv {
            signal_socket: "/tmp/waitagent-agent-signal-wa-1.sock".to_string(),
            socket_name: "wa-1".to_string(),
            target_session_name: "target-1".to_string(),
            token: "secret".to_string(),
        };
        let hooks = ShellRuntimeHooks::for_shell("/bin/bash", &command, &env)
            .expect("bash runtime hooks should be created");
        let args = hooks.shell_args();
        assert_eq!(args.first().map(String::as_str), Some("--rcfile"));
        assert_eq!(args.last().map(String::as_str), Some("-i"));
        let rcfile = args.get(1).expect("rcfile path should exist");
        let content = std::fs::read_to_string(rcfile).expect("rcfile should be readable");
        assert!(content.contains("__chrome-refresh-socket-signal"));
        assert!(content.contains("--target-session-name"));
        assert!(content.contains("--running"));
        assert!(!content.contains("--command-name"));
        assert!(!content.contains("__waitagent_command_name"));
        assert!(content.contains("& disown"));
        assert!(content.contains("__waitagent_preexec"));
        assert!(content.contains("__waitagent_prompt_command"));
        assert!(content.contains("WAITAGENT_SIGNAL_SOCKET"));
        assert!(content.contains("WAITAGENT_PANE_ID=\"${TMUX_PANE:-}\""));
        assert!(content.contains("kimi() { __waitagent_agent_exec kimi"));
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
                target: first_target_name.clone(),
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
        assert_eq!(
            backend
                .show_session_option(&workspace.workspace_handle, "@waitagent_active_target")
                .expect("active target should read after sidecar defer")
                .as_deref(),
            Some(first_target_name.as_str()),
            "local-target-exited should defer active main pane ownership to main-pane-died"
        );
        assert_eq!(
            backend
                .show_session_option(&workspace.workspace_handle, "@waitagent_main_pane_id")
                .expect("main pane option should read after sidecar defer")
                .as_deref(),
            Some(main_pane.as_str()),
            "local-target-exited should not clear the active main pane"
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
        let second_target_name = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            second_target.session_name.as_str()
        );
        assert_eq!(
            backend
                .show_session_option(&workspace.workspace_handle, "@waitagent_active_target")
                .expect("active target should read after main-pane-died recovery")
                .as_deref(),
            Some(second_target_name.as_str()),
            "main-pane-died should recover to another local target"
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
