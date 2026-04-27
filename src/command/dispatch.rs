use crate::cli::Command;
use crate::error::AppError;
use crate::runtime::event_driven_pane_runtime::EventDrivenPaneRuntime;
use crate::runtime::footer_menu_runtime::FooterMenuRuntime;
use crate::runtime::workspace_command_runtime::WorkspaceCommandRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::ui::banner::print_banner;

pub struct CommandDispatcher {
    workspace_runtime: WorkspaceCommandRuntime,
    pane_runtime: EventDrivenPaneRuntime,
    footer_menu_runtime: FooterMenuRuntime,
    layout_runtime: WorkspaceLayoutRuntime,
}

impl CommandDispatcher {
    pub fn from_build_env() -> Result<Self, AppError> {
        Ok(Self {
            workspace_runtime: WorkspaceCommandRuntime::from_build_env()?,
            pane_runtime: EventDrivenPaneRuntime::from_build_env()?,
            footer_menu_runtime: FooterMenuRuntime::from_build_env()?,
            layout_runtime: WorkspaceLayoutRuntime::from_build_env()?,
        })
    }

    pub fn dispatch(&self, command: Command) -> Result<(), AppError> {
        match command {
            Command::Workspace => self
                .workspace_runtime
                .run_workspace_entry()
                .map_err(AppError::from),
            Command::UiSidebar(command) => self
                .pane_runtime
                .run_sidebar(command)
                .map_err(AppError::from),
            Command::UiFooter(command) => self
                .pane_runtime
                .run_footer(command)
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
