use crate::cli::{Cli, Command, RunCommand, ServerCommand};
use crate::config::AppConfig;
use crate::pty::{PtyManager, PtySize, SpawnRequest};
use crate::session::{SessionAddress, SessionRegistry};
use crate::terminal::{TerminalEngine, TerminalRuntime};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

pub fn run() -> Result<(), AppError> {
    let cli = Cli::parse(std::env::args_os())?;
    let config = AppConfig::from_env();
    let mut app = App::new(config);
    print_banner();

    app.execute(cli.command)
}

struct App {
    config: AppConfig,
    sessions: SessionRegistry,
    pty: PtyManager,
    terminal: TerminalRuntime,
}

impl App {
    fn new(config: AppConfig) -> Self {
        Self {
            config,
            sessions: SessionRegistry::new(),
            pty: PtyManager::new(),
            terminal: TerminalRuntime::stdio(),
        }
    }

    fn execute(&mut self, command: Command) -> Result<(), AppError> {
        match command {
            Command::Run(run) => self.handle_run(run),
            Command::Server(server) => self.handle_server(server),
            Command::Help(help) => {
                println!("{help}");
                Ok(())
            }
        }
    }

    fn handle_run(&mut self, command: RunCommand) -> Result<(), AppError> {
        if command.program.is_empty() {
            return Err(AppError::InvalidCommand(
                "run requires an agent command".to_string(),
            ));
        }

        let runtime = self
            .config
            .runtime_for_run(command.node_id.as_deref(), command.connect.as_deref());
        let command_line = command.command_line();
        let title = command.program.clone();
        let terminal_snapshot = self.terminal.snapshot()?;
        let session =
            self.sessions
                .create_local_session(runtime.node.node_id.clone(), title, command_line);
        let mut screen_engine = TerminalEngine::new(terminal_snapshot.size);
        let mut handle = self.pty.spawn(
            session.address().clone(),
            SpawnRequest {
                program: command.program,
                args: command.args,
                size: PtySize::from(terminal_snapshot.size),
            },
        )?;
        self.sessions
            .mark_running(session.address(), handle.process_id());
        self.sessions
            .update_screen_state(session.address(), screen_engine.state());

        print_runtime_header("run", &runtime, Some(session.address()));
        println!("agent_command: {}", session.command_line);
        println!("pty_id: {}", handle.pty_id());
        println!("status: running");
        println!(
            "terminal_size: {}x{}",
            handle.size().cols,
            handle.size().rows
        );
        println!(
            "console_tty: input={}, output={}",
            terminal_snapshot.input_is_tty, terminal_snapshot.output_is_tty
        );
        if let Some(process_id) = handle.process_id() {
            println!("process_id: {process_id}");
        }
        if let Some(connect_addr) = runtime.network.access_point.as_deref() {
            println!("mirror: enabled");
            println!("mirror_target: {connect_addr}");
        } else {
            println!("mirror: disabled");
        }
        println!(
            "note: raw mode is implemented in the terminal layer; live stdin routing lands with the console runtime."
        );
        println!();

        let output = handle.read_to_end()?;
        if !output.is_empty() {
            self.sessions.mark_output(session.address());
            screen_engine.feed(&output);
            self.sessions
                .update_screen_state(session.address(), screen_engine.state());
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(&output)
                .map_err(|error| AppError::Io("failed to write PTY output".to_string(), error))?;
            stdout
                .flush()
                .map_err(|error| AppError::Io("failed to flush PTY output".to_string(), error))?;
        }

        let exit_status = handle.wait()?;
        self.sessions.mark_exited(session.address());
        self.pty.release(session.address());
        println!();
        println!("session_exit: {}", format_exit_status(exit_status));

        Ok(())
    }

    fn handle_server(&mut self, command: ServerCommand) -> Result<(), AppError> {
        let runtime = self
            .config
            .runtime_for_server(command.listen.as_deref(), command.node_id.as_deref());

        print_runtime_header("server", &runtime, None);
        println!("listen_addr: {}", runtime.network.listen_addr);
        println!("status: stub");
        println!("note: transport and aggregate session view are deferred to Stage B.");
        println!(
            "note: local agents may later run directly on the server or mirror in via `waitagent run --connect {}`.",
            runtime.network.listen_addr
        );
        Ok(())
    }
}

fn format_exit_status(status: crate::pty::ExitStatus) -> String {
    if status.success() {
        "success".to_string()
    } else {
        status.to_string()
    }
}

fn print_banner() {
    println!(
        r#" __        __    _ _      _                            _
 \ \      / /_ _(_) |_   / \   __ _  ___ _ __   __ _ | |_
  \ \ /\ / / _` | | __| / _ \ / _` |/ _ \ '_ \ / _` || __|
   \ V  V / (_| | | |_ / ___ \ (_| |  __/ | | | (_| || |_
    \_/\_/ \__,_|_|\__/_/   \_\__, |\___|_| |_|\__,_| \__|
                              |___/
"#
    );
    println!("One terminal. Many agents. Zero tab thrash.");
    println!();
}

fn print_runtime_header(command: &str, config: &AppConfig, session: Option<&SessionAddress>) {
    println!("waitagent_command: {command}");
    println!("node_id: {}", config.node.node_id);
    println!("mode: {}", config.mode_name());
    println!("listen_addr: {}", config.network.listen_addr);
    println!("access_point: {}", config.network.access_point_display());

    if let Some(address) = session {
        println!("session: {address}");
    }
}

#[derive(Debug)]
pub enum AppError {
    Cli(crate::cli::CliError),
    InvalidCommand(String),
    Pty(crate::pty::PtyError),
    Terminal(crate::terminal::TerminalError),
    Io(String, io::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli(error) => write!(f, "{error}"),
            Self::InvalidCommand(message) => write!(f, "invalid command: {message}"),
            Self::Pty(error) => write!(f, "{error}"),
            Self::Terminal(error) => write!(f, "{error}"),
            Self::Io(context, error) => write!(f, "{context}: {error}"),
        }
    }
}

impl Error for AppError {}

impl From<crate::cli::CliError> for AppError {
    fn from(value: crate::cli::CliError) -> Self {
        Self::Cli(value)
    }
}

impl From<crate::pty::PtyError> for AppError {
    fn from(value: crate::pty::PtyError) -> Self {
        Self::Pty(value)
    }
}

impl From<crate::terminal::TerminalError> for AppError {
    fn from(value: crate::terminal::TerminalError) -> Self {
        Self::Terminal(value)
    }
}
