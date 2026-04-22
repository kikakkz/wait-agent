pub fn run_command(
    command: crate::cli::Command,
    config: crate::config::AppConfig,
) -> Result<(), crate::app::AppError> {
    crate::app::run_command(command, config)
}
