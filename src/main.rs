mod application;
mod bootstrap;
mod cli;
mod command;
mod domain;
mod error;
mod event;
mod infra;
mod lifecycle;
mod runtime;
mod terminal;
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
