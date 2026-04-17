mod app;
mod cli;
mod client;
mod config;
mod console;
mod event;
mod lifecycle;
mod pty;
mod renderer;
mod scheduler;
mod server;
mod session;
mod terminal;
mod transport;

use std::process::ExitCode;

fn main() -> ExitCode {
    match app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
