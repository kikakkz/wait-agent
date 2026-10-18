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
    /// Issue #209: do not bind the legacy enrollment listener at
    /// `listen port + 1`; enrollment is served through the main port's
    /// ALPN demux only (CLI wins over the relay.toml key).
    pub no_legacy_enroll: bool,
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
    /// Issue #206: dial the enrollment through the active remote-install
    /// proxy profile and record the proxy marker in the written pin.
    pub via_proxy: bool,
    /// Issue #208: a self-service enrollment link (full URL or bare JWT)
    /// from `relay join-request`; redeemed for a one-time enroll token
    /// against `/auth/enroll` before the standard join.
    pub link: Option<String>,
}

/// Issue #208: `waitagent relay join-request <relay:port>` — the
/// credential-less start of the self-service enrollment; the relay mails a
/// one-time, device-bound magic link to the administrator.
#[derive(Debug, Clone, Default)]
pub struct RelayJoinRequestCommand {
    /// Relay address (`host[:port]`, default 7475).
    pub address: String,
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
        "join-request" => {
            args.remove(0);
            Ok(Command::RelayJoinRequest(parse_relay_join_request(args)?))
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
            "--no-legacy-enroll" => {
                args.remove(0);
                command.no_legacy_enroll = true;
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
    // Everything after a literal `--` is positional: relay invite tokens are
    // base64url and can themselves start with `-` or `--`, which the flag
    // scan would otherwise reject (issue #197: the remote self-enroll broke
    // whenever the token drew a leading dash).
    let mut positional_only = false;
    while let Some(flag) = args.first().cloned() {
        if positional_only {
            args.remove(0);
            if command.address.is_empty() {
                command.address = flag;
            } else if command.token.is_empty() {
                command.token = flag;
            } else {
                return Err(CliError::UnexpectedArgument(flag));
            }
            continue;
        }
        match flag.as_str() {
            "--help" | "-h" => return Ok(command),
            "--via-proxy" => {
                args.remove(0);
                command.via_proxy = true;
            }
            "--link" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--link".to_string()))?;
                args.remove(0);
                command.link = Some(value);
            }
            "--" => {
                args.remove(0);
                positional_only = true;
            }
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
    if command.address.is_empty() || (command.token.is_empty() && command.link.is_none()) {
        return Err(CliError::MissingValue(
            "relay join <address> <token|--link <url-or-token>>".to_string(),
        ));
    }
    Ok(command)
}

/// `relay join-request <address>`: one positional address, nothing else.
fn parse_relay_join_request(mut args: Vec<String>) -> Result<RelayJoinRequestCommand, CliError> {
    let mut command = RelayJoinRequestCommand::default();
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(command),
            _ if flag.starts_with("--") => return Err(CliError::UnexpectedArgument(flag)),
            _ if command.address.is_empty() => {
                args.remove(0);
                command.address = flag;
            }
            _ => return Err(CliError::UnexpectedArgument(flag)),
        }
    }
    if command.address.is_empty() {
        return Err(CliError::MissingValue(
            "relay join-request <address>".to_string(),
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
    fn relay_serve_no_legacy_enroll_is_opt_out() {
        // Issue #209: --no-legacy-enroll closes the port+1 deprecation
        // window; the default keeps it open.
        match parse(&["waitagent", "relay", "serve", "--no-legacy-enroll"]).command {
            Command::RelayServe(command) => assert!(command.no_legacy_enroll),
            other => panic!("unexpected command: {other:?}"),
        }
        match parse(&["waitagent", "relay", "serve"]).command {
            Command::RelayServe(command) => assert!(!command.no_legacy_enroll),
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
    fn relay_join_composes_with_global_credential_path_flags() {
        // --node-key-path/--node-cert-path are global flags (parsed before
        // the subcommand); relay join must see them through the network
        // config (issue #141).
        let cli = parse(&[
            "waitagent",
            "--node-key-path",
            "/tmp/custom.key",
            "--node-cert-path",
            "/tmp/custom.crt",
            "relay",
            "join",
            "relay.example",
            "tok-1",
        ]);
        assert_eq!(
            cli.network.node_key_path.as_deref(),
            Some("/tmp/custom.key")
        );
        assert_eq!(
            cli.network.node_cert_path.as_deref(),
            Some("/tmp/custom.crt")
        );
        match cli.command {
            Command::RelayJoin(command) => {
                assert_eq!(command.address, "relay.example");
                assert_eq!(command.token, "tok-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn relay_join_separator_passes_dash_prefixed_tokens() {
        // Invite tokens are base64url and can draw a leading `-`/`--`;
        // after a literal `--` everything is positional (issue #197).
        match parse(&[
            "waitagent",
            "relay",
            "join",
            "127.0.0.1:7475",
            "--",
            "--VlsuBC_QQn20HPRub7YMn7JYuPm2QeNMaBF-8w6Q4",
        ])
        .command
        {
            Command::RelayJoin(command) => {
                assert_eq!(command.address, "127.0.0.1:7475");
                assert_eq!(command.token, "--VlsuBC_QQn20HPRub7YMn7JYuPm2QeNMaBF-8w6Q4");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // Without the separator a dash-prefixed token stays an error.
        assert_eq!(
            parse_error(&["waitagent", "relay", "join", "127.0.0.1:7475", "--oops"]),
            "unexpected argument: --oops"
        );
    }

    #[test]
    fn relay_join_requires_address_and_token() {
        assert_eq!(
            parse_error(&["waitagent", "relay", "join"]),
            "missing value for relay join <address> <token|--link <url-or-token>>"
        );
        assert_eq!(
            parse_error(&["waitagent", "relay", "join", "relay.example"]),
            "missing value for relay join <address> <token|--link <url-or-token>>"
        );
    }

    #[test]
    fn relay_join_via_proxy_flag_is_opt_in() {
        // Issue #206: --via-proxy routes the enrollment dial through the
        // active proxy profile; it never appears positionally.
        match parse(&[
            "waitagent",
            "relay",
            "join",
            "--via-proxy",
            "relay.example:7475",
            "invite-token",
        ])
        .command
        {
            Command::RelayJoin(command) => {
                assert!(command.via_proxy);
                assert_eq!(command.address, "relay.example:7475");
                assert_eq!(command.token, "invite-token");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match parse(&[
            "waitagent",
            "relay",
            "join",
            "relay.example:7475",
            "invite-token",
        ])
        .command
        {
            Command::RelayJoin(command) => assert!(!command.via_proxy),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn relay_join_link_flag_carries_the_magic_link() {
        // Issue #208: `relay join <relay> --link <url-or-token>` redeems a
        // self-service enrollment link instead of taking the invite token
        // positionally.
        match parse(&[
            "waitagent",
            "relay",
            "join",
            "relay.example:7475",
            "--link",
            "https://dash.example/auth/enroll?token=abc.def.ghi",
        ])
        .command
        {
            Command::RelayJoin(command) => {
                assert_eq!(command.address, "relay.example:7475");
                assert!(command.token.is_empty(), "--link replaces the token");
                assert_eq!(
                    command.link.as_deref(),
                    Some("https://dash.example/auth/enroll?token=abc.def.ghi")
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // A bare JWT is accepted too; a dash-leading base64url value sits in
        // the flag's value slot, never the flag scan (issue #197 rule).
        match parse(&[
            "waitagent",
            "relay",
            "join",
            "relay.example:7475",
            "--link",
            "-dG9rZW4.signature.part",
        ])
        .command
        {
            Command::RelayJoin(command) => {
                assert_eq!(command.link.as_deref(), Some("-dG9rZW4.signature.part"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn relay_join_request_subcommand_parses_the_address() {
        // Issue #208: `relay join-request <relay>` starts the self-service
        // enrollment; no token on the command line.
        match parse(&["waitagent", "relay", "join-request", "relay.example:7475"]).command {
            Command::RelayJoinRequest(command) => {
                assert_eq!(command.address, "relay.example:7475");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(
            parse_error(&["waitagent", "relay", "join-request"]),
            "missing value for relay join-request <address>"
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
