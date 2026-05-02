use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

pub const DEFAULT_REMOTE_NODE_PORT: u16 = 7474;

#[derive(Debug, Clone)]
pub struct Cli {
    pub network: RemoteNetworkConfig,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNetworkConfig {
    pub port: u16,
    pub connect: Option<String>,
}

impl Default for RemoteNetworkConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_REMOTE_NODE_PORT,
            connect: None,
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

    pub fn advertised_node_id(&self) -> String {
        self.advertised_listener_addr().ip().to_string()
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
    ChromeRefreshSocket(SocketNameCommand),
    UiSidebar(UiPaneCommand),
    UiFooter(UiPaneCommand),
    RemoteMainSlot(RemoteMainSlotCommand),
    RemoteServerConsole(RemoteServerConsoleCommand),
    RemoteAuthorityTargetHost(RemoteAuthorityTargetHostCommand),
    RemoteAuthorityOutputPump(RemoteAuthorityOutputPumpCommand),
    RemoteTargetPublicationServer(RemoteTargetPublicationServerCommand),
    RemoteTargetPublicationAgent(RemoteTargetPublicationAgentCommand),
    RemoteTargetPublicationSender(RemoteTargetPublicationSenderCommand),
    RemoteTargetPublicationOwner(RemoteTargetPublicationOwnerCommand),
    RemoteRuntimeOwner(RemoteRuntimeOwnerCommand),
    SocketLifecycleHook(SocketLifecycleHookCommand),
    RemoteTargetBindPublication(RemoteTargetBindPublicationCommand),
    RemoteTargetUnbindPublication(RemoteTargetUnbindPublicationCommand),
    RemoteTargetReconcilePublications(RemoteTargetReconcilePublicationsCommand),
    ActivateTarget(ActivateTargetCommand),
    NewTarget(NewTargetCommand),
    MainPaneDied(MainPaneDiedCommand),
    FooterMenu(FooterMenuCommand),
    ToggleFullscreen(ToggleFullscreenCommand),
    CloseSession(CloseSessionCommand),
    LayoutReconcile(LayoutReconcileCommand),
    ChromeRefresh(LayoutReconcileCommand),
    ChromeRefreshSignal(UiPaneCommand),
    ChromeRefreshAll,
    Attach(AttachCommand),
    List,
    Detach(DetachCommand),
    Help(String),
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
pub struct UiPaneCommand {
    pub socket_name: String,
    pub session_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct SocketNameCommand {
    pub socket_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct SocketLifecycleHookCommand {
    pub socket_name: String,
    pub hook_name: Option<String>,
    pub session_name: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteMainSlotCommand {
    pub socket_name: String,
    pub session_name: String,
    pub target: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteServerConsoleCommand {
    pub socket_name: String,
    pub console_name: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteAuthorityTargetHostCommand {
    pub socket_name: String,
    pub target_session_name: String,
    pub authority_id: String,
    pub target_id: String,
    pub transport_socket_path: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteAuthorityOutputPumpCommand {
    pub ingest_socket_path: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteTargetPublicationServerCommand {
    pub socket_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteTargetPublicationAgentCommand {
    pub socket_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteTargetPublicationSenderCommand {
    pub socket_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteTargetPublicationOwnerCommand {
    pub socket_name: String,
    pub target_session_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteRuntimeOwnerCommand {
    pub socket_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteTargetBindPublicationCommand {
    pub socket_name: String,
    pub target_session_name: String,
    pub authority_id: String,
    pub transport_session_id: String,
    pub selector: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteTargetUnbindPublicationCommand {
    pub socket_name: String,
    pub target_session_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteTargetReconcilePublicationsCommand {
    pub socket_name: String,
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
                network: RemoteNetworkConfig::default(),
                command: Command::Help(help_text()),
            });
        }

        args.remove(0);
        let network = parse_global_network_config(&mut args)?;

        if args.is_empty() {
            return Ok(Self {
                network,
                command: Command::Workspace,
            });
        }

        let command = match args[0].as_str() {
            "__chrome-refresh-socket" => {
                args.remove(0);
                Command::ChromeRefreshSocket(parse_socket_name_command(args)?)
            }
            "__ui-sidebar" => {
                args.remove(0);
                Command::UiSidebar(parse_ui_pane(args)?)
            }
            "__ui-footer" => {
                args.remove(0);
                Command::UiFooter(parse_ui_pane(args)?)
            }
            "__remote-main-slot" => {
                args.remove(0);
                Command::RemoteMainSlot(parse_remote_main_slot(args)?)
            }
            "__remote-server-console" => {
                args.remove(0);
                Command::RemoteServerConsole(parse_remote_server_console(args)?)
            }
            "__remote-authority-target-host" => {
                args.remove(0);
                Command::RemoteAuthorityTargetHost(parse_remote_authority_target_host(args)?)
            }
            "__remote-authority-output-pump" => {
                args.remove(0);
                Command::RemoteAuthorityOutputPump(parse_remote_authority_output_pump(args)?)
            }
            "__remote-target-publication-server" => {
                args.remove(0);
                Command::RemoteTargetPublicationServer(parse_remote_target_publication_server(
                    args,
                )?)
            }
            "__remote-target-publication-agent" => {
                args.remove(0);
                Command::RemoteTargetPublicationAgent(parse_remote_target_publication_agent(args)?)
            }
            "__remote-target-publication-sender" => {
                args.remove(0);
                Command::RemoteTargetPublicationSender(parse_remote_target_publication_sender(
                    args,
                )?)
            }
            "__remote-target-publication-owner" => {
                args.remove(0);
                Command::RemoteTargetPublicationOwner(parse_remote_target_publication_owner(args)?)
            }
            "__remote-runtime-owner" => {
                args.remove(0);
                Command::RemoteRuntimeOwner(parse_remote_runtime_owner(args)?)
            }
            "__socket-lifecycle-hook" => {
                args.remove(0);
                Command::SocketLifecycleHook(parse_socket_lifecycle_hook_command(args)?)
            }
            "__remote-target-bind-publication" => {
                args.remove(0);
                Command::RemoteTargetBindPublication(parse_remote_target_bind_publication(args)?)
            }
            "__remote-target-unbind-publication" => {
                args.remove(0);
                Command::RemoteTargetUnbindPublication(parse_remote_target_unbind_publication(
                    args,
                )?)
            }
            "__remote-target-reconcile-publications" => {
                args.remove(0);
                Command::RemoteTargetReconcilePublications(
                    parse_remote_target_reconcile_publications(args)?,
                )
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
            "__chrome-refresh-signal" => {
                args.remove(0);
                Command::ChromeRefreshSignal(parse_ui_pane(args)?)
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
                parse_no_args(args)?;
                Command::List
            }
            "detach" => {
                args.remove(0);
                Command::Detach(parse_detach(args)?)
            }
            "help" => Command::Help(help_text()),
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

        Ok(Self { network, command })
    }
}

fn parse_global_network_config(args: &mut Vec<String>) -> Result<RemoteNetworkConfig, CliError> {
    let mut network = RemoteNetworkConfig::default();

    loop {
        let Some(flag) = args.first().cloned() else {
            break;
        };
        match flag.as_str() {
            "--port" => {
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
            _ => break,
        }
    }

    Ok(network)
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

fn parse_socket_name_command(args: Vec<String>) -> Result<SocketNameCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(SocketNameCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
    })
}

fn parse_socket_lifecycle_hook_command(
    args: Vec<String>,
) -> Result<SocketLifecycleHookCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut hook_name = None;
    let mut session_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--hook-name" => hook_name = Some(expect_value("--hook-name", &mut iter)?),
            "--session-name" => session_name = Some(expect_value("--session-name", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(SocketLifecycleHookCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        hook_name,
        session_name,
    })
}

fn parse_remote_main_slot(args: Vec<String>) -> Result<RemoteMainSlotCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut session_name = None;
    let mut target = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--session-name" => session_name = Some(expect_value("--session-name", &mut iter)?),
            "--target" => target = Some(expect_value("--target", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteMainSlotCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        session_name: session_name
            .ok_or_else(|| CliError::MissingValue("--session-name".to_string()))?,
        target: target.ok_or_else(|| CliError::MissingValue("--target".to_string()))?,
    })
}

fn parse_remote_server_console(args: Vec<String>) -> Result<RemoteServerConsoleCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut console_name = None;
    let mut target = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--console-name" => console_name = Some(expect_value("--console-name", &mut iter)?),
            "--target" => target = Some(expect_value("--target", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteServerConsoleCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        console_name: console_name
            .ok_or_else(|| CliError::MissingValue("--console-name".to_string()))?,
        target,
    })
}

fn parse_remote_authority_target_host(
    args: Vec<String>,
) -> Result<RemoteAuthorityTargetHostCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut target_session_name = None;
    let mut authority_id = None;
    let mut target_id = None;
    let mut transport_socket_path = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--target-session-name" => {
                target_session_name = Some(expect_value("--target-session-name", &mut iter)?)
            }
            "--authority-id" => authority_id = Some(expect_value("--authority-id", &mut iter)?),
            "--target-id" => target_id = Some(expect_value("--target-id", &mut iter)?),
            "--transport-socket-path" => {
                transport_socket_path = Some(expect_value("--transport-socket-path", &mut iter)?)
            }
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteAuthorityTargetHostCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        target_session_name: target_session_name
            .ok_or_else(|| CliError::MissingValue("--target-session-name".to_string()))?,
        authority_id: authority_id
            .ok_or_else(|| CliError::MissingValue("--authority-id".to_string()))?,
        target_id: target_id.ok_or_else(|| CliError::MissingValue("--target-id".to_string()))?,
        transport_socket_path: transport_socket_path
            .ok_or_else(|| CliError::MissingValue("--transport-socket-path".to_string()))?,
    })
}

fn parse_remote_authority_output_pump(
    args: Vec<String>,
) -> Result<RemoteAuthorityOutputPumpCommand, CliError> {
    let mut iter = args.into_iter();
    let mut ingest_socket_path = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--ingest-socket-path" => {
                ingest_socket_path = Some(expect_value("--ingest-socket-path", &mut iter)?)
            }
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteAuthorityOutputPumpCommand {
        ingest_socket_path: ingest_socket_path
            .ok_or_else(|| CliError::MissingValue("--ingest-socket-path".to_string()))?,
    })
}

fn parse_remote_target_publication_server(
    args: Vec<String>,
) -> Result<RemoteTargetPublicationServerCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteTargetPublicationServerCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
    })
}

fn parse_remote_target_publication_agent(
    args: Vec<String>,
) -> Result<RemoteTargetPublicationAgentCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteTargetPublicationAgentCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
    })
}

fn parse_remote_target_publication_owner(
    args: Vec<String>,
) -> Result<RemoteTargetPublicationOwnerCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut target_session_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--target-session-name" => {
                target_session_name = Some(expect_value("--target-session-name", &mut iter)?)
            }
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteTargetPublicationOwnerCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        target_session_name: target_session_name
            .ok_or_else(|| CliError::MissingValue("--target-session-name".to_string()))?,
    })
}

fn parse_remote_target_publication_sender(
    args: Vec<String>,
) -> Result<RemoteTargetPublicationSenderCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteTargetPublicationSenderCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
    })
}

fn parse_remote_runtime_owner(args: Vec<String>) -> Result<RemoteRuntimeOwnerCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteRuntimeOwnerCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
    })
}

fn parse_remote_target_bind_publication(
    args: Vec<String>,
) -> Result<RemoteTargetBindPublicationCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut target_session_name = None;
    let mut authority_id = None;
    let mut transport_session_id = None;
    let mut selector = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--target-session-name" => {
                target_session_name = Some(expect_value("--target-session-name", &mut iter)?)
            }
            "--authority-id" => authority_id = Some(expect_value("--authority-id", &mut iter)?),
            "--transport-session-id" => {
                transport_session_id = Some(expect_value("--transport-session-id", &mut iter)?)
            }
            "--selector" => selector = Some(expect_value("--selector", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteTargetBindPublicationCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        target_session_name: target_session_name
            .ok_or_else(|| CliError::MissingValue("--target-session-name".to_string()))?,
        authority_id: authority_id
            .ok_or_else(|| CliError::MissingValue("--authority-id".to_string()))?,
        transport_session_id: transport_session_id
            .ok_or_else(|| CliError::MissingValue("--transport-session-id".to_string()))?,
        selector,
    })
}

fn parse_remote_target_reconcile_publications(
    args: Vec<String>,
) -> Result<RemoteTargetReconcilePublicationsCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteTargetReconcilePublicationsCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
    })
}

fn parse_remote_target_unbind_publication(
    args: Vec<String>,
) -> Result<RemoteTargetUnbindPublicationCommand, CliError> {
    let mut iter = args.into_iter();
    let mut socket_name = None;
    let mut target_session_name = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = Some(expect_value("--socket-name", &mut iter)?),
            "--target-session-name" => {
                target_session_name = Some(expect_value("--target-session-name", &mut iter)?)
            }
            "--help" | "-h" => {}
            _ => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    Ok(RemoteTargetUnbindPublicationCommand {
        socket_name: socket_name
            .ok_or_else(|| CliError::MissingValue("--socket-name".to_string()))?,
        target_session_name: target_session_name
            .ok_or_else(|| CliError::MissingValue("--target-session-name".to_string()))?,
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
        "  waitagent [--port <port>] [--connect <host:port>]",
        "  waitagent [--port <port>] [--connect <host:port>] attach [<target>]",
        "  waitagent [--port <port>] [--connect <host:port>] ls",
        "  waitagent [--port <port>] [--connect <host:port>] detach [<target>]",
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
    use super::{Cli, Command, DEFAULT_REMOTE_NODE_PORT};

    fn parse(args: &[&str]) -> Cli {
        let argv = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
        Cli::parse(argv).expect("cli parse should succeed")
    }

    #[test]
    fn defaults_to_workspace_command_without_subcommand() {
        let cli = parse(&["waitagent"]);
        assert!(matches!(cli.command, Command::Workspace));
        assert_eq!(cli.network.port, DEFAULT_REMOTE_NODE_PORT);
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
            "attach",
            "wa-1:waitagent-1",
        ]);

        assert_eq!(cli.network.port, 8484);
        assert_eq!(cli.network.connect.as_deref(), Some("remote.example:7474"));
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
    fn parses_hidden_sidebar_pane_command() {
        match parse(&[
            "waitagent",
            "__ui-sidebar",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
        ])
        .command
        {
            Command::UiSidebar(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_remote_server_console_command() {
        match parse(&[
            "waitagent",
            "__remote-server-console",
            "--socket-name",
            "wa-1",
            "--console-name",
            "console-a",
            "--target",
            "peer-a:shell-1",
        ])
        .command
        {
            Command::RemoteServerConsole(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.console_name, "console-a");
                assert_eq!(command.target.as_deref(), Some("peer-a:shell-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_remote_server_console_command_without_target() {
        match parse(&[
            "waitagent",
            "__remote-server-console",
            "--socket-name",
            "wa-1",
            "--console-name",
            "console-a",
        ])
        .command
        {
            Command::RemoteServerConsole(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.console_name, "console-a");
                assert!(command.target.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_remote_authority_target_host_command() {
        match parse(&[
            "waitagent",
            "__remote-authority-target-host",
            "--socket-name",
            "wa-1",
            "--target-session-name",
            "target-1",
            "--authority-id",
            "peer-a",
            "--target-id",
            "remote-peer:peer-a:target-1",
            "--transport-socket-path",
            "/tmp/transport.sock",
        ])
        .command
        {
            Command::RemoteAuthorityTargetHost(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.target_session_name, "target-1");
                assert_eq!(command.authority_id, "peer-a");
                assert_eq!(command.target_id, "remote-peer:peer-a:target-1");
                assert_eq!(command.transport_socket_path, "/tmp/transport.sock");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_remote_authority_output_pump_command() {
        match parse(&[
            "waitagent",
            "__remote-authority-output-pump",
            "--ingest-socket-path",
            "/tmp/output.sock",
        ])
        .command
        {
            Command::RemoteAuthorityOutputPump(command) => {
                assert_eq!(command.ingest_socket_path, "/tmp/output.sock");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_remote_target_unbind_publication_command() {
        match parse(&[
            "waitagent",
            "__remote-target-unbind-publication",
            "--socket-name",
            "wa-1",
            "--target-session-name",
            "target-1",
        ])
        .command
        {
            Command::RemoteTargetUnbindPublication(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.target_session_name, "target-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_socket_lifecycle_hook_command() {
        match parse(&[
            "waitagent",
            "__socket-lifecycle-hook",
            "--socket-name",
            "wa-1",
            "--hook-name",
            "client-attached",
            "--session-name",
            "target-1",
        ])
        .command
        {
            Command::SocketLifecycleHook(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.hook_name.as_deref(), Some("client-attached"));
                assert_eq!(command.session_name.as_deref(), Some("target-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_chrome_refresh_socket_command() {
        match parse(&[
            "waitagent",
            "__chrome-refresh-socket",
            "--socket-name",
            "wa-1",
        ])
        .command
        {
            Command::ChromeRefreshSocket(command) => {
                assert_eq!(command.socket_name, "wa-1");
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
        ])
        .command
        {
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
        ])
        .command
        {
            Command::ChromeRefresh(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
                assert_eq!(command.workspace_dir, "/tmp/workspace");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_chrome_refresh_signal_command() {
        match parse(&[
            "waitagent",
            "__chrome-refresh-signal",
            "--socket-name",
            "wa-1",
            "--session-name",
            "waitagent-1",
        ])
        .command
        {
            Command::ChromeRefreshSignal(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_chrome_refresh_all_command() {
        assert!(matches!(
            parse(&["waitagent", "__chrome-refresh-all"]).command,
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
        ])
        .command
        {
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
        ])
        .command
        {
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
        ])
        .command
        {
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
        ])
        .command
        {
            Command::ToggleFullscreen(command) => {
                assert_eq!(command.socket_name, "wa-1");
                assert_eq!(command.session_name, "waitagent-1");
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
