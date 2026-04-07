use std::error::Error;
use std::ffi::OsString;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Cli {
    pub command: Command,
}

#[derive(Debug, Clone)]
pub enum Command {
    Run(RunCommand),
    Attach(AttachCommand),
    Server(ServerCommand),
    Client(ClientCommand),
    Help(String),
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
pub struct AttachCommand {
    pub server: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerCommand {
    pub listen: Option<String>,
    pub node_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ClientCommand {
    pub connect: Option<String>,
    pub node_id: Option<String>,
    pub proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
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
                command: Command::Help(help_text()),
            });
        }

        let subcommand = args.remove(0);

        let command = match subcommand.as_str() {
            "run" => Command::Run(parse_run(args)?),
            "attach" => Command::Attach(parse_attach(args)?),
            "server" => Command::Server(parse_server(args)?),
            "client" => Command::Client(parse_client(args)?),
            "help" | "--help" | "-h" => Command::Help(help_text()),
            other => {
                return Err(CliError::UnknownSubcommand(other.to_string()));
            }
        };

        Ok(Self { command })
    }
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

fn parse_attach(args: Vec<String>) -> Result<AttachCommand, CliError> {
    let mut iter = args.into_iter();
    let mut command = AttachCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => command.server = Some(expect_value("--server", &mut iter)?),
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
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

fn parse_client(args: Vec<String>) -> Result<ClientCommand, CliError> {
    let mut iter = args.into_iter();
    let mut command = ClientCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--connect" => command.connect = Some(expect_value("--connect", &mut iter)?),
            "--node-id" => command.node_id = Some(expect_value("--node-id", &mut iter)?),
            "--proxy" => command.proxy = Some(expect_value("--proxy", &mut iter)?),
            "--all-proxy" => command.all_proxy = Some(expect_value("--all-proxy", &mut iter)?),
            "--http-proxy" => command.http_proxy = Some(expect_value("--http-proxy", &mut iter)?),
            "--https-proxy" => {
                command.https_proxy = Some(expect_value("--https-proxy", &mut iter)?)
            }
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
        "  waitagent run [--node-id <id>] [--connect <addr>] -- <agent-command...>",
        "  waitagent attach [--server <addr>]",
        "  waitagent server [--listen <addr>] [--node-id <id>]",
        "  waitagent client [--connect <addr>] [--node-id <id>] [--proxy <url>]",
        "",
        "Environment:",
        "  WAITAGENT_NODE_ID",
        "  WAITAGENT_ACCESS_POINT",
        "  WAITAGENT_LISTEN_ADDR",
        "  WAITAGENT_PROXY",
        "  WAITAGENT_ALL_PROXY",
        "  WAITAGENT_HTTP_PROXY",
        "  WAITAGENT_HTTPS_PROXY",
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
    fn parses_client_proxy_flags() {
        match parse(&[
            "waitagent",
            "client",
            "--connect",
            "ws://127.0.0.1:7474",
            "--proxy",
            "socks5://127.0.0.1:7897",
        ]) {
            Command::Client(client) => {
                assert_eq!(client.connect.as_deref(), Some("ws://127.0.0.1:7474"));
                assert_eq!(client.proxy.as_deref(), Some("socks5://127.0.0.1:7897"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
