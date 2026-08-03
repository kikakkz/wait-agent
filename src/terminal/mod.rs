mod ansi;
mod engine;
mod types;

pub use engine::TerminalEngine;
pub use types::{ScreenSnapshot, ScreenState, TerminalSize};

pub(crate) use types::{ColorValue, TextStyle};

pub(crate) use ansi::parse_ansi_styled_line;

#[cfg(test)]
mod tests;
