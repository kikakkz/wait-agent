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
    UiSidebar(UiPaneCommand),
    UiFooter(UiPaneCommand),
    ActivateTarget(ActivateTargetCommand),
    NewTarget(NewTargetCommand),
    MainPaneDied(MainPaneDiedCommand),
    FooterMenu(FooterMenuCommand),
    ToggleFullscreen(ToggleFullscreenCommand),
    CloseSession(CloseSessionCommand),
    LayoutReconcile(LayoutReconcileCommand),
    ChromeRefresh(LayoutReconcileCommand),
    ChromeRefreshAll,
    Attach(AttachCommand),
    List(ListCommand),
    Detach(DetachCommand),
    Help(String),
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceCommand {}

#[derive(Debug, Clone, Default)]
pub struct AttachCommand {
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ListCommand {}

#[derive(Debug, Clone, Default)]
pub struct DetachCommand {
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

#[derive(Debug, Clone, Default)]
pub struct FooterMenuCommand {
    pub socket_name: String,
    pub session_name: String,
    pub client_tty: String,
}

#[derive(Debug, Clone, Default)]
pub struct ToggleFullscreenCommand {
    pub socket_name: String,
    pub session_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct ActivateTargetCommand {
    pub current_socket_name: String,
    pub current_session_name: String,
    pub target: String,
}

#[derive(Debug, Clone, Default)]
pub struct NewTargetCommand {
    pub current_socket_name: String,
    pub current_session_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct MainPaneDiedCommand {
    pub socket_name: String,
    pub session_name: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct CloseSessionCommand {
    pub socket_name: String,
    pub session_name: String,
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
            "__ui-sidebar" => {
                args.remove(0);
                Command::UiSidebar(parse_ui_pane(args)?)
            }
            "__ui-footer" => {
                args.remove(0);
                Command::UiFooter(parse_ui_pane(args)?)
            }
            "__activate-target" => {
                args.remove(0);
                Command::ActivateTarget(parse_activate_target(args)?)
            }
            "__new-target" => {
                args.remove(0);
                Command::NewTarget(parse_new_target(args)?)
            }
            "__main-pane-died" => {
                args.remove(0);
                Command::MainPaneDied(parse_main_pane_died(args)?)
            }
            "__footer-menu" => {
                args.remove(0);
                Command::FooterMenu(parse_footer_menu(args)?)
            }
            "__toggle-fullscreen" => {
                args.remove(0);
                Command::ToggleFullscreen(parse_toggle_fullscreen(args)?)
            }
            "__close-session" => {
                args.remove(0);
                Command::CloseSession(parse_close_session(args)?)
            }
            "__layout-reconcile" => {
                args.remove(0);
                Command::LayoutReconcile(parse_layout_reconcile(args)?)
            }
            "__chrome-refresh" => {
                args.remove(0);
                Command::ChromeRefresh(parse_layout_reconcile(args)?)
            }
            "__chrome-refresh-all" => {
                args.remove(0);
                parse_no_args(args)?;
                Command::ChromeRefreshAll
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
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(WorkspaceCommand::default())
}

fn parse_attach(args: Vec<String>) -> Result<AttachCommand, CliError> {
    let mut iter = args.into_iter();
    let mut command = AttachCommand::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
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

fn parse_footer_menu(args: Vec<String>) -> Result<FooterMenuCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut session_name = None;
    let mut client_tty = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--session-name" => session_name = Some(expect_value("--session-name", &mut iter)?),
            "--client-tty" => client_tty = Some(expect_value("--client-tty", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(FooterMenuCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        session_name: session_name
            .ok_or_else(|| CliError::MissingValue("--session-name".to_string()))?,
        client_tty: client_tty.ok_or_else(|| CliError::MissingValue("--client-tty".to_string()))?,
    })
}

fn parse_toggle_fullscreen(args: Vec<String>) -> Result<ToggleFullscreenCommand, CliError> {
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

    Ok(ToggleFullscreenCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        session_name: session_name
            .ok_or_else(|| CliError::MissingValue("--session-name".to_string()))?,
    })
}

fn parse_activate_target(args: Vec<String>) -> Result<ActivateTargetCommand, CliError> {
    let mut iter = args.into_iter();
    let mut current_socket_name = None;
    let mut current_session_name = None;
    let mut target = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--current-socket-name" => {
                current_socket_name = Some(expect_value("--current-socket-name", &mut iter)?)
            }
            "--current-session-name" => {
                current_session_name = Some(expect_value("--current-session-name", &mut iter)?)
            }
            "--target" => target = Some(expect_value("--target", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(ActivateTargetCommand {
        current_socket_name: current_socket_name
            .ok_or_else(|| CliError::MissingValue("--current-socket-name".to_string()))?,
        current_session_name: current_session_name
            .ok_or_else(|| CliError::MissingValue("--current-session-name".to_string()))?,
        target: target.ok_or_else(|| CliError::MissingValue("--target".to_string()))?,
    })
}

fn parse_new_target(args: Vec<String>) -> Result<NewTargetCommand, CliError> {
    let mut iter = args.into_iter();
    let mut current_socket_name = None;
    let mut current_session_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--current-socket-name" => {
                current_socket_name = Some(expect_value("--current-socket-name", &mut iter)?)
            }
            "--current-session-name" => {
                current_session_name = Some(expect_value("--current-session-name", &mut iter)?)
            }
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(NewTargetCommand {
        current_socket_name: current_socket_name
            .ok_or_else(|| CliError::MissingValue("--current-socket-name".to_string()))?,
        current_session_name: current_session_name
            .ok_or_else(|| CliError::MissingValue("--current-session-name".to_string()))?,
    })
}

fn parse_main_pane_died(args: Vec<String>) -> Result<MainPaneDiedCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut session_name = None;
    let mut pane_id = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--session-name" => session_name = Some(expect_value("--session-name", &mut iter)?),
            "--pane-id" => pane_id = Some(expect_value("--pane-id", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(MainPaneDiedCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        session_name: session_name
            .ok_or_else(|| CliError::MissingValue("--session-name".to_string()))?,
        pane_id: pane_id.ok_or_else(|| CliError::MissingValue("--pane-id".to_string()))?,
    })
}

fn parse_close_session(args: Vec<String>) -> Result<CloseSessionCommand, CliError> {
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

    Ok(CloseSessionCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        session_name: session_name
            .ok_or_else(|| CliError::MissingValue("--session-name".to_string()))?,
    })
}

fn expect_value<I>(flag: &str, iter: &mut I) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    iter.next()
        .ok_or_else(|| CliError::MissingValue(flag.to_string()))
}

fn parse_no_args(args: Vec<String>) -> Result<(), CliError> {
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
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
        "  waitagent",
        "  waitagent attach [<target>]",
        "  waitagent ls",
        "  waitagent detach [<target>]",
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
            Command::Workspace(_) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_removed_top_level_remote_flags() {
        let argv = ["waitagent", "--connect", "127.0.0.1:7474"]
            .iter()
            .map(|arg| (*arg).into())
            .collect::<Vec<_>>();
        let error = Cli::parse(argv).expect_err("legacy remote flags should no longer parse");

        assert_eq!(error.to_string(), "unexpected argument: --connect");
    }

    #[test]
    fn parses_attach_command() {
        match parse(&["waitagent", "attach"]) {
            Command::Attach(command) => {
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
    fn parses_hidden_chrome_refresh_command() {
        match parse(&[
            "waitagent",
            "__chrome-refresh",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
            "--workspace-dir",
            "/tmp/workspace",
        ]) {
            Command::ChromeRefresh(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
                assert_eq!(command.workspace_dir, "/tmp/workspace");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_chrome_refresh_all_command() {
        assert!(matches!(
            parse(&["waitagent", "__chrome-refresh-all"]),
            Command::ChromeRefreshAll
        ));
    }

    #[test]
    fn parses_hidden_close_session_command() {
        match parse(&[
            "waitagent",
            "__close-session",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
        ]) {
            Command::CloseSession(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_main_pane_died_command() {
        match parse(&[
            "waitagent",
            "__main-pane-died",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
            "--pane-id",
            "%9",
        ]) {
            Command::MainPaneDied(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
                assert_eq!(command.pane_id, "%9");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_footer_menu_command() {
        match parse(&[
            "waitagent",
            "__footer-menu",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
            "--client-tty",
            "/dev/pts/7",
        ]) {
            Command::FooterMenu(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
                assert_eq!(command.client_tty, "/dev/pts/7");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_toggle_fullscreen_command() {
        match parse(&[
            "waitagent",
            "__toggle-fullscreen",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
        ]) {
            Command::ToggleFullscreen(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
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
