use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

pub const DEFAULT_REMOTE_NODE_PORT: u16 = 7474;

/// Reads the default port from the `WAITAGENT_DEFAULT_PORT` environment variable,
/// falling back to [`DEFAULT_REMOTE_NODE_PORT`] if the variable is unset or invalid.
/// This lets tests run on a non-default port without modifying source constants.
pub fn default_remote_node_port() -> u16 {
    std::env::var("WAITAGENT_DEFAULT_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_REMOTE_NODE_PORT)
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub network: RemoteNetworkConfig,
    pub network_explicit: bool,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNetworkConfig {
    pub port: u16,
    pub connect: Option<String>,
    pub node_id: Option<String>,
    pub public_endpoint: Option<String>,
}

impl Default for RemoteNetworkConfig {
    fn default() -> Self {
        Self {
            port: default_remote_node_port(),
            connect: None,
            node_id: None,
            public_endpoint: None,
        }
    }
}

impl RemoteNetworkConfig {
    pub fn listener_addr(&self) -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], self.port))
    }

    pub fn advertised_listener_addr(&self) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V4(discover_advertised_lan_ipv4().unwrap_or(Ipv4Addr::LOCALHOST)),
            self.port,
        )
    }

    pub fn advertised_listener_label(&self) -> String {
        self.advertised_listener_addr().to_string()
    }

    pub fn advertised_public_endpoint_label(&self) -> String {
        self.public_endpoint
            .clone()
            .unwrap_or_else(|| self.advertised_listener_label())
    }

    pub fn advertised_host_id(&self) -> String {
        self.public_endpoint
            .as_ref()
            .and_then(|endpoint| endpoint.rsplit_once(':').map(|(host, _)| host.to_string()))
            .unwrap_or_else(|| self.advertised_listener_addr().ip().to_string())
    }

    pub fn advertised_node_id(&self) -> String {
        self.node_id
            .clone()
            .unwrap_or_else(|| format!("{}#{}", self.advertised_host_id(), self.port))
    }

    pub fn connect_endpoint_uri(&self) -> Option<String> {
        self.connect.as_ref().map(|connect| {
            if connect.contains("://") {
                connect.clone()
            } else {
                format!("http://{connect}")
            }
        })
    }

    pub fn to_cli_args(&self) -> Vec<String> {
        let mut args = vec!["--port".to_string(), self.port.to_string()];
        if let Some(connect) = &self.connect {
            args.push("--connect".to_string());
            args.push(connect.clone());
        }
        if let Some(node_id) = &self.node_id {
            args.push("--node-id".to_string());
            args.push(node_id.clone());
        }
        if let Some(public_endpoint) = &self.public_endpoint {
            args.push("--public".to_string());
            args.push(public_endpoint.clone());
        }
        args
    }
}

fn discover_advertised_lan_ipv4() -> Option<Ipv4Addr> {
    const PROBE_TARGETS: [([u8; 4], u16); 4] = [
        ([192, 168, 0, 1], 9),
        ([10, 0, 0, 1], 9),
        ([172, 16, 0, 1], 9),
        ([8, 8, 8, 8], 53),
    ];

    for (ip, port) in PROBE_TARGETS {
        let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).ok()?;
        if socket.connect(SocketAddr::from((ip, port))).is_err() {
            continue;
        }
        let Ok(SocketAddr::V4(local_addr)) = socket.local_addr() else {
            continue;
        };
        let ip = *local_addr.ip();
        if !ip.is_loopback() && !ip.is_unspecified() {
            return Some(ip);
        }
    }

    None
}

pub fn prepend_global_network_args(
    mut command_args: Vec<String>,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    let mut args = network.to_cli_args();
    args.append(&mut command_args);
    args
}

#[derive(Debug, Clone)]
pub enum Command {
    Workspace,
    ShowErrorLog,
    Attach(AttachCommand),
    List,
    Cleanup,
    Detach(DetachCommand),
    Stop(StopCommand),
    RatatuiListSessions(RatatuiListSessionsCommand),
    RatatuiNodeServer(RatatuiNodeServerCommand),
    RatatuiClient(RatatuiClientCommand),
    Help(String),
    Version,
}

#[derive(Debug, Clone, Default)]
pub struct AttachCommand {
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DetachCommand {
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StopCommand {
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RatatuiNodeServerCommand;

#[derive(Debug, Clone, Default)]
pub struct RatatuiClientCommand;

#[derive(Debug, Clone, Default)]
pub struct RatatuiListSessionsCommand {
    pub target: Option<String>,
}

/// Used directly by ratatui runtime code; not parsed from the CLI.
#[derive(Debug, Clone, Default)]
pub struct RemoteRuntimeOwnerCommand {
    pub ready_socket: Option<String>,
}

/// Used directly by ratatui runtime code; not parsed from the CLI.
#[derive(Debug, Clone, Default)]
pub struct ConnectRemoteHostPaneCommand {
    pub current_socket_name: String,
    pub current_session_name: String,
}

/// Used directly by ratatui runtime code; not parsed from the CLI.
#[derive(Debug, Clone, Default)]
pub struct ConnectRemoteHostCommand {
    pub profile: Option<String>,
    pub host: Option<String>,
    pub ssh_user: Option<String>,
    pub auth: Option<String>,
    pub key_path: Option<String>,
    pub ssh_password_secret_id: Option<String>,
    pub sudo_password_secret_id: Option<String>,
    pub ssh_password_stdin: bool,
    pub sudo_password_stdin: bool,
    pub remote_port: Option<String>,
    pub save_profile: Option<String>,
    pub replace_profile: Option<String>,
    pub use_install_proxy: Option<bool>,
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
                network: RemoteNetworkConfig::default(),
                network_explicit: false,
                command: Command::Help(help_text()),
            });
        }

        args.remove(0);
        let (network, network_explicit) = parse_global_network_config(&mut args)?;

        if args.is_empty() {
            return Ok(Self {
                network,
                network_explicit,
                command: Command::Workspace,
            });
        }

        let command = match args[0].as_str() {
            "__error-log" => {
                args.remove(0);
                parse_no_args(args)?;
                Command::ShowErrorLog
            }
            "attach" => {
                args.remove(0);
                Command::Attach(parse_attach(args)?)
            }
            "ls" => {
                args.remove(0);
                parse_no_args(args)?;
                Command::List
            }
            "cleanup" => {
                args.remove(0);
                parse_no_args(args)?;
                Command::Cleanup
            }
            "detach" => {
                args.remove(0);
                Command::Detach(parse_detach(args)?)
            }
            "stop" => {
                args.remove(0);
                Command::Stop(parse_stop(args)?)
            }
            "list-sessions" => {
                args.remove(0);
                Command::RatatuiListSessions(parse_ratatui_list_sessions(args)?)
            }
            "__ratatui-node-server" => {
                args.remove(0);
                Command::RatatuiNodeServer(parse_ratatui_node_server(args)?)
            }
            "__ratatui-client" => {
                args.remove(0);
                Command::RatatuiClient(parse_ratatui_client(args)?)
            }
            "version" => Command::Version,
            "help" => Command::Help(help_text()),
            "--version" | "-V" => Command::Version,
            "--help" | "-h" => Command::Help(help_text()),
            other => {
                if other.starts_with("--") {
                    parse_no_args(args)?;
                    Command::Workspace
                } else {
                    return Err(CliError::UnknownSubcommand(other.to_string()));
                }
            }
        };

        Ok(Self {
            network,
            network_explicit,
            command,
        })
    }
}

fn parse_global_network_config(
    args: &mut Vec<String>,
) -> Result<(RemoteNetworkConfig, bool), CliError> {
    let mut network = RemoteNetworkConfig::default();
    let mut explicit = false;

    loop {
        let Some(flag) = args.first().cloned() else {
            break;
        };
        match flag.as_str() {
            "--port" => {
                explicit = true;
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--port".to_string()))?;
                args.remove(0);
                network.port = value
                    .parse::<u16>()
                    .map_err(|_| CliError::InvalidValue("--port".to_string(), value.clone()))?;
            }
            "--connect" => {
                explicit = true;
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--connect".to_string()))?;
                args.remove(0);
                if value.trim().is_empty() {
                    return Err(CliError::InvalidValue("--connect".to_string(), value));
                }
                network.connect = Some(value);
            }
            "--node-id" => {
                explicit = true;
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--node-id".to_string()))?;
                args.remove(0);
                if value.trim().is_empty() {
                    return Err(CliError::InvalidValue("--node-id".to_string(), value));
                }
                network.node_id = Some(value);
            }
            "--public" => {
                explicit = true;
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--public".to_string()))?;
                args.remove(0);
                if value.trim().is_empty() {
                    return Err(CliError::InvalidValue("--public".to_string(), value));
                }
                network.public_endpoint = Some(value);
            }
            _ => break,
        }
    }

    Ok((network, explicit))
}

fn parse_ratatui_node_server(args: Vec<String>) -> Result<RatatuiNodeServerCommand, CliError> {
    if let Some(arg) = args.into_iter().next() {
        if arg == "--help" || arg == "-h" {
            return Ok(RatatuiNodeServerCommand);
        }
        return Err(CliError::UnexpectedArgument(arg));
    }

    Ok(RatatuiNodeServerCommand)
}

fn parse_ratatui_client(args: Vec<String>) -> Result<RatatuiClientCommand, CliError> {
    if let Some(arg) = args.into_iter().next() {
        if arg == "--help" || arg == "-h" {
            return Ok(RatatuiClientCommand);
        }
        return Err(CliError::UnexpectedArgument(arg));
    }

    Ok(RatatuiClientCommand)
}

fn parse_ratatui_list_sessions(args: Vec<String>) -> Result<RatatuiListSessionsCommand, CliError> {
    let iter = args.into_iter();
    let mut command = RatatuiListSessionsCommand::default();

    for arg in iter {
        match arg.as_str() {
            "--help" | "-h" => return Ok(command),
            _ if arg.starts_with("--") => return Err(CliError::UnexpectedArgument(arg)),
            _ if command.target.is_none() => command.target = Some(arg),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_attach(args: Vec<String>) -> Result<AttachCommand, CliError> {
    let iter = args.into_iter();
    let mut command = AttachCommand::default();

    for arg in iter {
        match arg.as_str() {
            "--help" | "-h" => return Ok(command),
            _ if arg.starts_with("--") => return Err(CliError::UnexpectedArgument(arg)),
            _ if command.target.is_none() => command.target = Some(arg),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_detach(args: Vec<String>) -> Result<DetachCommand, CliError> {
    let iter = args.into_iter();
    let mut command = DetachCommand::default();

    for arg in iter {
        match arg.as_str() {
            "--help" | "-h" => return Ok(command),
            _ if arg.starts_with("--") => return Err(CliError::UnexpectedArgument(arg)),
            _ if command.target.is_none() => command.target = Some(arg),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_stop(args: Vec<String>) -> Result<StopCommand, CliError> {
    let iter = args.into_iter();
    let mut command = StopCommand::default();

    for arg in iter {
        match arg.as_str() {
            "--help" | "-h" => return Ok(command),
            _ if arg.starts_with("--") => return Err(CliError::UnexpectedArgument(arg)),
            _ if command.target.is_none() => command.target = Some(arg),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_no_args(args: Vec<String>) -> Result<(), CliError> {
    let iter = args.into_iter();

    for arg in iter {
        match arg.as_str() {
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(())
}

fn help_text() -> String {
    [
        "WaitAgent",
        "",
        "Usage:",
        "  waitagent [--port <port>] [--connect <host:port>] [--public <host:port>]",
        "  waitagent [--port <port>] [--connect <host:port>] [--public <host:port>] attach [<index>]",
        "  waitagent ls",
        "  waitagent list-sessions [<index>]",
        "  waitagent cleanup",
        "  waitagent detach [<index>]",
        "  waitagent stop [<index>]",
        "  waitagent version",
    ]
    .join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    UnknownSubcommand(String),
    UnexpectedArgument(String),
    MissingValue(String),
    InvalidValue(String, String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSubcommand(command) => write!(f, "unknown subcommand: {command}"),
            Self::UnexpectedArgument(argument) => write!(f, "unexpected argument: {argument}"),
            Self::MissingValue(flag) => write!(f, "missing value for {flag}"),
            Self::InvalidValue(flag, value) => {
                write!(f, "invalid value for {flag}: {value}")
            }
        }
    }
}

impl Error for CliError {}

#[cfg(test)]
mod tests {
    use super::{default_remote_node_port, Cli, Command};

    fn parse(args: &[&str]) -> Cli {
        let argv = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
        Cli::parse(argv).expect("cli parse should succeed")
    }

    #[test]
    fn defaults_to_workspace_command_without_subcommand() {
        let cli = parse(&["waitagent"]);
        assert!(matches!(cli.command, Command::Workspace));
        assert_eq!(cli.network.port, default_remote_node_port());
        assert!(cli.network.connect.is_none());
    }

    #[test]
    fn rejects_removed_top_level_remote_flags() {
        let argv = ["waitagent", "--server", "127.0.0.1:7474"]
            .iter()
            .map(|arg| (*arg).into())
            .collect::<Vec<_>>();
        let error = Cli::parse(argv).expect_err("legacy remote flags should no longer parse");

        assert_eq!(error.to_string(), "unexpected argument: --server");
    }

    #[test]
    fn parses_attach_command() {
        match parse(&["waitagent", "attach"]).command {
            Command::Attach(command) => {
                assert!(command.target.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_global_network_flags_before_command() {
        let cli = parse(&[
            "waitagent",
            "--port",
            "8484",
            "--connect",
            "remote.example:7474",
            "--public",
            "nat.example:17474",
            "attach",
            "wa-1:waitagent-1",
        ]);

        assert_eq!(cli.network.port, 8484);
        assert_eq!(cli.network.connect.as_deref(), Some("remote.example:7474"));
        assert_eq!(
            cli.network.public_endpoint.as_deref(),
            Some("nat.example:17474")
        );
        assert_eq!(
            cli.network.connect_endpoint_uri().as_deref(),
            Some("http://remote.example:7474")
        );
        match cli.command {
            Command::Attach(command) => {
                assert_eq!(command.target.as_deref(), Some("wa-1:waitagent-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_global_port_value() {
        let argv = ["waitagent", "--port", "abc"]
            .iter()
            .map(|arg| (*arg).into())
            .collect::<Vec<_>>();
        let error = Cli::parse(argv).expect_err("invalid port should fail");

        assert_eq!(error.to_string(), "invalid value for --port: abc");
    }

    #[test]
    fn parses_attach_command_with_tmux_target() {
        match parse(&["waitagent", "attach", "wa-1:waitagent-1"]).command {
            Command::Attach(command) => {
                assert_eq!(command.target.as_deref(), Some("wa-1:waitagent-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_command() {
        assert!(matches!(parse(&["waitagent", "ls"]).command, Command::List));
    }

    #[test]
    fn parses_cleanup_command() {
        assert!(matches!(
            parse(&["waitagent", "cleanup"]).command,
            Command::Cleanup
        ));
    }

    #[test]
    fn rejects_status_subcommand() {
        let argv = ["waitagent", "status"]
            .iter()
            .map(|arg| (*arg).into())
            .collect::<Vec<_>>();
        let error = Cli::parse(argv).expect_err("status should no longer parse");

        assert_eq!(error.to_string(), "unknown subcommand: status");
    }

    #[test]
    fn rejects_removed_server_subcommand() {
        let argv = ["waitagent", "server"]
            .iter()
            .map(|arg| (*arg).into())
            .collect::<Vec<_>>();
        let error = Cli::parse(argv).expect_err("server subcommand should no longer parse");

        assert_eq!(error.to_string(), "unknown subcommand: server");
    }

    #[test]
    fn parses_stop_command() {
        match parse(&["waitagent", "stop", "waitagent-1"]).command {
            Command::Stop(command) => {
                assert_eq!(command.target.as_deref(), Some("waitagent-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_sessions_command() {
        match parse(&["waitagent", "list-sessions", "wa-1"]).command {
            Command::RatatuiListSessions(command) => {
                assert_eq!(command.target.as_deref(), Some("wa-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_ratatui_node_server_command() {
        assert!(matches!(
            parse(&["waitagent", "__ratatui-node-server"]).command,
            Command::RatatuiNodeServer(_)
        ));
    }

    #[test]
    fn parses_ratatui_client_command() {
        assert!(matches!(
            parse(&["waitagent", "__ratatui-client"]).command,
            Command::RatatuiClient(_)
        ));
    }

    #[test]
    fn parses_show_error_log_command() {
        assert!(matches!(
            parse(&["waitagent", "__error-log"]).command,
            Command::ShowErrorLog
        ));
    }

    #[test]
    fn parses_version_command() {
        assert!(matches!(
            parse(&["waitagent", "version"]).command,
            Command::Version
        ));
    }

    #[test]
    fn parses_help_command() {
        match parse(&["waitagent", "help"]).command {
            Command::Help(text) => {
                assert!(text.contains("WaitAgent"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_detach_command_with_tmux_target() {
        match parse(&["waitagent", "detach", "waitagent-1"]).command {
            Command::Detach(command) => {
                assert_eq!(command.target.as_deref(), Some("waitagent-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
