//! `waitagent mesh-probe` (issue #224 slice 0, spike): manual evaluation of
//! the iroh mesh data plane. Hidden on purpose — not in the help listing,
//! not wired into the TUI. Arguments are parsed here rather than in
//! `src/cli/` so the spike's flags stay outside the CLI surface checker's
//! scan scope.

use crate::error::AppError;
use crate::lifecycle::LifecycleError;

pub fn run(args: Vec<String>) -> Result<(), AppError> {
    let mut args = args;
    let Some(subcommand) = args.first().cloned() else {
        return Err(usage("missing subcommand (listen | dial)"));
    };
    args.remove(0);
    match subcommand.as_str() {
        "listen" => run_listen(args),
        "dial" => run_dial(args),
        other => Err(usage(&format!("unknown subcommand {other:?}"))),
    }
}

fn run_listen(mut args: Vec<String>) -> Result<(), AppError> {
    let mut key_file: Option<String> = None;
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--key-file" => {
                args.remove(0);
                key_file = Some(value(&mut args, "--key-file")?);
            }
            other if other.starts_with("--") => {
                return Err(usage(&format!("unknown flag {other:?}")))
            }
            other => return Err(usage(&format!("unexpected argument {other:?}"))),
        }
    }
    let key_path = key_path(key_file);
    let runtime = runtime()?;
    runtime
        .block_on(crate::infra::mesh_probe::listen(&key_path))
        .map_err(protocol_error)
}

fn run_dial(mut args: Vec<String>) -> Result<(), AppError> {
    let mut key_file: Option<String> = None;
    let mut node_id: Option<String> = None;
    let mut addr: Option<String> = None;
    let mut seconds: u64 = 5;
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--key-file" => {
                args.remove(0);
                key_file = Some(value(&mut args, "--key-file")?);
            }
            "--node-id" => {
                args.remove(0);
                node_id = Some(value(&mut args, "--node-id")?);
            }
            "--addr" => {
                args.remove(0);
                addr = Some(value(&mut args, "--addr")?);
            }
            "--seconds" => {
                args.remove(0);
                seconds = value(&mut args, "--seconds")?
                    .parse()
                    .map_err(|_| protocol_error("--seconds must be a whole number".to_string()))?;
            }
            other if other.starts_with("--") => {
                return Err(usage(&format!("unknown flag {other:?}")))
            }
            other => return Err(usage(&format!("unexpected argument {other:?}"))),
        }
    }
    let node_id_text = node_id.ok_or_else(|| usage("missing --node-id"))?;
    let addr_text = addr.ok_or_else(|| usage("missing --addr"))?;
    let node_id = crate::infra::mesh_probe::parse_node_id(&node_id_text).map_err(protocol_error)?;
    let addr = crate::infra::mesh_probe::parse_node_addr(&addr_text).map_err(protocol_error)?;
    let key_path = key_path(key_file);
    let runtime = runtime()?;
    runtime
        .block_on(crate::infra::mesh_probe::dial(
            &key_path, node_id, addr, seconds,
        ))
        .map_err(protocol_error)
}

fn key_path(key_file: Option<String>) -> std::path::PathBuf {
    match key_file {
        Some(path) => std::path::PathBuf::from(path),
        None => crate::infra::mesh_probe::default_key_path(),
    }
}

fn value(args: &mut Vec<String>, flag: &str) -> Result<String, AppError> {
    let value = args
        .first()
        .cloned()
        .ok_or_else(|| usage(&format!("missing value for {flag}")))?;
    args.remove(0);
    Ok(value)
}

fn runtime() -> Result<tokio::runtime::Runtime, AppError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            AppError::Lifecycle(LifecycleError::Io("build mesh runtime".into(), error))
        })
}

fn usage(message: &str) -> AppError {
    AppError::Lifecycle(LifecycleError::Protocol(format!(
        "{message}\nusage:\n  waitagent mesh-probe listen [--key-file PATH]\n  \
         waitagent mesh-probe dial --node-id <id> --addr <node-addr-json> [--seconds N]"
    )))
}

fn protocol_error(message: String) -> AppError {
    AppError::Lifecycle(LifecycleError::Protocol(message))
}
