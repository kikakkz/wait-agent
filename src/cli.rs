use std::error::Error;
use std::ffi::OsString;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Cli {
    pub command: Command,
}

#[derive(Debug, Clone)]
pub enum Command {
    Workspace(WorkspaceCommand),
    Run(RunCommand),
    Server(ServerCommand),
    Help(String),
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceCommand {
    pub node_id: Option<String>,
    pub connect: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunCommand {
    pub node_id: Option<String>,
    pub connect: Option<String>,
    pub program: String,
    pub args: Vec<String>,
}

impl RunCommand {
    pub fn command_line(&self) -> String {
        let mut parts = Vec::with_capacity(self.args.len() + 1);
        parts.push(self.program.clone());
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

#[derive(Debug, Clone, Default)]
pub struct ServerCommand {
    pub listen: Option<String>,
    pub node_id: Option<String>,
}

impl Cli {
    pub fn parse<I>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut args = args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        if args.is_empty() {
            return Ok(Self {
                command: Command::Help(help_text()),
            });
        }

        args.remove(0);

        if args.is_empty() {
            return Ok(Self {
                command: Command::Workspace(WorkspaceCommand::default()),
            });
        }

        let command = match args[0].as_str() {
            "run" => {
                args.remove(0);
                Command::Run(parse_run(args)?)
            }
            "server" => {
                args.remove(0);
                Command::Server(parse_server(args)?)
            }
            "help" => Command::Help(help_text()),
            "--help" | "-h" => Command::Help(help_text()),
            other => {
                if other.starts_with("--") {
                    Command::Workspace(parse_workspace(args)?)
                } else {
                    return Err(CliError::UnknownSubcommand(other.to_string()));
                }
            }
        };

        Ok(Self { command })
    }
}

fn parse_workspace(args: Vec<String>) -> Result<WorkspaceCommand, CliError> {
    let mut iter = args.into_iter();
    let mut command = WorkspaceCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--node-id" => command.node_id = Some(expect_value("--node-id", &mut iter)?),
            "--connect" => command.connect = Some(expect_value("--connect", &mut iter)?),
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_run(args: Vec<String>) -> Result<RunCommand, CliError> {
    let mut iter = args.into_iter().peekable();
    let mut node_id = None;
    let mut connect = None;
    let mut command = Vec::new();
    let mut passthrough = false;

    while let Some(arg) = iter.next() {
        if passthrough {
            command.push(arg);
            continue;
        }

        match arg.as_str() {
            "--" => {
                passthrough = true;
            }
            "--node-id" => node_id = Some(expect_value("--node-id", &mut iter)?),
            "--connect" => connect = Some(expect_value("--connect", &mut iter)?),
            "--help" | "-h" => {
                return Ok(RunCommand {
                    node_id,
                    connect,
                    program: String::new(),
                    args: vec!["--help".to_string()],
                });
            }
            _ if arg.starts_with("--") => {
                return Err(CliError::UnexpectedArgument(arg));
            }
            _ => {
                command.push(arg);
                command.extend(iter);
                break;
            }
        }
    }

    let mut parts = command.into_iter();
    let program = parts.next().unwrap_or_default();
    let args = parts.collect();

    Ok(RunCommand {
        node_id,
        connect,
        program,
        args,
    })
}

fn parse_server(args: Vec<String>) -> Result<ServerCommand, CliError> {
    let mut iter = args.into_iter();
    let mut command = ServerCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--listen" => command.listen = Some(expect_value("--listen", &mut iter)?),
            "--node-id" => command.node_id = Some(expect_value("--node-id", &mut iter)?),
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}
fn expect_value<I>(flag: &str, iter: &mut I) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    iter.next()
        .ok_or_else(|| CliError::MissingValue(flag.to_string()))
}

fn help_text() -> String {
    [
        "WaitAgent",
        "",
        "Usage:",
        "  waitagent [--node-id <id>] [--connect <addr>]",
        "  waitagent run [--node-id <id>] [--connect <addr>] -- <agent-command...>",
        "  waitagent server [--listen <addr>] [--node-id <id>]",
        "",
        "Environment:",
        "  WAITAGENT_NODE_ID",
        "  WAITAGENT_ACCESS_POINT",
        "  WAITAGENT_LISTEN_ADDR",
    ]
    .join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    UnknownSubcommand(String),
    UnexpectedArgument(String),
    MissingValue(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSubcommand(command) => write!(f, "unknown subcommand: {command}"),
            Self::UnexpectedArgument(argument) => write!(f, "unexpected argument: {argument}"),
            Self::MissingValue(flag) => write!(f, "missing value for {flag}"),
        }
    }
}

impl Error for CliError {}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};

    fn parse(args: &[&str]) -> Command {
        let argv = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
        Cli::parse(argv).expect("cli parse should succeed").command
    }

    #[test]
    fn defaults_to_workspace_command_without_subcommand() {
        match parse(&["waitagent"]) {
            Command::Workspace(command) => {
                assert!(command.node_id.is_none());
                assert!(command.connect.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_workspace_command_with_top_level_flags() {
        match parse(&[
            "waitagent",
            "--connect",
            "127.0.0.1:7474",
            "--node-id",
            "devbox-1",
        ]) {
            Command::Workspace(command) => {
                assert_eq!(command.connect.as_deref(), Some("127.0.0.1:7474"));
                assert_eq!(command.node_id.as_deref(), Some("devbox-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_run_command_with_passthrough() {
        match parse(&[
            "waitagent",
            "run",
            "--node-id",
            "devbox-1",
            "--",
            "codex",
            "fix",
        ]) {
            Command::Run(run) => {
                assert_eq!(run.node_id.as_deref(), Some("devbox-1"));
                assert_eq!(run.program, "codex");
                assert_eq!(run.args, vec!["fix"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_run_command_with_connect() {
        match parse(&[
            "waitagent",
            "run",
            "--connect",
            "ws://127.0.0.1:7474",
            "--",
            "claude",
        ]) {
            Command::Run(run) => {
                assert_eq!(run.connect.as_deref(), Some("ws://127.0.0.1:7474"));
                assert_eq!(run.program, "claude");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
