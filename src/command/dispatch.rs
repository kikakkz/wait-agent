use crate::cli::Command;
use crate::error::AppError;
use crate::runtime::event_driven_pane_runtime::EventDrivenPaneRuntime;
use crate::runtime::footer_menu_runtime::FooterMenuRuntime;
use crate::runtime::remote_authority_target_host_runtime::RemoteAuthorityTargetHostRuntime;
use crate::runtime::remote_main_slot_pane_runtime::RemoteMainSlotPaneRuntime;
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::workspace_command_runtime::WorkspaceCommandRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::ui::banner::print_banner;

// This dispatcher is the single command-side ownership boundary for the
// accepted local default route. `workspace` and `attach` must continue to flow
// into `WorkspaceCommandRuntime`, while hidden chrome panes stay on the
// dedicated event-driven pane runtimes.
pub struct CommandDispatcher {
    workspace_runtime: WorkspaceCommandRuntime,
    pane_runtime: EventDrivenPaneRuntime,
    footer_menu_runtime: FooterMenuRuntime,
    remote_authority_target_host_runtime: RemoteAuthorityTargetHostRuntime,
    remote_main_slot_pane_runtime: RemoteMainSlotPaneRuntime,
    remote_target_publication_runtime: RemoteTargetPublicationRuntime,
    layout_runtime: WorkspaceLayoutRuntime,
}

impl CommandDispatcher {
    pub fn from_build_env() -> Result<Self, AppError> {
        Ok(Self {
            workspace_runtime: WorkspaceCommandRuntime::from_build_env()?,
            pane_runtime: EventDrivenPaneRuntime::from_build_env()?,
            footer_menu_runtime: FooterMenuRuntime::from_build_env()?,
            remote_authority_target_host_runtime: RemoteAuthorityTargetHostRuntime::from_build_env(
            )?,
            remote_main_slot_pane_runtime: RemoteMainSlotPaneRuntime::from_build_env()?,
            remote_target_publication_runtime: RemoteTargetPublicationRuntime::from_build_env()?,
            layout_runtime: WorkspaceLayoutRuntime::from_build_env()?,
        })
    }

    pub fn dispatch(&self, command: Command) -> Result<(), AppError> {
        match command {
            Command::Workspace => self
                .workspace_runtime
                .run_workspace_entry()
                .map_err(AppError::from),
            Command::ChromeRefreshSocket(command) => self
                .layout_runtime
                .run_chrome_refresh_on_socket(&command.socket_name)
                .map_err(AppError::from),
            Command::UiSidebar(command) => self
                .pane_runtime
                .run_sidebar(command)
                .map_err(AppError::from),
            Command::UiFooter(command) => self
                .pane_runtime
                .run_footer(command)
                .map_err(AppError::from),
            Command::RemoteMainSlot(command) => self
                .remote_main_slot_pane_runtime
                .run(command)
                .map_err(AppError::from),
            Command::RemoteAuthorityTargetHost(command) => self
                .remote_authority_target_host_runtime
                .run_target_host(command)
                .map_err(AppError::from),
            Command::RemoteAuthorityOutputPump(command) => self
                .remote_authority_target_host_runtime
                .run_output_pump(command)
                .map_err(AppError::from),
            Command::RemoteTargetPublicationServer(command) => self
                .remote_target_publication_runtime
                .run_publication_server(command)
                .map_err(AppError::from),
            Command::RemoteTargetPublicationAgent(command) => self
                .remote_target_publication_runtime
                .run_publication_agent(command)
                .map_err(AppError::from),
            Command::RemoteTargetPublicationOwner(command) => self
                .remote_target_publication_runtime
                .run_publication_owner(command)
                .map_err(AppError::from),
            Command::SocketLifecycleHook(command) => {
                let socket_name = command.socket_name.clone();
                self.remote_target_publication_runtime
                    .run_socket_lifecycle_hook(command)
                    .map_err(AppError::from)?;
                self.layout_runtime
                    .run_chrome_refresh_on_socket(&socket_name)
                    .map_err(AppError::from)
            }
            Command::RemoteTargetBindPublication(command) => self
                .remote_target_publication_runtime
                .run_bind_publication(command)
                .map_err(AppError::from),
            Command::RemoteTargetUnbindPublication(command) => self
                .remote_target_publication_runtime
                .run_unbind_publication(command)
                .map_err(AppError::from),
            Command::RemoteTargetReconcilePublications(command) => self
                .remote_target_publication_runtime
                .run_reconcile_publications(command)
                .map_err(AppError::from),
            Command::ActivateTarget(command) => self
                .workspace_runtime
                .run_activate_target(command)
                .map_err(AppError::from),
            Command::NewTarget(command) => self
                .workspace_runtime
                .run_new_target(command)
                .map_err(AppError::from),
            Command::MainPaneDied(command) => self
                .workspace_runtime
                .run_main_pane_died(command)
                .map_err(AppError::from),
            Command::FooterMenu(command) => self
                .footer_menu_runtime
                .run(command)
                .map_err(AppError::from),
            Command::ToggleFullscreen(command) => self
                .workspace_runtime
                .run_toggle_fullscreen(command)
                .map_err(AppError::from),
            Command::CloseSession(command) => self
                .layout_runtime
                .run_close_session(command)
                .map_err(AppError::from),
            Command::LayoutReconcile(command) => self
                .layout_runtime
                .run_reconcile(command)
                .map_err(AppError::from),
            Command::ChromeRefresh(command) => self
                .layout_runtime
                .run_chrome_refresh(command)
                .map_err(AppError::from),
            Command::ChromeRefreshSignal(command) => self
                .layout_runtime
                .run_chrome_refresh_signal(command)
                .map_err(AppError::from),
            Command::ChromeRefreshAll => self
                .layout_runtime
                .run_chrome_refresh_all()
                .map_err(AppError::from),
            Command::Attach(command) => self
                .workspace_runtime
                .run_attach(command)
                .map_err(AppError::from),
            Command::List => self.workspace_runtime.run_list().map_err(AppError::from),
            Command::Detach(command) => self
                .workspace_runtime
                .run_detach(command)
                .map_err(AppError::from),
            Command::Help(help) => {
                print_banner();
                println!("{help}");
                Ok(())
            }
        }
    }
}
