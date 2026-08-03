use std::fmt;
use std::io;

#[derive(Debug)]
pub enum LifecycleError {
    Io(String, io::Error),
    Protocol(String),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(context, error) => write!(f, "{context}: {error}"),
            Self::Protocol(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for LifecycleError {}
