use std::error::Error;
use std::fmt;

#[derive(Debug)]
pub enum AppError {
    Cli(crate::cli::CliError),
    Lifecycle(crate::lifecycle::LifecycleError),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli(error) => write!(f, "{error}"),
            Self::Lifecycle(error) => write!(f, "{error}"),
        }
    }
}

impl Error for AppError {}

impl From<crate::cli::CliError> for AppError {
    fn from(value: crate::cli::CliError) -> Self {
        Self::Cli(value)
    }
}

impl From<crate::lifecycle::LifecycleError> for AppError {
    fn from(value: crate::lifecycle::LifecycleError) -> Self {
        Self::Lifecycle(value)
    }
}
