use crate::app::AppError;
use crate::cli::Cli;
use crate::config::AppConfig;

pub fn run() -> Result<(), AppError> {
    let cli = Cli::parse(std::env::args_os())?;
    let config = AppConfig::from_env();

    crate::command::dispatch::CommandDispatcher::dispatch(cli.command, config)
}
