use crate::cli::Cli;
use crate::error::AppError;
use crate::infra::error_log::ERROR_LOG;
use crate::runtime::network_state_runtime::command_network_config;

// The accepted local default route is: bootstrap -> command dispatch ->
// workspace command runtime. Event-r4 keeps that path explicit so new local
// behavior does not drift back into ad hoc or historical entrypoints.
pub fn run() -> Result<(), AppError> {
    let args: Vec<String> = std::env::args().collect();
    ERROR_LOG.log(format!(
        "[diag] bootstrap: pid={} args={:?}",
        std::process::id(),
        args
    ));
    let cli = Cli::parse(std::env::args_os())?;
    let network = command_network_config(cli.network.clone(), cli.network_explicit, &cli.command);
    let dispatcher =
        crate::command::dispatch::CommandDispatcher::from_build_env_with_network_and_command(
            network,
            &cli.command,
        )?;

    let command = cli.command.clone();
    let result = dispatcher.dispatch(cli.command);
    if let Err(error) = &result {
        ERROR_LOG.log(format!(
            "[diag-error] dispatch failed: command={command:?} error={error:?}"
        ));
    }
    result
}
