//! `waitagent web` subcommands: `serve` starts the WebUI service (issue
//! #131). Slice 1 wires only the serve surface: an optional `--listen`
//! override defaulting to loopback.

use super::{CliError, Command};

#[derive(Debug, Clone, Default)]
pub struct WebServeCommand {
    /// Listen address override, e.g. `127.0.0.1:8788`.
    pub listen: Option<String>,
}

pub(crate) fn parse_web(mut args: Vec<String>) -> Result<Command, CliError> {
    let Some(subcommand) = args.first().cloned() else {
        return Err(CliError::MissingValue("web subcommand (serve)".to_string()));
    };
    match subcommand.as_str() {
        "serve" => {
            args.remove(0);
            Ok(Command::WebServe(parse_web_serve(args)?))
        }
        other => Err(CliError::UnknownSubcommand(format!("web {other}"))),
    }
}

fn parse_web_serve(mut args: Vec<String>) -> Result<WebServeCommand, CliError> {
    let mut command = WebServeCommand::default();
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--listen" => {
                args.remove(0);
                let value = args
                    .first()
                    .cloned()
                    .ok_or_else(|| CliError::MissingValue("--listen".to_string()))?;
                args.remove(0);
                if value.trim().is_empty() {
                    return Err(CliError::InvalidValue("--listen".to_string(), value));
                }
                command.listen = Some(value);
            }
            "--help" | "-h" => return Ok(command),
            _ => return Err(CliError::UnexpectedArgument(flag)),
        }
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
    fn parses_web_serve_command_with_defaults() {
        match parse(&["waitagent", "web", "serve"]).command {
            Command::WebServe(command) => {
                assert_eq!(command.listen, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_web_serve_command_listen_flag() {
        match parse(&["waitagent", "web", "serve", "--listen", "127.0.0.1:9999"]).command {
            Command::WebServe(command) => {
                assert_eq!(command.listen.as_deref(), Some("127.0.0.1:9999"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn web_serve_rejects_unknown_flags_and_missing_values() {
        assert_eq!(
            parse_error(&["waitagent", "web", "serve", "--bogus"]),
            "unexpected argument: --bogus"
        );
        assert_eq!(
            parse_error(&["waitagent", "web", "serve", "--listen"]),
            "missing value for --listen"
        );
        assert_eq!(
            parse_error(&["waitagent", "web", "serve", "--listen", " "]),
            "invalid value for --listen:  "
        );
    }

    #[test]
    fn web_requires_a_subcommand() {
        assert_eq!(
            parse_error(&["waitagent", "web"]),
            "missing value for web subcommand (serve)"
        );
    }

    #[test]
    fn rejects_unknown_web_subcommand() {
        assert_eq!(
            parse_error(&["waitagent", "web", "bogus"]),
            "unknown subcommand: web bogus"
        );
    }
}
