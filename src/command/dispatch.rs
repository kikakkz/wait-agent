use crate::app::AppError;
use crate::cli::Command;
use crate::config::AppConfig;
use crate::ui::banner::print_banner;

pub struct CommandDispatcher;

impl CommandDispatcher {
    pub fn dispatch(command: Command, config: AppConfig) -> Result<(), AppError> {
        match command {
            Command::Workspace(workspace) => {
                crate::lifecycle::run_workspace_entry(config, workspace).map_err(AppError::from)
            }
            Command::Daemon(command) => {
                crate::lifecycle::run_daemon(config, command).map_err(AppError::from)
            }
            Command::Attach(command) => {
                crate::lifecycle::run_attach(command).map_err(AppError::from)
            }
            Command::List(command) => crate::lifecycle::run_list(command).map_err(AppError::from),
            Command::Status(command) => {
                crate::lifecycle::run_status(command).map_err(AppError::from)
            }
            Command::Detach(command) => {
                crate::lifecycle::run_detach(command).map_err(AppError::from)
            }
            Command::Help(help) => {
                print_banner();
                println!("{help}");
                Ok(())
            }
            other => crate::legacy::run_command(other, config),
        }
    }
}
