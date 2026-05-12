#![allow(dead_code)]

mod ansi;
mod engine;
mod platform;
mod runtime;
mod types;

pub use engine::TerminalEngine;
pub use runtime::TerminalRuntime;
pub use types::{ScreenSnapshot, ScreenState, TerminalError, TerminalSize};

#[cfg(test)]
mod tests;
