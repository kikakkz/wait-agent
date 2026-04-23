use crate::app::AppError;
use crate::cli::Command;
use crate::config::AppConfig;
use crate::runtime::footer_menu_runtime::FooterMenuRuntime;
use crate::runtime::ui_pane_runtime::UiPaneRuntime;
use crate::runtime::workspace_command_runtime::WorkspaceCommandRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::ui::banner::print_banner;

pub struct CommandDispatcher {
    workspace_runtime: WorkspaceCommandRuntime,
    ui_pane_runtime: UiPaneRuntime,
    footer_menu_runtime: FooterMenuRuntime,
    layout_runtime: WorkspaceLayoutRuntime,
}

impl CommandDispatcher {
    pub fn from_build_env() -> Result<Self, AppError> {
        Ok(Self {
            workspace_runtime: WorkspaceCommandRuntime::from_build_env()?,
            ui_pane_runtime: UiPaneRuntime::from_build_env()?,
            footer_menu_runtime: FooterMenuRuntime::from_build_env()?,
            layout_runtime: WorkspaceLayoutRuntime::from_build_env()?,
        })
    }

    pub fn dispatch(&self, command: Command, config: AppConfig) -> Result<(), AppError> {
        match command {
            Command::Workspace(command)
                if command.node_id.is_some() || command.connect.is_some() =>
            {
                crate::legacy::run_command(Command::Workspace(command), config)
            }
            Command::Workspace(command) => self
                .workspace_runtime
                .run_workspace_entry(config, command)
                .map_err(AppError::from),
            Command::UiSidebar(command) => self
                .ui_pane_runtime
                .run_sidebar(command)
                .map_err(AppError::from),
            Command::UiFooter(command) => self
                .ui_pane_runtime
                .run_footer(command)
                .map_err(AppError::from),
            Command::FooterMenu(command) => self
                .footer_menu_runtime
                .run(command)
                .map_err(AppError::from),
            Command::LayoutReconcile(command) => self
                .layout_runtime
                .run_reconcile(command)
                .map_err(AppError::from),
            Command::Daemon(command) => self
                .workspace_runtime
                .run_daemon(config, command)
                .map_err(AppError::from),
            Command::Attach(command) => self
                .workspace_runtime
                .run_attach(command)
                .map_err(AppError::from),
            Command::List(command) => self
                .workspace_runtime
                .run_list(command)
                .map_err(AppError::from),
            Command::Detach(command) => self
                .workspace_runtime
                .run_detach(command)
                .map_err(AppError::from),
            Command::Help(help) => {
                print_banner();
                println!("{help}");
                Ok(())
            }
            other => crate::legacy::run_command(other, config),
        }
    }
}
