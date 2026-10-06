//! `waitagent relay` subcommands: `serve` starts the relay; `status` /
//! `shutdown` speak the local admin socket; `invite` / `join` / `remove`
//! run the enrollment lifecycle (docs/relay-design.md 身份认证与入网).

use super::{CliError, Command};

#[derive(Debug, Clone, Default)]
pub struct RelayServeCommand {
    /// Listen address override, e.g. `0.0.0.0:7475`.
    pub listen: Option<String>,
    /// authorized_nodes whitelist directory override.
    pub authorized_nodes_dir: Option<String>,
    /// Launch the WebUI service alongside the relay (issue #142): one
    /// process, still the loopback-standard node<->relay protocol.
    pub web: bool,
    /// WebUI listen address override for `--web` (default `0.0.0.0:8788`).
    pub web_listen: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RelayStatusCommand {
    /// Listen address the relay was started with (derives the admin socket).
    pub listen: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RelayShutdownCommand {
    /// Listen address the relay was started with (derives the admin socket).
    pub listen: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RelayInviteCommand {
    /// Listen address the relay was started with (derives the admin socket).
    pub listen: Option<String>,
    /// Token TTL override in seconds; defaults to the relay's invite/deploy
    /// TTLs.
    pub ttl_secs: Option<String>,
    /// Mint a reusable deploy token instead of a one-time invite token.
    pub deploy: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RelayRemoveCommand {
    /// Listen address the relay was started with (derives the admin socket).
    pub listen: Option<String>,
    /// SHA-256 fingerprint of the node to revoke.
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default)]
pub struct RelayJoinCommand {
    /// Relay address (`host[:port]`, default 7475).
    pub address: String,
    /// Enrollment token minted by `relay invite`.
    pub token: String,
}

pub(crate) fn parse_relay(mut args: Vec<String>) -> Result<Command, CliError> {
    let Some(subcommand) = args.first().cloned() else {
        return Err(CliError::MissingValue(
            "relay subcommand (serve | invite | join | remove | status | shutdown)".to_string(),
        ));
    };
    match subcommand.as_str() {
        "serve" => {
            args.remove(0);
            Ok(Command::RelayServe(parse_relay_serve(args)?))
        }
        "invite" => {
            args.remove(0);
            Ok(Command::RelayInvite(parse_relay_invite(args)?))
        }
        "join" => {
            args.remove(0);
            Ok(Command::RelayJoin(parse_relay_join(args)?))
        }
        "remove" => {
            args.remove(0);
            Ok(Command::RelayRemove(parse_relay_remove(args)?))
        }
        "status" => {
            args.remove(0);
            Ok(Command::RelayStatus(parse_relay_listen_flag(args)?))
        }
        "shutdown" => {
            args.remove(0);
            Ok(Command::RelayShutdown(parse_relay_listen_flag(args)?))
        }
        other => Err(CliError::UnknownSubcommand(format!("relay {other}"))),
    }
}

pub trait HasListenFlag {
    fn set_listen(&mut self, value: String);
}

impl HasListenFlag for RelayStatusCommand {
    fn set_listen(&mut self, value: String) {
        self.listen = Some(value);
    }
}

impl HasListenFlag for RelayShutdownCommand {
    fn set_listen(&mut self, value: String) {
        self.listen = Some(value);
    }
}

fn parse_relay_listen_flag<T: Default + HasListenFlag>(
    mut args: Vec<String>,
) -> Result<T, CliError> {
    let mut command = T::default();
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--listen" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--listen".to_string()))?;
                args.remove(0);
                command.set_listen(value);
            }
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(flag)),
        }
    }
    Ok(command)
}

fn parse_relay_serve(mut args: Vec<String>) -> Result<RelayServeCommand, CliError> {
    let mut command = RelayServeCommand::default();
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--listen" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--listen".to_string()))?;
                args.remove(0);
                command.listen = Some(value);
            }
            "--authorized-nodes" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--authorized-nodes".to_string()))?;
                args.remove(0);
                command.authorized_nodes_dir = Some(value);
            }
            "--web" => {
                args.remove(0);
                command.web = true;
            }
            "--web-listen" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--web-listen".to_string()))?;
                args.remove(0);
                if value.trim().is_empty() {
                    return Err(CliError::InvalidValue("--web-listen".to_string(), value));
                }
                command.web_listen = Some(value);
            }
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(flag)),
        }
    }
    Ok(command)
}

fn parse_relay_invite(mut args: Vec<String>) -> Result<RelayInviteCommand, CliError> {
    let mut command = RelayInviteCommand::default();
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--listen" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--listen".to_string()))?;
                args.remove(0);
                command.listen = Some(value);
            }
            "--ttl" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--ttl".to_string()))?;
                args.remove(0);
                if value.trim().is_empty() {
                    return Err(CliError::InvalidValue("--ttl".to_string(), value));
                }
                command.ttl_secs = Some(value);
            }
            "--deploy" => {
                args.remove(0);
                command.deploy = true;
            }
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(flag)),
        }
    }
    Ok(command)
}

fn parse_relay_remove(mut args: Vec<String>) -> Result<RelayRemoveCommand, CliError> {
    let mut command = RelayRemoveCommand::default();
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--listen" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--listen".to_string()))?;
                args.remove(0);
                command.listen = Some(value);
            }
            "--help" | "-h" => return Ok(command),
            _ if flag.starts_with("--") => return Err(CliError::UnexpectedArgument(flag)),
            _ if command.fingerprint.is_empty() => {
                args.remove(0);
                command.fingerprint = flag;
            }
            _ => return Err(CliError::UnexpectedArgument(flag)),
        }
    }
    if command.fingerprint.is_empty() {
        return Err(CliError::MissingValue(
            "relay remove <fingerprint>".to_string(),
        ));
    }
    Ok(command)
}

fn parse_relay_join(mut args: Vec<String>) -> Result<RelayJoinCommand, CliError> {
    let mut command = RelayJoinCommand::default();
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(command),
            _ if flag.starts_with("--") => return Err(CliError::UnexpectedArgument(flag)),
            _ if command.address.is_empty() => {
                args.remove(0);
                command.address = flag;
            }
            _ if command.token.is_empty() => {
                args.remove(0);
                command.token = flag;
            }
            _ => return Err(CliError::UnexpectedArgument(flag)),
        }
    }
    if command.address.is_empty() || command.token.is_empty() {
        return Err(CliError::MissingValue(
            "relay join <address> <token>".to_string(),
        ));
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command};

    fn parse(args: &[&str]) -> Cli {
        let argv = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
        Cli::parse(argv).expect("cli parse should succeed")
    }

    fn parse_error(args: &[&str]) -> String {
        let argv = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
        Cli::parse(argv).expect_err("parse should fail").to_string()
    }

    #[test]
    fn parses_relay_serve_command_with_defaults() {
        match parse(&["waitagent", "relay", "serve"]).command {
            Command::RelayServe(command) => {
                assert_eq!(command.listen, None);
                assert_eq!(command.authorized_nodes_dir, None);
                assert!(!command.web);
                assert_eq!(command.web_listen, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_relay_serve_web_flags() {
        match parse(&[
            "waitagent",
            "relay",
            "serve",
            "--web",
            "--web-listen",
            "127.0.0.1:9999",
        ])
        .command
        {
            Command::RelayServe(command) => {
                assert!(command.web);
                assert_eq!(command.web_listen.as_deref(), Some("127.0.0.1:9999"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn relay_serve_web_listen_requires_a_value() {
        assert_eq!(
            parse_error(&["waitagent", "relay", "serve", "--web-listen"]),
            "missing value for --web-listen"
        );
        assert_eq!(
            parse_error(&["waitagent", "relay", "serve", "--web-listen", " "]),
            "invalid value for --web-listen:  "
        );
    }

    #[test]
    fn parses_relay_serve_command_flags() {
        match parse(&[
            "waitagent",
            "relay",
            "serve",
            "--listen",
            "0.0.0.0:7475",
            "--authorized-nodes",
            "/tmp/authorized_nodes",
        ])
        .command
        {
            Command::RelayServe(command) => {
                assert_eq!(command.listen.as_deref(), Some("0.0.0.0:7475"));
                assert_eq!(
                    command.authorized_nodes_dir.as_deref(),
                    Some("/tmp/authorized_nodes")
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_relay_invite_command_with_defaults() {
        match parse(&["waitagent", "relay", "invite"]).command {
            Command::RelayInvite(command) => {
                assert_eq!(command.listen, None);
                assert_eq!(command.ttl_secs, None);
                assert!(!command.deploy);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_relay_invite_command_flags() {
        match parse(&[
            "waitagent",
            "relay",
            "invite",
            "--ttl",
            "3600",
            "--deploy",
            "--listen",
            "0.0.0.0:7475",
        ])
        .command
        {
            Command::RelayInvite(command) => {
                assert_eq!(command.ttl_secs.as_deref(), Some("3600"));
                assert!(command.deploy);
                assert_eq!(command.listen.as_deref(), Some("0.0.0.0:7475"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_relay_join_command() {
        match parse(&["waitagent", "relay", "join", "relay.example", "tok-1"]).command {
            Command::RelayJoin(command) => {
                assert_eq!(command.address, "relay.example");
                assert_eq!(command.token, "tok-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn relay_join_requires_address_and_token() {
        assert_eq!(
            parse_error(&["waitagent", "relay", "join"]),
            "missing value for relay join <address> <token>"
        );
        assert_eq!(
            parse_error(&["waitagent", "relay", "join", "relay.example"]),
            "missing value for relay join <address> <token>"
        );
    }

    #[test]
    fn parses_relay_remove_command_with_listen_flag() {
        match parse(&[
            "waitagent",
            "relay",
            "remove",
            "7c857a105486f46d",
            "--listen",
            "0.0.0.0:9999",
        ])
        .command
        {
            Command::RelayRemove(command) => {
                assert_eq!(command.fingerprint, "7c857a105486f46d");
                assert_eq!(command.listen.as_deref(), Some("0.0.0.0:9999"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn relay_remove_requires_a_fingerprint() {
        assert_eq!(
            parse_error(&["waitagent", "relay", "remove"]),
            "missing value for relay remove <fingerprint>"
        );
    }

    #[test]
    fn rejects_unknown_relay_subcommand() {
        assert_eq!(
            parse_error(&["waitagent", "relay", "bogus"]),
            "unknown subcommand: relay bogus"
        );
    }

    #[test]
    fn parses_relay_status_and_shutdown_with_listen_flag() {
        match parse(&["waitagent", "relay", "status"]).command {
            Command::RelayStatus(command) => {
                assert_eq!(command.listen, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match parse(&["waitagent", "relay", "shutdown", "--listen", "0.0.0.0:9999"]).command {
            Command::RelayShutdown(command) => {
                assert_eq!(command.listen.as_deref(), Some("0.0.0.0:9999"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
