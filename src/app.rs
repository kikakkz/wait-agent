use crate::cli::{Cli, Command, RunCommand, ServerCommand};
use crate::config::AppConfig;
use crate::session::{SessionAddress, SessionRegistry};
use std::error::Error;
use std::fmt;

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
}

impl App {
    fn new(config: AppConfig) -> Self {
        Self {
            config,
            sessions: SessionRegistry::new(),
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
        let session =
            self.sessions
                .create_local_session(runtime.node.node_id.clone(), title, command_line);

        print_runtime_header("run", &runtime, Some(session.address()));
        println!("agent_command: {}", session.command_line);
        println!("status: bootstrapped");
        if let Some(connect_addr) = runtime.network.access_point.as_deref() {
            println!("mirror: enabled");
            println!("mirror_target: {connect_addr}");
        } else {
            println!("mirror: disabled");
        }
        println!(
            "note: PTY execution is not implemented yet; command and config plumbing are ready."
        );

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
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli(error) => write!(f, "{error}"),
            Self::InvalidCommand(message) => write!(f, "invalid command: {message}"),
        }
    }
}

impl Error for AppError {}

impl From<crate::cli::CliError> for AppError {
    fn from(value: crate::cli::CliError) -> Self {
        Self::Cli(value)
    }
}
