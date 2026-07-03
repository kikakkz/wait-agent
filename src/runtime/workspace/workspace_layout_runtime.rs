use crate::application::control_service::{ControlService, FooterMenuBindings};
use crate::application::layout_service::{
    LayoutFocusBehavior, LayoutService, FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE,
};
use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{
    prepend_global_network_args, CloseSessionCommand, LayoutReconcileCommand, RemoteNetworkConfig,
    UiPaneCommand,
};
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxLayoutGateway, TmuxPaneId, TmuxProgram, TmuxSessionName,
    TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::local_target_host_runtime::main_pane_output_event_bridge_command;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
type LayoutTopologyAfterOptionsHook = dyn Fn(&TmuxWorkspaceHandle) + Send + Sync + 'static;

#[cfg(test)]
static LAYOUT_TOPOLOGY_AFTER_OPTIONS_HOOK: Mutex<Option<Arc<LayoutTopologyAfterOptionsHook>>> =
    Mutex::new(None);

const STARTUP_CHROME_READY_TIMEOUT: Duration = Duration::from_millis(300);
const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";
const WAITAGENT_MAIN_PANE_PIPE_OPTION: &str = "@waitagent_main_pane_pipe_id";
const WORKSPACE_MAIN_BRIDGE_PIPE_OWNER: &str = "workspace-main-bridge";
const WAITAGENT_MAIN_PANE_OUTPUT_BRIDGE_OPTION: &str = "@waitagent_main_pane_output_bridge";
const WAITAGENT_MAIN_PANE_GENERATION_OPTION: &str = "@waitagent_main_pane_generation";
const WAITAGENT_MAIN_PANE_TRANSITION_OPTION: &str = "@waitagent_main_pane_transition";
const MAIN_PANE_OUTPUT_BRIDGE_DISABLED: &str = "disabled";
#[allow(dead_code)]
const MAIN_PANE_OUTPUT_BRIDGE_ENABLED: &str = "enabled";

#[derive(Clone, Copy)]
enum InitialChromePane {
    Sidebar,
    Footer,
}

#[cfg(test)]
pub(crate) fn set_layout_topology_after_options_hook_for_tests(
    hook: Option<Arc<LayoutTopologyAfterOptionsHook>>,
) {
    *LAYOUT_TOPOLOGY_AFTER_OPTIONS_HOOK
        .lock()
        .expect("layout topology test hook should lock") = hook;
}

#[cfg(test)]
fn layout_topology_after_options_hook_for_tests() -> Option<Arc<LayoutTopologyAfterOptionsHook>> {
    LAYOUT_TOPOLOGY_AFTER_OPTIONS_HOOK
        .lock()
        .expect("layout topology test hook should lock")
        .clone()
}

pub struct WorkspaceLayoutRuntime {
    backend: EmbeddedTmuxBackend,
    control_service: ControlService<EmbeddedTmuxBackend>,
    layout_service: LayoutService<EmbeddedTmuxBackend>,
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    current_executable: std::path::PathBuf,
    network: RemoteNetworkConfig,
}

impl WorkspaceLayoutRuntime {
    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_layout_error)?;
        let current_executable = current_waitagent_executable()?;

        Ok(Self {
            control_service: ControlService::new(backend.clone()),
            layout_service: LayoutService::new(backend.clone()),
            target_registry: TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_network(network.clone())
                    .map_err(tmux_layout_error)?,
            ),
            backend,
            current_executable,
            network,
        })
    }

    #[cfg(test)]
    pub fn new_for_tests(
        backend: EmbeddedTmuxBackend,
        current_executable: PathBuf,
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            control_service: ControlService::new(backend.clone()),
            layout_service: LayoutService::new(backend.clone()),
            target_registry: TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_network(network.clone())
                    .map_err(tmux_layout_error)?,
            ),
            backend,
            current_executable,
            network,
        })
    }

    pub fn ensure_layout(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        let layout = self.ensure_layout_topology(
            workspace,
            workspace_dir,
            LayoutFocusBehavior::ReturnToMain,
        )?;
        self.wait_for_initial_chrome_render(workspace, &layout);
        Ok(())
    }

    pub fn sync_main_slot_bindings(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        if self.native_fullscreen_active(workspace)? {
            return Ok(());
        }
        if self.main_slot_transition_owns_main_pane_metadata(workspace)? {
            ERROR_LOG.log(format!(
                "[diag] sync_main_slot_bindings: skipped while main-slot transition owns main pane metadata for session={:?}",
                workspace.session_name
            ));
            return Ok(());
        }
        self.ensure_layout_topology(workspace, workspace_dir, LayoutFocusBehavior::ReturnToMain)
            .map(|_| ())
    }

    pub fn refresh_workspace_chrome(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        ERROR_LOG.log(format!(
            "[diag] refresh_workspace_chrome: session={:?}, native_fullscreen_active={:?}",
            workspace.session_name,
            self.native_fullscreen_active(workspace)
        ));
        if self.native_fullscreen_active(workspace)? {
            let r = self
                .backend
                .signal_chrome_refresh_on_socket(
                    workspace.socket_name.as_str(),
                    workspace.session_name.as_str(),
                )
                .map_err(tmux_layout_error);
            ERROR_LOG.log(format!(
                "[diag] refresh_workspace_chrome: native_fullscreen path, result={:?}",
                r.is_ok()
            ));
            return r;
        }
        let notified = self.notify_existing_chrome_panes(workspace)?;
        ERROR_LOG.log(format!(
            "[diag] refresh_workspace_chrome: notify_existing={}, will {}",
            notified,
            if notified {
                "return Ok"
            } else {
                "call refresh_chrome"
            }
        ));
        if notified {
            Ok(())
        } else {
            self.refresh_chrome(workspace, workspace_dir)
        }
    }

    pub fn suspend_main_pane_output_bridge(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), LifecycleError> {
        let Some(main_pane) = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(tmux_layout_error)?
            .map(TmuxPaneId::new)
        else {
            return Ok(());
        };
        let bridge_pipe = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_PIPE_OPTION)
            .map_err(tmux_layout_error)?;
        if bridge_pipe.as_deref() == Some(main_pane.as_str()) {
            match self.backend.clear_pane_pipe_if_owner(
                workspace,
                &main_pane,
                WORKSPACE_MAIN_BRIDGE_PIPE_OWNER,
            ) {
                Ok(_) => {}
                Err(error) if error.is_command_failure() => {}
                Err(error) => return Err(tmux_layout_error(error)),
            }
            self.backend
                .set_session_option(workspace, WAITAGENT_MAIN_PANE_PIPE_OPTION, "")
                .map_err(tmux_layout_error)?;
        }
        Ok(())
    }

    pub fn disable_main_pane_output_bridge(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), LifecycleError> {
        self.backend
            .set_session_option(
                workspace,
                WAITAGENT_MAIN_PANE_OUTPUT_BRIDGE_OPTION,
                MAIN_PANE_OUTPUT_BRIDGE_DISABLED,
            )
            .map_err(tmux_layout_error)?;
        self.suspend_main_pane_output_bridge(workspace)
    }

    #[allow(dead_code)]
    pub fn enable_main_pane_output_bridge(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), LifecycleError> {
        self.backend
            .set_session_option(
                workspace,
                WAITAGENT_MAIN_PANE_OUTPUT_BRIDGE_OPTION,
                MAIN_PANE_OUTPUT_BRIDGE_ENABLED,
            )
            .map_err(tmux_layout_error)?;
        let Some(main_pane) = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(tmux_layout_error)?
            .map(TmuxPaneId::new)
        else {
            return Ok(());
        };
        let command = self.main_pane_output_bridge_shell_command(workspace, Path::new(""));
        self.ensure_main_pane_output_bridge(workspace, &main_pane, &command)
    }

    pub fn run_reconcile(&self, command: LayoutReconcileCommand) -> Result<(), LifecycleError> {
        let workspace_dir = PathBuf::from(&command.workspace_dir);
        let workspace = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(command.session_name.clone()),
            socket_name: TmuxSocketName::new(command.socket_name),
            session_name: TmuxSessionName::new(command.session_name),
        };
        if self.layout_reconcile_event_is_stale(&workspace, command.pane_generation.as_deref())? {
            ERROR_LOG.log(format!(
                "[diag-newhost] layout_reconcile skip_stale socket={} session={} event_generation={:?}",
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
                command.pane_generation
            ));
            return self
                .backend
                .signal_chrome_refresh_on_socket(
                    workspace.socket_name.as_str(),
                    workspace.session_name.as_str(),
                )
                .map_err(tmux_layout_error);
        }
        self.reconcile_layout(
            &workspace,
            &workspace_dir,
            LayoutFocusBehavior::PreserveCurrent,
        )
    }

    pub fn run_chrome_refresh(
        &self,
        command: LayoutReconcileCommand,
    ) -> Result<(), LifecycleError> {
        let workspace_dir = PathBuf::from(&command.workspace_dir);
        let workspace = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(command.session_name.clone()),
            socket_name: TmuxSocketName::new(command.socket_name),
            session_name: TmuxSessionName::new(command.session_name),
        };
        self.refresh_chrome(&workspace, &workspace_dir)
    }

    pub fn run_chrome_refresh_signal(&self, command: UiPaneCommand) -> Result<(), LifecycleError> {
        self.backend
            .signal_chrome_refresh_on_socket(&command.socket_name, &command.session_name)
            .map_err(tmux_layout_error)
    }

    pub fn run_chrome_refresh_owner(&self, command: UiPaneCommand) -> Result<(), LifecycleError> {
        self.backend
            .run_chrome_refresh_owner_on_socket(&command.socket_name, &command.session_name)
            .map_err(tmux_layout_error)
    }

    pub fn run_chrome_refresh_all(&self) -> Result<(), LifecycleError> {
        let sessions = self
            .target_registry
            .list_workspace_chrome_targets()
            .map_err(tmux_layout_error)?;
        self.refresh_workspace_chrome_targets(&sessions)
    }

    pub fn run_chrome_refresh_on_socket(&self, socket_name: &str) -> Result<(), LifecycleError> {
        let t_refresh_socket = Instant::now();
        ERROR_LOG.log(format!(
            "[diag-newhost] chrome_refresh_socket start socket={}",
            socket_name
        ));
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env_with_socket_name(socket_name.to_string())
                .map_err(tmux_layout_error)?
                .with_fresh_local_tmux(),
        );
        ERROR_LOG.log(format!(
            "[diag-newhost] chrome_refresh_socket registry_ready socket={} elapsed={:?}",
            socket_name,
            t_refresh_socket.elapsed()
        ));
        let sessions = target_registry
            .list_local_workspace_chrome_targets_on_authority(socket_name)
            .map_err(tmux_layout_error)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] chrome_refresh_socket chrome_targets socket={} count={} elapsed={:?}",
            socket_name,
            sessions.len(),
            t_refresh_socket.elapsed()
        ));
        let result = self.refresh_workspace_chrome_targets(&sessions);
        ERROR_LOG.log(format!(
            "[diag-newhost] chrome_refresh_socket done socket={} ok={} elapsed={:?}",
            socket_name,
            result.is_ok(),
            t_refresh_socket.elapsed()
        ));
        result
    }

    pub fn run_chrome_refresh_signal_on_socket(
        &self,
        socket_name: &str,
    ) -> Result<(), LifecycleError> {
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env_with_socket_name(socket_name.to_string())
                .map_err(tmux_layout_error)?
                .with_fresh_local_tmux(),
        );
        let sessions = target_registry
            .list_local_workspace_chrome_targets_on_authority(socket_name)
            .map_err(tmux_layout_error)?;
        for session in sessions {
            self.backend
                .signal_chrome_refresh_on_socket(
                    session.address.server_id(),
                    session.address.session_id(),
                )
                .map_err(tmux_layout_error)?;
        }
        Ok(())
    }

    fn refresh_workspace_chrome_targets(
        &self,
        sessions: &[crate::domain::session_catalog::ManagedSessionRecord],
    ) -> Result<(), LifecycleError> {
        let t_targets = Instant::now();
        for session in sessions {
            let t_one = Instant::now();
            let Some(workspace_dir) = session.workspace_dir.as_ref() else {
                ERROR_LOG.log(format!(
                    "[diag-newhost] chrome_refresh_target skip_no_workspace_dir target={} total={:?}",
                    session.address.qualified_target(),
                    t_targets.elapsed()
                ));
                continue;
            };
            let workspace = TmuxWorkspaceHandle {
                workspace_id: WorkspaceInstanceId::new(session.address.session_id()),
                socket_name: TmuxSocketName::new(session.address.server_id()),
                session_name: TmuxSessionName::new(session.address.session_id()),
            };
            self.refresh_workspace_chrome(&workspace, workspace_dir)?;
            ERROR_LOG.log(format!(
                "[diag-newhost] chrome_refresh_target done target={} elapsed={:?} total={:?}",
                session.address.qualified_target(),
                t_one.elapsed(),
                t_targets.elapsed()
            ));
        }

        Ok(())
    }

    pub fn run_close_session(&self, command: CloseSessionCommand) -> Result<(), LifecycleError> {
        self.backend
            .run_socket_command(
                &TmuxSocketName::new(command.socket_name),
                &[
                    "kill-session".to_string(),
                    "-t".to_string(),
                    command.session_name,
                ],
            )
            .map_err(tmux_layout_error)?;
        Ok(())
    }

    fn reconcile_layout(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
        focus_behavior: LayoutFocusBehavior,
    ) -> Result<(), LifecycleError> {
        if self.native_fullscreen_active(workspace)? {
            return self
                .backend
                .signal_chrome_refresh_on_socket(
                    workspace.socket_name.as_str(),
                    workspace.session_name.as_str(),
                )
                .map_err(tmux_layout_error);
        }
        if self.main_slot_transition_owns_main_pane_metadata(workspace)? {
            ERROR_LOG.log(format!(
                "[diag] reconcile_layout: skipped while main-slot transition owns main pane metadata for session={:?}",
                workspace.session_name
            ));
            return Ok(());
        }
        self.ensure_layout_topology(workspace, workspace_dir, focus_behavior)
            .map(|_| ())
    }

    fn refresh_chrome(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        let t_total = Instant::now();
        self.ensure_layout_topology(
            workspace,
            workspace_dir,
            LayoutFocusBehavior::PreserveCurrent,
        )?;
        ERROR_LOG.log(format!(
            "[diag-newhost] refresh_chrome ensure_layout_topology socket={} session={} elapsed={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            t_total.elapsed()
        ));
        let t_signal = Instant::now();
        let result = self
            .backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .map_err(tmux_layout_error);
        ERROR_LOG.log(format!(
            "[diag-newhost] refresh_chrome signal socket={} session={} ok={} elapsed={:?} total={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            result.is_ok(),
            t_signal.elapsed(),
            t_total.elapsed()
        ));
        result
    }

    fn notify_existing_chrome_panes(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<bool, LifecycleError> {
        let window = self
            .backend
            .current_window(workspace)
            .map_err(tmux_layout_error)?;
        let panes = self
            .backend
            .list_panes(workspace, &window)
            .map_err(tmux_layout_error)?;
        let Some(sidebar) = panes
            .iter()
            .find(|pane| pane.title == SIDEBAR_PANE_TITLE && !pane.is_dead)
        else {
            return Ok(false);
        };
        let Some(footer) = panes
            .iter()
            .find(|pane| pane.title == FOOTER_PANE_TITLE && !pane.is_dead)
        else {
            return Ok(false);
        };

        let _ = sidebar;
        let _ = footer;
        self.backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .map_err(tmux_layout_error)?;
        Ok(true)
    }

    fn ensure_layout_topology(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
        focus_behavior: LayoutFocusBehavior,
    ) -> Result<crate::domain::workspace_layout::WorkspaceChromeLayout, LifecycleError> {
        let t_total = Instant::now();
        let sidebar_program = self.sidebar_program(workspace, workspace_dir);
        let footer_program = self.footer_program(workspace, workspace_dir);
        let main_pane_pipe_command =
            self.main_pane_output_bridge_shell_command(workspace, workspace_dir);
        let t_generation = Instant::now();
        let pane_generation = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_GENERATION_OPTION)
            .map_err(tmux_layout_error)?
            .filter(|generation| !generation.is_empty())
            .unwrap_or_else(|| "0".to_string());
        ERROR_LOG.log(format!(
            "[diag-newhost] layout_topology pane_generation socket={} session={} generation={} elapsed={:?} total={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            pane_generation,
            t_generation.elapsed(),
            t_total.elapsed()
        ));
        let reconcile_command =
            self.layout_reconcile_hook_command(workspace, workspace_dir, &pane_generation);
        let pane_died_command = self.main_pane_died_hook_command(workspace, &pane_generation);
        let t_options = Instant::now();
        let transition_active = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_TRANSITION_OPTION)
            .map_err(tmux_layout_error)?
            .as_deref()
            == Some("1");
        let previous_main_pane = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(tmux_layout_error)?
            .filter(|pane| !pane.is_empty())
            .map(TmuxPaneId::new);
        ERROR_LOG.log(format!(
            "[diag-newhost] layout_topology options socket={} session={} transition_active={} previous_main={:?} elapsed={:?} total={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            transition_active,
            previous_main_pane,
            t_options.elapsed(),
            t_total.elapsed()
        ));
        #[cfg(test)]
        if let Some(hook) = layout_topology_after_options_hook_for_tests() {
            hook(workspace);
        }
        let t_layout = Instant::now();
        let layout = self
            .layout_service
            .ensure_workspace_layout(workspace, &sidebar_program, &footer_program, focus_behavior)
            .map_err(tmux_layout_error)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] layout_topology ensure_workspace_layout socket={} session={} main={} sidebar={} footer={} elapsed={:?} total={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            layout.main_pane.as_str(),
            layout.sidebar_pane.as_str(),
            layout.footer_pane.as_str(),
            t_layout.elapsed(),
            t_total.elapsed()
        ));
        let footer_bindings = self.footer_menu_bindings(workspace);
        let fullscreen_toggle_command = self.fullscreen_toggle_command(workspace);
        let t_controls = Instant::now();
        self.control_service
            .ensure_native_controls(
                workspace,
                &layout,
                &fullscreen_toggle_command,
                Some(&footer_bindings),
            )
            .map_err(tmux_layout_error)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] layout_topology ensure_native_controls socket={} session={} elapsed={:?} total={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            t_controls.elapsed(),
            t_total.elapsed()
        ));
        if transition_active && previous_main_pane.is_none() {
            ERROR_LOG.log(format!(
                "[diag] ensure_layout_topology: skipped main pane metadata while transition is active for session={:?}",
                workspace.session_name
            ));
            return Ok(layout);
        }
        // Resolve the effective main pane. When the pane designated by
        // @waitagent_main_pane_id (previous_main_pane) is still alive and
        // is not a chrome pane, prefer it over layout.main_pane (which
        // comes from main_pane_id() and picks the first non-chrome pane
        // by list-panes index order). This prevents the 1-cell leftover
        // pane created by create_remote_session_pane (which often has the
        // lowest pane index after swap-pane) from being incorrectly
        // designated as the main pane.
        let main_pane = resolve_effective_main_pane(
            &self.backend,
            workspace,
            previous_main_pane.as_ref(),
            &layout.main_pane,
        );
        if self.main_slot_transition_owns_main_pane_metadata(workspace)? {
            ERROR_LOG.log(format!(
                "[diag] ensure_layout_topology: skipped stale main pane metadata write while main-slot transition owns metadata for session={:?}",
                workspace.session_name
            ));
            return Ok(layout);
        }
        let t_set_main = Instant::now();
        self.backend
            .set_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION, main_pane.as_str())
            .map_err(tmux_layout_error)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] layout_topology set_main_option socket={} session={} pane={} elapsed={:?} total={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            main_pane.as_str(),
            t_set_main.elapsed(),
            t_total.elapsed()
        ));
        let t_bridge = Instant::now();
        if self.main_pane_output_bridge_enabled(workspace)? {
            self.ensure_main_pane_output_bridge(workspace, &main_pane, &main_pane_pipe_command)?;
        } else {
            self.suspend_main_pane_output_bridge(workspace)?;
        }
        ERROR_LOG.log(format!(
            "[diag-newhost] layout_topology main_bridge socket={} session={} elapsed={:?} total={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            t_bridge.elapsed(),
            t_total.elapsed()
        ));
        let t_hooks = Instant::now();
        self.layout_service
            .ensure_layout_hooks(
                workspace,
                &main_pane,
                previous_main_pane.as_ref(),
                &reconcile_command,
                &pane_died_command,
            )
            .map_err(tmux_layout_error)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] layout_topology hooks socket={} session={} elapsed={:?} total={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            t_hooks.elapsed(),
            t_total.elapsed()
        ));
        Ok(layout)
    }

    fn main_slot_transition_owns_main_pane_metadata(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<bool, LifecycleError> {
        let transition_active = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_TRANSITION_OPTION)
            .map_err(tmux_layout_error)?
            .as_deref()
            == Some("1");
        if !transition_active {
            return Ok(false);
        }
        let main_pane = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(tmux_layout_error)?;
        Ok(main_pane.as_deref().is_none_or(str::is_empty))
    }

    fn layout_reconcile_event_is_stale(
        &self,
        workspace: &TmuxWorkspaceHandle,
        event_generation: Option<&str>,
    ) -> Result<bool, LifecycleError> {
        let Some(event_generation) = event_generation.filter(|generation| !generation.is_empty())
        else {
            return Ok(false);
        };
        let current_generation = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_GENERATION_OPTION)
            .map_err(tmux_layout_error)?
            .filter(|generation| !generation.is_empty())
            .unwrap_or_else(|| "0".to_string());
        Ok(current_generation != event_generation)
    }

    fn ensure_main_pane_output_bridge(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
        command: &str,
    ) -> Result<(), LifecycleError> {
        let previous_pipe = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_PIPE_OPTION)
            .map_err(tmux_layout_error)?;
        if let Some(previous_pipe) = previous_pipe.as_deref() {
            if previous_pipe != main_pane.as_str() {
                match self.backend.clear_pane_pipe_if_owner(
                    workspace,
                    &TmuxPaneId::new(previous_pipe),
                    WORKSPACE_MAIN_BRIDGE_PIPE_OWNER,
                ) {
                    Ok(_) => {}
                    Err(error) if error.is_command_failure() => {}
                    Err(error) => return Err(tmux_layout_error(error)),
                }
            }
        }

        let installed = match self.backend.set_pane_pipe_owned_if_available(
            workspace,
            main_pane,
            WORKSPACE_MAIN_BRIDGE_PIPE_OWNER,
            command,
        ) {
            Ok(installed) => installed,
            Err(error) if error.is_command_failure() => false,
            Err(error) => return Err(tmux_layout_error(error)),
        };
        if installed {
            self.backend
                .set_session_option(
                    workspace,
                    WAITAGENT_MAIN_PANE_PIPE_OPTION,
                    main_pane.as_str(),
                )
                .map_err(tmux_layout_error)?;
        } else if previous_pipe.as_deref() == Some(main_pane.as_str()) {
            self.backend
                .set_session_option(workspace, WAITAGENT_MAIN_PANE_PIPE_OPTION, "")
                .map_err(tmux_layout_error)?;
        }

        Ok(())
    }

    fn main_pane_output_bridge_enabled(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<bool, LifecycleError> {
        Ok(self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OUTPUT_BRIDGE_OPTION)
            .map_err(tmux_layout_error)?
            .as_deref()
            == Some(MAIN_PANE_OUTPUT_BRIDGE_ENABLED))
    }

    fn wait_for_initial_chrome_render(
        &self,
        workspace: &TmuxWorkspaceHandle,
        layout: &crate::domain::workspace_layout::WorkspaceChromeLayout,
    ) {
        let t_total = Instant::now();
        let mut sidebar_ready = self
            .backend
            .sidebar_ready_matches(workspace, layout.sidebar_pane.as_str())
            .unwrap_or(false);
        let mut footer_ready = self
            .backend
            .footer_ready_matches(workspace, layout.footer_pane.as_str())
            .unwrap_or(false);
        ERROR_LOG.log(format!(
            "[diag-newhost] wait_initial_chrome initial socket={} session={} sidebar_ready={} footer_ready={} elapsed={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            sidebar_ready,
            footer_ready,
            t_total.elapsed()
        ));
        if sidebar_ready && footer_ready {
            return;
        }

        let (done_tx, done_rx) = mpsc::channel();
        if !sidebar_ready {
            let backend = self.backend.clone();
            let socket_name = workspace.socket_name.as_str().to_string();
            let session_name = workspace.session_name.as_str().to_string();
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                if backend
                    .wait_for_sidebar_ready_on_socket(&socket_name, &session_name)
                    .is_ok()
                {
                    let _ = done_tx.send(InitialChromePane::Sidebar);
                }
            });
        }
        if !footer_ready {
            let backend = self.backend.clone();
            let socket_name = workspace.socket_name.as_str().to_string();
            let session_name = workspace.session_name.as_str().to_string();
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                if backend
                    .wait_for_footer_ready_on_socket(&socket_name, &session_name)
                    .is_ok()
                {
                    let _ = done_tx.send(InitialChromePane::Footer);
                }
            });
        }
        drop(done_tx);

        let deadline = Instant::now() + STARTUP_CHROME_READY_TIMEOUT;
        while !(sidebar_ready && footer_ready) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match done_rx.recv_timeout(remaining) {
                Ok(InitialChromePane::Sidebar) => {
                    sidebar_ready = self
                        .backend
                        .sidebar_ready_matches(workspace, layout.sidebar_pane.as_str())
                        .unwrap_or(true);
                }
                Ok(InitialChromePane::Footer) => {
                    footer_ready = self
                        .backend
                        .footer_ready_matches(workspace, layout.footer_pane.as_str())
                        .unwrap_or(true);
                }
                Err(_) => break,
            }
        }

        ERROR_LOG.log(format!(
            "[diag-newhost] wait_initial_chrome done socket={} session={} sidebar_ready={} footer_ready={} elapsed={:?}",
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            sidebar_ready,
            footer_ready,
            t_total.elapsed()
        ));
        if !(sidebar_ready && footer_ready) {
            let _ = self.backend.signal_sidebar_ready_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            );
            let _ = self.backend.signal_footer_ready_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            );
        }
    }

    fn sidebar_program(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> TmuxProgram {
        TmuxProgram::new(self.current_executable.display().to_string())
            .with_args(prepend_global_network_args(
                vec![
                    "__ui-sidebar".to_string(),
                    "--socket-name".to_string(),
                    workspace.socket_name.as_str().to_string(),
                    "--session-name".to_string(),
                    workspace.session_name.as_str().to_string(),
                ],
                &self.network,
            ))
            .with_start_directory(workspace_dir)
    }

    fn footer_program(&self, workspace: &TmuxWorkspaceHandle, workspace_dir: &Path) -> TmuxProgram {
        TmuxProgram::new(self.current_executable.display().to_string())
            .with_args(prepend_global_network_args(
                vec![
                    "__ui-footer".to_string(),
                    "--socket-name".to_string(),
                    workspace.socket_name.as_str().to_string(),
                    "--session-name".to_string(),
                    workspace.session_name.as_str().to_string(),
                ],
                &self.network,
            ))
            .with_start_directory(workspace_dir)
    }

    fn footer_menu_bindings(&self, workspace: &TmuxWorkspaceHandle) -> FooterMenuBindings {
        let create_target_shell_command = new_target_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
            &self.network,
        );
        let connect_remote_host_popup_command = connect_remote_host_popup_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
            &self.network,
        );
        let create_remote_session_shell_command = create_remote_session_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
            &self.network,
        );
        let shell_command = footer_menu_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
            &self.network,
            &self.network.advertised_listener_label(),
            self.network.connect.as_deref(),
        );
        FooterMenuBindings {
            create_session_command: format!(
                "run-shell -b {}",
                tmux_quote_argument(&create_target_shell_command)
            ),
            connect_remote_host_command: connect_remote_host_popup_command,
            create_remote_session_command: create_remote_session_tmux_command(
                &create_remote_session_shell_command,
            ),
            open_sessions_menu_command: format!(
                "run-shell -b {}",
                tmux_quote_argument(&shell_command)
            ),
            error_log_command: format!(
                "display-popup -w 80% -h 80% -E {}",
                tmux_quote_argument(&format!(
                    "{} __error-log && echo '' && echo '--- Press ENTER to close ---' && read -r",
                    self.current_executable.display(),
                ))
            ),
        }
    }

    fn fullscreen_toggle_command(&self, workspace: &TmuxWorkspaceHandle) -> String {
        fullscreen_toggle_tmux_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
            &self.network,
        )
    }

    fn layout_reconcile_hook_command(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
        pane_generation: &str,
    ) -> String {
        let shell_command = layout_reconcile_hook_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
            &workspace_dir.display().to_string(),
            pane_generation,
            &self.network,
        );
        format!(
            "run-shell -b {}",
            tmux_quote_argument(&format!("{shell_command} >/dev/null 2>&1"))
        )
    }

    pub(crate) fn main_pane_died_hook_command(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane_generation: &str,
    ) -> String {
        let shell_command = main_pane_died_hook_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            pane_generation,
            &self.network,
        );
        format!(
            "run-shell -b {}",
            tmux_quote_argument(&format!("{shell_command} >/dev/null 2>&1"))
        )
    }

    fn main_pane_output_bridge_shell_command(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> String {
        main_pane_output_bridge_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
            &workspace_dir.display().to_string(),
            &self.network,
        )
    }

    fn native_fullscreen_active(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<bool, LifecycleError> {
        let main_pane = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(tmux_layout_error)?
            .map(TmuxPaneId::new)
            .unwrap_or(
                self.backend
                    .current_pane(workspace)
                    .map_err(tmux_layout_error)?,
            );
        self.backend
            .window_zoomed_on_socket(workspace.socket_name.as_str(), main_pane.as_str())
            .map_err(tmux_layout_error)
    }
}

fn create_remote_session_tmux_command(shell_command: &str) -> String {
    format!(
        "run-shell -b {}",
        tmux_quote_argument(&format!("{shell_command} >/dev/null 2>&1 || true"))
    )
}

fn fullscreen_toggle_tmux_command(
    executable: &str,
    workspace: &TmuxWorkspaceHandle,
    network: &RemoteNetworkConfig,
) -> String {
    let shell_command = shell_command_with_network(
        executable,
        vec![
            "__toggle-fullscreen".to_string(),
            "--socket-name".to_string(),
            workspace.socket_name.as_str().to_string(),
            "--session-name".to_string(),
            workspace.session_name.as_str().to_string(),
        ],
        network,
    );
    format!("run-shell -b {}", tmux_quote_argument(&shell_command))
}

fn footer_menu_shell_command(
    executable: &str,
    workspace: &TmuxWorkspaceHandle,
    network: &RemoteNetworkConfig,
    listener_label: &str,
    connect_endpoint: Option<&str>,
) -> String {
    let mut args = vec![
        "__footer-menu".to_string(),
        "--socket-name".to_string(),
        workspace.socket_name.as_str().to_string(),
        "--session-name".to_string(),
        workspace.session_name.as_str().to_string(),
        "--client-tty".to_string(),
        "#{client_tty}".to_string(),
        "--listener-display".to_string(),
        listener_label.to_string(),
    ];
    if let Some(endpoint) = connect_endpoint {
        args.push("--connect-endpoint".to_string());
        args.push(endpoint.to_string());
    }
    shell_command_with_network(executable, args, network)
}

fn new_target_shell_command(
    executable: &str,
    workspace: &TmuxWorkspaceHandle,
    network: &RemoteNetworkConfig,
) -> String {
    shell_command_with_network(
        executable,
        vec![
            "__new-target".to_string(),
            "--current-socket-name".to_string(),
            workspace.socket_name.as_str().to_string(),
            "--current-session-name".to_string(),
            workspace.session_name.as_str().to_string(),
        ],
        network,
    )
}

fn connect_remote_host_popup_command(
    executable: &str,
    workspace: &TmuxWorkspaceHandle,
    network: &RemoteNetworkConfig,
) -> String {
    let pane_command = shell_command_with_network(
        executable,
        vec![
            "__connect-remote-host-pane".to_string(),
            "--current-socket-name".to_string(),
            workspace.socket_name.as_str().to_string(),
            "--current-session-name".to_string(),
            workspace.session_name.as_str().to_string(),
        ],
        network,
    );
    connect_remote_host_display_popup_command(&pane_command)
}

fn create_remote_session_shell_command(
    executable: &str,
    workspace: &TmuxWorkspaceHandle,
    network: &RemoteNetworkConfig,
) -> String {
    shell_command_with_network(
        executable,
        vec![
            "__new-selected-remote-session".to_string(),
            "--current-socket-name".to_string(),
            workspace.socket_name.as_str().to_string(),
            "--current-session-name".to_string(),
            workspace.session_name.as_str().to_string(),
        ],
        network,
    )
}

fn layout_reconcile_hook_shell_command(
    executable: &str,
    workspace: &TmuxWorkspaceHandle,
    workspace_dir: &str,
    pane_generation: &str,
    network: &RemoteNetworkConfig,
) -> String {
    shell_command_with_network(
        executable,
        vec![
            "__layout-reconcile".to_string(),
            "--socket-name".to_string(),
            workspace.socket_name.as_str().to_string(),
            "--session-name".to_string(),
            workspace.session_name.as_str().to_string(),
            "--workspace-dir".to_string(),
            workspace_dir.to_string(),
            "--pane-generation".to_string(),
            pane_generation.to_string(),
        ],
        network,
    )
}

fn main_pane_died_hook_shell_command(
    executable: &str,
    socket_name: &str,
    session_name: &str,
    pane_generation: &str,
    network: &RemoteNetworkConfig,
) -> String {
    shell_command_with_network(
        executable,
        vec![
            "__main-pane-died".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
            "--session-name".to_string(),
            session_name.to_string(),
            "--pane-id".to_string(),
            "#{hook_pane}".to_string(),
            "--pane-generation".to_string(),
            pane_generation.to_string(),
        ],
        network,
    )
}

fn main_pane_output_bridge_shell_command(
    executable: &str,
    workspace: &TmuxWorkspaceHandle,
    _workspace_dir: &str,
    network: &RemoteNetworkConfig,
) -> String {
    main_pane_output_event_bridge_command(
        std::path::Path::new(executable),
        workspace.socket_name.as_str(),
        None,
        network,
    )
}

fn shell_command_with_network(
    executable: &str,
    command_args: Vec<String>,
    network: &RemoteNetworkConfig,
) -> String {
    std::iter::once(executable.to_string())
        .chain(prepend_global_network_args(command_args, network))
        .map(|arg| shell_escape(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_quote_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn connect_remote_host_display_popup_command(pane_command: &str) -> String {
    let command = tmux_quote_argument(pane_command);
    format!(
        "if -F '#{{e|>=:#{{client_width}},100}}' 'display-popup -w 100 -h 18 -E {command}' 'display-popup -w 100% -h 18 -E {command}'"
    )
}

fn resolve_effective_main_pane(
    backend: &EmbeddedTmuxBackend,
    workspace: &TmuxWorkspaceHandle,
    previous: Option<&TmuxPaneId>,
    suggested: &TmuxPaneId,
) -> TmuxPaneId {
    let Ok(window) = backend.current_window(workspace) else {
        return suggested.clone();
    };
    let Ok(panes) = backend.list_panes(workspace, &window) else {
        return suggested.clone();
    };

    let is_valid_content_pane = |pane_id: &TmuxPaneId| {
        panes.iter().any(|pane| {
            pane.pane_id == *pane_id
                && !pane.is_dead
                && pane.title != SIDEBAR_PANE_TITLE
                && pane.title != FOOTER_PANE_TITLE
        })
    };

    if previous.is_some_and(|pane| is_valid_content_pane(pane)) {
        return previous.cloned().unwrap();
    }
    if is_valid_content_pane(suggested) {
        return suggested.clone();
    }
    panes
        .iter()
        .find(|pane| {
            !pane.is_dead && pane.title != SIDEBAR_PANE_TITLE && pane.title != FOOTER_PANE_TITLE
        })
        .map(|pane| pane.pane_id.clone())
        .unwrap_or_else(|| suggested.clone())
}

fn tmux_layout_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to ensure tmux-owned waitagent layout".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

#[cfg(test)]
fn should_refresh_workspace_chrome(
    session: &crate::domain::session_catalog::ManagedSessionRecord,
) -> bool {
    matches!(
        session.session_role,
        Some(crate::domain::workspace::WorkspaceSessionRole::WorkspaceChrome)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        connect_remote_host_popup_command, create_remote_session_shell_command,
        create_remote_session_tmux_command, footer_menu_shell_command,
        fullscreen_toggle_tmux_command, layout_reconcile_hook_shell_command,
        main_pane_died_hook_shell_command, main_pane_output_bridge_shell_command,
        should_refresh_workspace_chrome, tmux_quote_argument,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::domain::workspace::WorkspaceInstanceId;
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::tmux::{TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle};
    use std::path::PathBuf;

    #[test]
    fn footer_menu_shell_command_quotes_shell_arguments_but_not_tmux_layer() {
        let workspace = workspace();

        let command = footer_menu_shell_command(
            "/tmp/wait agent",
            &workspace,
            &RemoteNetworkConfig::default(),
            "192.168.1.22:7474",
            None,
        );

        assert_eq!(
            command,
            "'/tmp/wait agent' '--port' '7474' '__footer-menu' '--socket-name' 'wa-1' '--session-name' 'session-1' '--client-tty' '#{client_tty}' '--listener-display' '192.168.1.22:7474'"
        );
    }

    #[test]
    fn footer_menu_shell_command_includes_connect_endpoint_when_provided() {
        let workspace = workspace();

        let command = footer_menu_shell_command(
            "/tmp/waitagent",
            &workspace,
            &RemoteNetworkConfig {
                port: 9001,
                connect: Some("10.0.0.8:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            },
            "192.168.1.22:7474",
            Some("10.0.0.5:7474"),
        );

        assert!(command.contains("'--port' '9001'"));
        assert!(command.contains("'--connect' '10.0.0.8:7474'"));
        assert!(command.contains("'--connect-endpoint'"));
        assert!(command.contains("'10.0.0.5:7474'"));
        assert!(command.contains("'--listener-display'"));
    }

    #[test]
    fn remote_creation_shell_commands_target_current_workspace() {
        let workspace = workspace();
        let network = RemoteNetworkConfig {
            port: 9001,
            connect: Some("10.0.0.8:7474".to_string()),
            node_id: None,
            public_endpoint: None,
        };

        let connect = connect_remote_host_popup_command("/tmp/wait agent", &workspace, &network);
        let remote_new =
            create_remote_session_shell_command("/tmp/wait agent", &workspace, &network);

        assert!(connect.contains("'__connect-remote-host-pane'"));
        assert!(connect.contains("'--current-socket-name' 'wa-1'"));
        assert!(connect.contains("'--connect' '10.0.0.8:7474'"));
        assert!(connect.contains("#{client_width}"));
        assert!(connect.contains("display-popup -w 100 -h 18 -E"));
        assert!(connect.contains("display-popup -w 100% -h 18 -E"));
        assert!(remote_new.contains("'__new-selected-remote-session'"));
        assert!(remote_new.contains("'--current-session-name' 'session-1'"));
        assert!(remote_new.contains("'--port' '9001'"));

        let tmux_command = create_remote_session_tmux_command(&remote_new);
        assert!(tmux_command.contains("__new-selected-remote-session"));
        assert!(tmux_command.contains(">/dev/null 2>&1 || true"));
    }

    #[test]
    fn tmux_quote_argument_wraps_shell_command_for_tmux_parser() {
        let quoted =
            tmux_quote_argument("'waitagent' '__footer-menu' '--client-tty' '#{client_tty}'");

        assert_eq!(
            quoted,
            "\"'waitagent' '__footer-menu' '--client-tty' '#{client_tty}'\""
        );
    }

    #[test]
    fn fullscreen_toggle_tmux_command_targets_current_workspace() {
        let workspace = workspace();

        let command = fullscreen_toggle_tmux_command(
            "/tmp/wait agent",
            &workspace,
            &RemoteNetworkConfig::default(),
        );
        let expected_shell = "'/tmp/wait agent' '--port' '7474' '__toggle-fullscreen' '--socket-name' 'wa-1' '--session-name' 'session-1'";

        assert_eq!(
            command,
            format!("run-shell -b {}", tmux_quote_argument(expected_shell))
        );
    }

    #[test]
    fn layout_reconcile_hook_shell_command_preserves_workspace_directory_as_shell_argument() {
        let workspace = workspace();

        let command = layout_reconcile_hook_shell_command(
            "/tmp/wait agent",
            &workspace,
            "/tmp/demo path",
            "7",
            &RemoteNetworkConfig::default(),
        );

        assert_eq!(
            command,
            "'/tmp/wait agent' '--port' '7474' '__layout-reconcile' '--socket-name' 'wa-1' '--session-name' 'session-1' '--workspace-dir' '/tmp/demo path' '--pane-generation' '7'"
        );
    }

    #[test]
    fn main_pane_died_hook_shell_command_targets_current_session() {
        let command = main_pane_died_hook_shell_command(
            "/tmp/wait agent",
            "wa-1",
            "session-1",
            "7",
            &RemoteNetworkConfig::default(),
        );

        assert_eq!(
            command,
            "'/tmp/wait agent' '--port' '7474' '__main-pane-died' '--socket-name' 'wa-1' '--session-name' 'session-1' '--pane-id' '#{hook_pane}' '--pane-generation' '7'"
        );
    }

    #[test]
    fn main_pane_output_bridge_shell_command_refreshes_on_output_lines() {
        let workspace = workspace();

        let command = main_pane_output_bridge_shell_command(
            "/tmp/wait agent",
            &workspace,
            "/tmp/demo path",
            &RemoteNetworkConfig::default(),
        );

        assert_eq!(
            command,
            "'/tmp/wait agent' '--port' '7474' '__main-pane-output-event-bridge' '--socket-name' 'wa-1'"
        );
    }

    #[test]
    fn chrome_refresh_all_only_tracks_workspace_chrome_sessions() {
        let chrome = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1", "session-1"),
            selector: Some("wa-1:session-1".to_string()),
            availability: crate::domain::session_catalog::SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        };
        let target = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1", "session-2"),
            selector: Some("wa-1:session-2".to_string()),
            availability: crate::domain::session_catalog::SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Running,
        };

        assert!(should_refresh_workspace_chrome(&chrome));
        assert!(!should_refresh_workspace_chrome(&target));
    }

    fn workspace() -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("session-1"),
            socket_name: TmuxSocketName::new("wa-1"),
            session_name: TmuxSessionName::new("session-1"),
        }
    }
}
