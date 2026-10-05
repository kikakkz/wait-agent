//! `waitagent web serve` command wiring: resolves `--listen` against the
//! loopback default, then drives the web service runtime (enrollment into
//! the pinned relay first, then the axum listener until Ctrl-C).

use crate::cli::WebServeCommand;
use crate::error::AppError;
use crate::lifecycle::LifecycleError;
use crate::web::serve::WebServeConfig;

pub fn run(command: WebServeCommand) -> Result<(), AppError> {
    let mut config = WebServeConfig::from_waitagent_home();
    if let Some(listen) = &command.listen {
        config.listen = listen.parse().map_err(|error| {
            AppError::Lifecycle(LifecycleError::Protocol(format!(
                "invalid --listen address {listen:?}: {error}"
            )))
        })?;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            AppError::Lifecycle(LifecycleError::Io("build web runtime".to_string(), error))
        })?;
    runtime
        .block_on(crate::web::serve::run(&config))
        .map_err(|error| AppError::Lifecycle(LifecycleError::Protocol(error.to_string())))
}
