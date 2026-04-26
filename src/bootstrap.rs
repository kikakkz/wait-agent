use crate::app::AppError;
use crate::cli::Cli;
use crate::config::AppConfig;

pub fn run() -> Result<(), AppError> {
    let cli = Cli::parse(std::env::args_os())?;
    let config = AppConfig::from_env();
    let dispatcher = crate::command::dispatch::CommandDispatcher::from_build_env()?;

    dispatcher.dispatch(cli.command, config)
}
