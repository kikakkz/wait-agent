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
    WorkspaceInternal(WorkspaceCommand),
    UiSidebar(UiPaneCommand),
    UiFooter(UiPaneCommand),
    LayoutReconcile(LayoutReconcileCommand),
    Daemon(DaemonCommand),
    Attach(AttachCommand),
    List(ListCommand),
    Detach(DetachCommand),
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

#[derive(Debug, Clone, Default)]
pub struct DaemonCommand {
    pub node_id: Option<String>,
    pub connect: Option<String>,
    pub workspace_dir: Option<String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub pixel_width: Option<u16>,
    pub pixel_height: Option<u16>,
}

#[derive(Debug, Clone, Default)]
pub struct AttachCommand {
    pub workspace_dir: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ListCommand {}

#[derive(Debug, Clone, Default)]
pub struct DetachCommand {
    pub workspace_dir: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UiPaneCommand {
    pub socket_name: String,
    pub session_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct LayoutReconcileCommand {
    pub socket_name: String,
    pub session_name: String,
    pub workspace_dir: String,
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
            "__workspace-internal" => {
                args.remove(0);
                Command::WorkspaceInternal(parse_workspace(args)?)
            }
            "__ui-sidebar" => {
                args.remove(0);
                Command::UiSidebar(parse_ui_pane(args)?)
            }
            "__ui-footer" => {
                args.remove(0);
                Command::UiFooter(parse_ui_pane(args)?)
            }
            "__layout-reconcile" => {
                args.remove(0);
                Command::LayoutReconcile(parse_layout_reconcile(args)?)
            }
            "daemon" => {
                args.remove(0);
                Command::Daemon(parse_daemon(args)?)
            }
            "attach" => {
                args.remove(0);
                Command::Attach(parse_attach(args)?)
            }
            "ls" => {
                args.remove(0);
                Command::List(parse_list(args)?)
            }
            "detach" => {
                args.remove(0);
                Command::Detach(parse_detach(args)?)
            }
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

fn parse_daemon(args: Vec<String>) -> Result<DaemonCommand, CliError> {
    let mut iter = args.into_iter();
    let mut command = DaemonCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--node-id" => command.node_id = Some(expect_value("--node-id", &mut iter)?),
            "--connect" => command.connect = Some(expect_value("--connect", &mut iter)?),
            "--workspace-dir" => {
                command.workspace_dir = Some(expect_value("--workspace-dir", &mut iter)?)
            }
            "--rows" => {
                command.rows = Some(parse_u16("--rows", expect_value("--rows", &mut iter)?)?)
            }
            "--cols" => {
                command.cols = Some(parse_u16("--cols", expect_value("--cols", &mut iter)?)?)
            }
            "--pixel-width" => {
                command.pixel_width = Some(parse_u16(
                    "--pixel-width",
                    expect_value("--pixel-width", &mut iter)?,
                )?)
            }
            "--pixel-height" => {
                command.pixel_height = Some(parse_u16(
                    "--pixel-height",
                    expect_value("--pixel-height", &mut iter)?,
                )?)
            }
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_attach(args: Vec<String>) -> Result<AttachCommand, CliError> {
    let mut iter = args.into_iter();
    let mut command = AttachCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace-dir" => {
                command.workspace_dir = Some(expect_value("--workspace-dir", &mut iter)?)
            }
            "--help" | "-h" => return Ok(command),
            _ if arg.starts_with("--") => return Err(CliError::UnexpectedArgument(arg)),
            _ if command.target.is_none() => command.target = Some(arg),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_list(args: Vec<String>) -> Result<ListCommand, CliError> {
    let mut iter = args.into_iter();
    let command = ListCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_detach(args: Vec<String>) -> Result<DetachCommand, CliError> {
    let mut iter = args.into_iter();
    let mut command = DetachCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace-dir" => {
                command.workspace_dir = Some(expect_value("--workspace-dir", &mut iter)?)
            }
            "--help" | "-h" => return Ok(command),
            _ if arg.starts_with("--") => return Err(CliError::UnexpectedArgument(arg)),
            _ if command.target.is_none() => command.target = Some(arg),
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(command)
}

fn parse_ui_pane(args: Vec<String>) -> Result<UiPaneCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut session_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--session-name" => session_name = Some(expect_value("--session-name", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(UiPaneCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        session_name: session_name
            .ok_or_else(|| CliError::MissingValue("--session-name".to_string()))?,
    })
}

fn parse_layout_reconcile(args: Vec<String>) -> Result<LayoutReconcileCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut session_name = None;
    let mut workspace_dir = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--session-name" => session_name = Some(expect_value("--session-name", &mut iter)?),
            "--workspace-dir" => workspace_dir = Some(expect_value("--workspace-dir", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(LayoutReconcileCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        session_name: session_name
            .ok_or_else(|| CliError::MissingValue("--session-name".to_string()))?,
        workspace_dir: workspace_dir
            .ok_or_else(|| CliError::MissingValue("--workspace-dir".to_string()))?,
    })
}

fn expect_value<I>(flag: &str, iter: &mut I) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    iter.next()
        .ok_or_else(|| CliError::MissingValue(flag.to_string()))
}

fn parse_u16(flag: &str, value: String) -> Result<u16, CliError> {
    value
        .parse::<u16>()
        .map_err(|_| CliError::InvalidValue(flag.to_string(), value))
}

fn help_text() -> String {
    [
        "WaitAgent",
        "",
        "Usage:",
        "  waitagent [--node-id <id>] [--connect <addr>]",
        "  waitagent attach [<target>]",
        "  waitagent ls",
        "  waitagent detach [<target>]",
        "  waitagent run [--node-id <id>] [--connect <addr>] -- <agent-command...>",
        "  waitagent daemon",
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

    #[test]
    fn parses_attach_command() {
        match parse(&["waitagent", "attach"]) {
            Command::Attach(command) => {
                assert!(command.workspace_dir.is_none());
                assert!(command.target.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_attach_command_with_tmux_target() {
        match parse(&["waitagent", "attach", "wa-1:waitagent-1"]) {
            Command::Attach(command) => {
                assert_eq!(command.target.as_deref(), Some("wa-1:waitagent-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_command() {
        match parse(&["waitagent", "ls"]) {
            Command::List(_) => {}
            other => panic!("unexpected command: {other:?}"),
        }
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
    fn parses_daemon_command_with_hidden_size_flags() {
        match parse(&[
            "waitagent",
            "daemon",
            "--workspace-dir",
            "/tmp/demo",
            "--rows",
            "50",
            "--cols",
            "120",
        ]) {
            Command::Daemon(command) => {
                assert_eq!(command.workspace_dir.as_deref(), Some("/tmp/demo"));
                assert_eq!(command.rows, Some(50));
                assert_eq!(command.cols, Some(120));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_workspace_internal_command() {
        match parse(&["waitagent", "__workspace-internal", "--node-id", "devbox-1"]) {
            Command::WorkspaceInternal(command) => {
                assert_eq!(command.node_id.as_deref(), Some("devbox-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_sidebar_pane_command() {
        match parse(&[
            "waitagent",
            "__ui-sidebar",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
        ]) {
            Command::UiSidebar(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_layout_reconcile_command() {
        match parse(&[
            "waitagent",
            "__layout-reconcile",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
            "--workspace-dir",
            "/tmp/workspace",
        ]) {
            Command::LayoutReconcile(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
                assert_eq!(command.workspace_dir, "/tmp/workspace");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_detach_command_with_tmux_target() {
        match parse(&["waitagent", "detach", "waitagent-1"]) {
            Command::Detach(command) => {
                assert_eq!(command.target.as_deref(), Some("waitagent-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
