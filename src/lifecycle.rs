use crate::terminal::TerminalError;
use std::fmt;
use std::io;

#[derive(Debug)]
pub enum LifecycleError {
    Io(String, io::Error),
    Protocol(String),
    Pty(crate::pty::PtyError),
    Terminal(TerminalError),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(context, error) => write!(f, "{context}: {error}"),
            Self::Protocol(message) => write!(f, "{message}"),
            Self::Pty(error) => write!(f, "{error}"),
            Self::Terminal(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<crate::pty::PtyError> for LifecycleError {
    fn from(value: crate::pty::PtyError) -> Self {
        Self::Pty(value)
    }
}

impl From<TerminalError> for LifecycleError {
    fn from(value: TerminalError) -> Self {
        Self::Terminal(value)
    }
}
