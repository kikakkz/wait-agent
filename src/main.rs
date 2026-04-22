mod agent;
mod app;
mod application;
mod bootstrap;
mod cli;
mod client;
mod command;
mod config;
mod console;
mod domain;
mod event;
mod infra;
mod legacy;
mod lifecycle;
mod pty;
mod renderer;
mod runtime;
mod scheduler;
mod server;
mod session;
mod terminal;
mod transcript;
mod transport;
mod ui;

use std::process::ExitCode;

fn main() -> ExitCode {
    match bootstrap::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
