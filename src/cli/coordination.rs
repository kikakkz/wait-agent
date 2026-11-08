use super::{CliError, Command};

#[derive(Debug, Clone, PartialEq, Eq)]
/// Subcommands of `waitagent coordination`.
pub enum CoordinationSubcommand {
    Serve {
        listen: Option<String>,
        admin_stdout: bool,
    },
    EnrollRequest {
        coordination: String,
    },
    Join {
        link: String,
        coordination: Option<String>,
    },
    Requests {
        coordination: String,
    },
    Approve {
        request_id: String,
        coordination: String,
    },
    Revoke {
        node_id: String,
        coordination: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Parsed `coordination` command line.
pub struct CoordinationCommand {
    pub subcommand: CoordinationSubcommand,
}

/// Parses `coordination <subcommand> [flags]`.
pub fn parse_coordination(mut args: Vec<String>) -> Result<Command, CliError> {
    let Some(subcommand) = args.first().cloned() else {
        return Err(missing("coordination subcommand"));
    };
    args.remove(0);
    let parsed = match subcommand.as_str() {
        "serve" => parse_serve(args)?,
        "enroll-request" => {
            let coordination = required_flag(&mut args, "--coordination")?;
            ensure_empty(args)?;
            CoordinationSubcommand::EnrollRequest { coordination }
        }
        "join" => {
            let link = required_flag(&mut args, "--link")?;
            let coordination = optional_flag(&mut args, "--coordination")?;
            ensure_empty(args)?;
            CoordinationSubcommand::Join { link, coordination }
        }
        "requests" => {
            let coordination = required_flag(&mut args, "--coordination")?;
            ensure_empty(args)?;
            CoordinationSubcommand::Requests { coordination }
        }
        "approve" => {
            let request_id = positional(&mut args, "request id")?;
            let coordination = required_flag(&mut args, "--coordination")?;
            ensure_empty(args)?;
            CoordinationSubcommand::Approve {
                request_id,
                coordination,
            }
        }
        "revoke" => {
            let node_id = positional(&mut args, "node id")?;
            let coordination = required_flag(&mut args, "--coordination")?;
            ensure_empty(args)?;
            CoordinationSubcommand::Revoke {
                node_id,
                coordination,
            }
        }
        other => return Err(CliError::UnexpectedArgument(other.to_string())),
    };
    Ok(Command::Coordination(CoordinationCommand {
        subcommand: parsed,
    }))
}

fn parse_serve(mut args: Vec<String>) -> Result<CoordinationSubcommand, CliError> {
    let mut listen: Option<String> = None;
    let mut admin_stdout = false;
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--listen" => {
                args.remove(0);
                listen = Some(take_value(&mut args, "--listen")?);
            }
            "--admin-stdout" => {
                args.remove(0);
                admin_stdout = true;
            }
            other if other.starts_with("--") => {
                return Err(CliError::UnexpectedArgument(other.to_string()))
            }
            other => return Err(CliError::UnexpectedArgument(other.to_string())),
        }
    }
    Ok(CoordinationSubcommand::Serve {
        listen,
        admin_stdout,
    })
}

fn positional(args: &mut Vec<String>, what: &str) -> Result<String, CliError> {
    let Some(value) = args.first().cloned() else {
        return Err(missing(what));
    };
    if value.starts_with("--") {
        return Err(missing(what));
    }
    args.remove(0);
    Ok(value)
}

fn required_flag(args: &mut Vec<String>, flag: &str) -> Result<String, CliError> {
    optional_flag(args, flag)?.ok_or_else(|| missing(&format!("{flag} <value>")))
}

fn optional_flag(args: &mut Vec<String>, flag: &str) -> Result<Option<String>, CliError> {
    let position = args.iter().position(|arg| arg == flag);
    let Some(position) = position else {
        return Ok(None);
    };
    args.remove(position);
    Ok(Some(take_value_at(args, position, flag)?))
}

fn take_value(args: &mut Vec<String>, flag: &str) -> Result<String, CliError> {
    take_value_at(args, 0, flag)
}

fn take_value_at(args: &mut Vec<String>, position: usize, flag: &str) -> Result<String, CliError> {
    let Some(value) = args.get(position).cloned() else {
        return Err(CliError::MissingValue(flag.to_string()));
    };
    args.remove(position);
    Ok(value)
}

fn ensure_empty(args: Vec<String>) -> Result<(), CliError> {
    match args.first() {
        None => Ok(()),
        Some(extra) => Err(CliError::UnexpectedArgument(extra.clone())),
    }
}

fn missing(what: &str) -> CliError {
    CliError::MissingValue(format!("coordination {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CoordinationCommand {
        let argv = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
        match crate::cli::Cli::parse(argv).expect("parse").command {
            Command::Coordination(command) => command,
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_serve_flags() {
        match parse(&["waitagent", "coordination", "serve"]).subcommand {
            CoordinationSubcommand::Serve {
                listen: None,
                admin_stdout: false,
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&[
            "waitagent",
            "coordination",
            "serve",
            "--listen",
            "0.0.0.0:7479",
            "--admin-stdout",
        ])
        .subcommand
        {
            CoordinationSubcommand::Serve {
                listen: Some(listen),
                admin_stdout: true,
            } => assert_eq!(listen, "0.0.0.0:7479"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_enroll_request_and_join() {
        match parse(&[
            "waitagent",
            "coordination",
            "enroll-request",
            "--coordination",
            "addr.json",
        ])
        .subcommand
        {
            CoordinationSubcommand::EnrollRequest { coordination } => {
                assert_eq!(coordination, "addr.json")
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&[
            "waitagent",
            "coordination",
            "join",
            "--link",
            "waitagent://join?token=abc",
        ])
        .subcommand
        {
            CoordinationSubcommand::Join {
                link,
                coordination: None,
            } => assert_eq!(link, "waitagent://join?token=abc"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&[
            "waitagent",
            "coordination",
            "join",
            "--link",
            "abc",
            "--coordination",
            "id@host:7479",
        ])
        .subcommand
        {
            CoordinationSubcommand::Join {
                link,
                coordination: Some(coordination),
            } => {
                assert_eq!(link, "abc");
                assert_eq!(coordination, "id@host:7479");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_operator_commands() {
        match parse(&[
            "waitagent",
            "coordination",
            "requests",
            "--coordination",
            "c",
        ])
        .subcommand
        {
            CoordinationSubcommand::Requests { coordination } => assert_eq!(coordination, "c"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&[
            "waitagent",
            "coordination",
            "approve",
            "req-1",
            "--coordination",
            "c",
        ])
        .subcommand
        {
            CoordinationSubcommand::Approve {
                request_id,
                coordination,
            } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(coordination, "c");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&[
            "waitagent",
            "coordination",
            "revoke",
            "node-9",
            "--coordination",
            "c",
        ])
        .subcommand
        {
            CoordinationSubcommand::Revoke {
                node_id,
                coordination,
            } => {
                assert_eq!(node_id, "node-9");
                assert_eq!(coordination, "c");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_and_unknown_arguments() {
        let argv = |args: &[&str]| {
            let argv = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
            crate::cli::Cli::parse(argv)
        };
        assert!(argv(&["waitagent", "coordination", "enroll-request"]).is_err());
        assert!(argv(&["waitagent", "coordination", "approve", "req-1"]).is_err());
        assert!(argv(&["waitagent", "coordination", "join", "--coordination", "c"]).is_err());
        assert!(argv(&["waitagent", "coordination", "serve", "--bogus"]).is_err());
    }
}
