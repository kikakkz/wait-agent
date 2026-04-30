use crate::cli::Cli;
use crate::error::AppError;

// The accepted local default route is: bootstrap -> command dispatch ->
// workspace command runtime. Event-r4 keeps that path explicit so new local
// behavior does not drift back into ad hoc or historical entrypoints.
pub fn run() -> Result<(), AppError> {
    let cli = Cli::parse(std::env::args_os())?;
    let dispatcher =
        crate::command::dispatch::CommandDispatcher::from_build_env_with_network_and_command(
            cli.network.clone(),
            &cli.command,
        )?;

    dispatcher.dispatch(cli.command)
}
