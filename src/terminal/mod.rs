#![allow(dead_code)]

mod ansi;
mod engine;
pub(crate) mod platform;
mod runtime;
mod types;

pub use engine::TerminalEngine;
pub use runtime::TerminalRuntime;
pub use types::{
    MouseEncoding, MouseReportingMode, ScreenSnapshot, ScreenState, TerminalError, TerminalSize,
};

pub(crate) use types::{ColorValue, TextStyle};

pub(crate) use ansi::parse_ansi_styled_line;

#[cfg(test)]
mod tests;
