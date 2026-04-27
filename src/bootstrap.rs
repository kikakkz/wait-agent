use crate::cli::Cli;
use crate::error::AppError;

pub fn run() -> Result<(), AppError> {
    let cli = Cli::parse(std::env::args_os())?;
    let dispatcher = crate::command::dispatch::CommandDispatcher::from_build_env()?;

    dispatcher.dispatch(cli.command)
}
