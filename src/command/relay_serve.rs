//! `waitagent relay` runtimes: `serve` starts the relay (TLS listener with
//! mTLS whitelist auth plus the local admin socket, see
//! `infra::relay_server` / `infra::relay_admin`) and runs until the admin
//! `shutdown` command or a listener error stops it; `status` and `shutdown`
//! speak the admin protocol over the owner-control socket and print clear
//! guidance when the relay is not running — nothing here implicitly starts
//! a daemon.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cli::{
    RelayServeCommand, RelayShutdownCommand, RelayStatusCommand, RemoteNetworkConfig,
};
use crate::error::AppError;
use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_admin::{relay_admin_addr, relay_not_running_guidance};
use crate::infra::relay_serve_toml_store::RelayServeTomlConfig;
use crate::infra::relay_server::{
    self, RelayServeConfig, RelayServerHandle, TokenTtlConfig, DEFAULT_RELAY_LISTEN_PORT,
};
use crate::lifecycle::LifecycleError;
use crate::platform::remote_ipc::RemoteControlAsyncStream;
use crate::web::serve::WebServeConfig;

fn default_listen_text() -> String {
    format!("0.0.0.0:{DEFAULT_RELAY_LISTEN_PORT}")
}

pub(crate) fn parse_listen(listen: &Option<String>) -> Result<(String, SocketAddr), AppError> {
    let listen_text = listen.clone().unwrap_or_else(default_listen_text);
    let addr: SocketAddr = listen_text.parse().map_err(|error| {
        AppError::Lifecycle(LifecycleError::Protocol(format!(
            "invalid --listen address {listen_text:?}: {error}"
        )))
    })?;
    Ok((listen_text, addr))
}

/// Assembles the serve config from CLI flags and an (optional) relay.toml,
/// with per-key precedence CLI > file > default. Pure so the matrix is
/// unit-testable without starting a relay.
fn build_serve_config(
    command: &RelayServeCommand,
    file: Option<&RelayServeTomlConfig>,
) -> Result<RelayServeConfig, AppError> {
    let file_listen = file.and_then(|config| config.listen);
    let listen: SocketAddr = match &command.listen {
        Some(cli) => parse_listen(&Some(cli.clone()))?.1,
        None => match file_listen {
            Some(addr) => addr,
            None => parse_listen(&None)?.1,
        },
    };

    let mut config = RelayServeConfig::new(listen);
    if let Some(dir) = &command.authorized_nodes_dir {
        config.authorized_nodes_dir = PathBuf::from(dir);
    } else if let Some(dir) = file.and_then(|config| config.whitelist_dir.clone()) {
        config.authorized_nodes_dir = dir;
    }

    let mut token_ttls = TokenTtlConfig::default();
    if let Some(secs) = file.and_then(|config| config.token_invite_ttl_secs) {
        token_ttls.invite = Duration::from_secs(secs);
    }
    if let Some(secs) = file.and_then(|config| config.token_deploy_ttl_secs) {
        token_ttls.deploy = Duration::from_secs(secs);
    }
    config.token_ttls = token_ttls;

    if let Some(secs) = file.and_then(|config| config.heartbeat_offline_after_secs) {
        config.lifecycle.offline_after = Duration::from_secs(secs);
    }
    if let Some(secs) = file.and_then(|config| config.register_timeout_secs) {
        config.lifecycle.register_timeout = Duration::from_secs(secs);
    }

    if let Some(value) = file.and_then(|config| config.capacity_max_nodes) {
        config.capacity.max_nodes = value;
    }
    if let Some(value) = file.and_then(|config| config.capacity_max_streams) {
        config.capacity.max_streams = value;
    }
    if let Some(value) = file.and_then(|config| config.capacity_max_throughput_bytes_per_sec) {
        config.capacity.max_throughput_bytes_per_sec = value;
    }

    if let Some(path) = file.and_then(|config| config.admin_socket.clone()) {
        #[cfg(windows)]
        {
            return Err(AppError::Lifecycle(LifecycleError::Protocol(format!(
                "relay.toml `admin_socket` ({}) is only supported on Unix; on Windows the admin listener derives from --listen (port + 20000)",
                path.display()
            ))));
        }
        #[cfg(not(windows))]
        {
            config.admin_socket = Some(path);
        }
    }

    Ok(config)
}

pub fn run(command: RelayServeCommand, network: &RemoteNetworkConfig) -> Result<(), AppError> {
    let toml_path = crate::infra::relay_serve_toml_store::default_path();
    let file_config = match RelayServeTomlConfig::load(&toml_path) {
        Ok(config) => config,
        Err(error) => {
            return Err(AppError::Lifecycle(LifecycleError::Protocol(format!(
                "invalid relay config {}: {error}",
                toml_path.display()
            ))));
        }
    };
    if file_config.is_some() {
        println!("relay config: {}", toml_path.display());
    }
    let mut config = build_serve_config(&command, file_config.as_ref())?;

    config.credentials = NodeCredentialPaths::resolve_overrides(
        network.node_key_path.as_deref(),
        network.node_cert_path.as_deref(),
    );

    // `--web` resolves the WebUI config up front so a bad `--web-listen`
    // fails before the relay binds anything.
    let web_config = if command.web {
        Some(build_web_config(&command)?)
    } else {
        None
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            AppError::Lifecycle(LifecycleError::Io("build relay runtime".to_string(), error))
        })?;

    runtime.block_on(async move {
        let fingerprint =
            node_credentials::ensure_credentials(&config.credentials).map_err(|error| {
                AppError::Lifecycle(LifecycleError::Protocol(format!(
                    "relay credentials: {error}"
                )))
            })?;
        let whitelist_dir = config.authorized_nodes_dir.clone();
        let authorized =
            relay_server::authorized_node_fingerprints(&whitelist_dir).map_err(|error| {
                AppError::Lifecycle(LifecycleError::Io(
                    format!("scan {}", whitelist_dir.display()),
                    error,
                ))
            })?;
        let started = relay_server::start(config).await.map_err(|error| {
            AppError::Lifecycle(LifecycleError::Protocol(format!("relay listener: {error}")))
        })?;
        let handle = started.server;
        drop(started.events);
        println!("relay listening on {}", handle.local_addr());
        println!("enrollment listener on {}", handle.enroll_local_addr());
        println!("relay identity fingerprint: {fingerprint}");
        println!(
            "authorized nodes: {} ({})",
            authorized.len(),
            whitelist_dir.display()
        );
        println!("admin socket: {}", handle.admin_addr());
        println!("active connections: {}", handle.active_connections());
        match web_config {
            Some(web_config) => run_web_supervised(handle, web_config).await,
            None => {
                handle.wait_until_stopped().await;
                println!("relay stopped");
                Ok(())
            }
        }
    })
}

/// Assembles the WebUI config for `relay serve --web`: the stock
/// `waitagent web serve` defaults (WebServeConfig::from_waitagent_home),
/// with the optional `--web-listen` override applied.
fn build_web_config(command: &RelayServeCommand) -> Result<WebServeConfig, AppError> {
    let mut config = WebServeConfig::from_waitagent_home();
    if let Some(listen) = &command.web_listen {
        config.listen = listen.parse().map_err(|error| {
            AppError::Lifecycle(LifecycleError::Protocol(format!(
                "invalid --web-listen address {listen:?}: {error}"
            )))
        })?;
    }
    Ok(config)
}

/// Runs the WebUI half alongside the relay under fail-stop supervision
/// (issue #142): the two halves share one process lifetime, and either
/// half ending ends the whole unit.
///
/// * Relay ends first (admin `shutdown` or a listener error) -> the web
///   task is aborted and the process exits.
/// * Web ends first (startup failure, runtime crash, or a Ctrl-C that
///   drains the axum server) -> the relay is shut down through the cloned
///   shutdown sender; a web failure propagates as the process error, so
///   `relay serve --web` without a deployment config (webui.toml, pinned
///   relay.toml) dies at startup with the web error instead of running a
///   headless relay.
///
/// No mutable state crosses the halves: the web service talks to the relay
/// over the loopback-standard node<->relay protocol, and the only coupling
/// is the shutdown channel.
async fn run_web_supervised(
    handle: RelayServerHandle,
    web_config: WebServeConfig,
) -> Result<(), AppError> {
    let relay_shutdown = handle.shutdown_sender();
    let relay_wait = handle.wait_until_stopped();
    tokio::pin!(relay_wait);
    let mut web_task = tokio::spawn(async move { crate::web::serve::run(&web_config).await });
    tokio::select! {
        _ = &mut relay_wait => {
            // Relay first: the web half exits with the process.
            web_task.abort();
            let _ = web_task.await;
            println!("relay stopped");
            Ok(())
        }
        outcome = &mut web_task => {
            // Web first: bring the relay down for a single clean exit, then
            // propagate the web outcome.
            let _ = relay_shutdown.send(true);
            relay_wait.await;
            println!("relay stopped");
            match outcome {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(AppError::Lifecycle(LifecycleError::Protocol(
                    format!("web service stopped: {error}"),
                ))),
                Err(join_error) => Err(AppError::Lifecycle(LifecycleError::Protocol(
                    format!("web service task failed: {join_error}"),
                ))),
            }
        }
    }
}

pub fn run_status(command: RelayStatusCommand) -> Result<(), AppError> {
    let response = admin_request(&command.listen, r#"{"command":"status"}"#)?;
    println!("{response}");
    Ok(())
}

pub fn run_shutdown(command: RelayShutdownCommand) -> Result<(), AppError> {
    let response = admin_request(&command.listen, r#"{"command":"shutdown"}"#)?;
    println!("{response}");
    Ok(())
}

/// Sends one admin request and returns the response body. A missing or
/// unreachable socket is a clear guidance error, never an implicit start.
/// Shared by the status/shutdown/invite/remove commands.
pub(crate) fn admin_request(listen: &Option<String>, request: &str) -> Result<String, AppError> {
    let (listen_text, listen_addr) = parse_listen(listen)?;
    let addr = relay_admin_addr(listen_addr);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            AppError::Lifecycle(LifecycleError::Io(
                "build relay admin runtime".to_string(),
                error,
            ))
        })?;
    runtime.block_on(async move {
        let mut stream = RemoteControlAsyncStream::connect(&addr)
            .await
            .map_err(|error| {
                AppError::Lifecycle(LifecycleError::Protocol(format!(
                    "{}\n(detail: {error})",
                    relay_not_running_guidance(&addr, &listen_text)
                )))
            })?;
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|error| {
                AppError::Lifecycle(LifecycleError::Io(
                    "write relay admin request".to_string(),
                    error,
                ))
            })?;
        stream.shutdown().await.map_err(|error| {
            AppError::Lifecycle(LifecycleError::Io(
                "shut down relay admin request".to_string(),
                error,
            ))
        })?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .map_err(|error| {
                AppError::Lifecycle(LifecycleError::Io(
                    "read relay admin response".to_string(),
                    error,
                ))
            })?;
        Ok(response)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_listen_text_matches_the_serve_default() {
        assert_eq!(default_listen_text(), "0.0.0.0:7475");
    }

    #[test]
    fn absent_file_yields_flag_defaults() {
        let config = build_serve_config(&RelayServeCommand::default(), None)
            .expect("assembly should succeed");
        assert_eq!(config.listen.to_string(), "0.0.0.0:7475");
        assert_eq!(
            config.authorized_nodes_dir,
            crate::host::ssh::remote_host_home::waitagent_home().join("authorized_nodes")
        );
        assert_eq!(
            config.token_ttls,
            TokenTtlConfig {
                invite: crate::infra::relay_enrollment::DEFAULT_INVITE_TTL,
                deploy: crate::infra::relay_enrollment::DEFAULT_DEPLOY_TTL,
            }
        );
        let defaults = crate::infra::relay_connection_table::RelayLifecycleConfig::default();
        assert_eq!(config.lifecycle.offline_after, defaults.offline_after);
        assert_eq!(config.lifecycle.register_timeout, defaults.register_timeout);
        assert_eq!(config.lifecycle.sweep_interval, defaults.sweep_interval);
        assert_eq!(
            config.capacity,
            crate::infra::relay_capacity::RelayCapacityConfig::default()
        );
        assert_eq!(config.admin_socket, None);
    }

    #[test]
    fn file_keys_apply_without_cli_flags() {
        let file = RelayServeTomlConfig {
            listen: Some("127.0.0.1:9999".parse().expect("socket addr")),
            whitelist_dir: Some(PathBuf::from("/tmp/file-whitelist")),
            token_invite_ttl_secs: Some(60),
            token_deploy_ttl_secs: Some(120),
            heartbeat_offline_after_secs: Some(90),
            register_timeout_secs: Some(7),
            capacity_max_nodes: Some(3),
            capacity_max_streams: Some(5),
            capacity_max_throughput_bytes_per_sec: Some(4096),
            ..RelayServeTomlConfig::default()
        };
        let config = build_serve_config(&RelayServeCommand::default(), Some(&file))
            .expect("assembly should succeed");
        assert_eq!(config.listen.to_string(), "127.0.0.1:9999");
        assert_eq!(
            config.authorized_nodes_dir,
            PathBuf::from("/tmp/file-whitelist")
        );
        assert_eq!(config.token_ttls.invite, Duration::from_secs(60));
        assert_eq!(config.token_ttls.deploy, Duration::from_secs(120));
        assert_eq!(config.lifecycle.offline_after, Duration::from_secs(90));
        assert_eq!(config.lifecycle.register_timeout, Duration::from_secs(7));
        assert_eq!(config.capacity.max_nodes, 3);
        assert_eq!(config.capacity.max_streams, 5);
        assert_eq!(config.capacity.max_throughput_bytes_per_sec, 4096);
    }

    #[cfg(not(windows))]
    #[test]
    fn file_admin_socket_applies_on_unix() {
        let file = RelayServeTomlConfig {
            admin_socket: Some(PathBuf::from("/tmp/waitagent-admin.sock")),
            ..RelayServeTomlConfig::default()
        };
        let config = build_serve_config(&RelayServeCommand::default(), Some(&file))
            .expect("assembly should succeed");
        assert_eq!(
            config.admin_socket,
            Some(PathBuf::from("/tmp/waitagent-admin.sock"))
        );
    }

    #[test]
    fn cli_flags_beat_file_values() {
        let file = RelayServeTomlConfig {
            listen: Some("127.0.0.1:9999".parse().expect("socket addr")),
            whitelist_dir: Some(PathBuf::from("/tmp/file-whitelist")),
            ..RelayServeTomlConfig::default()
        };
        let command = RelayServeCommand {
            listen: Some("127.0.0.1:1111".to_string()),
            authorized_nodes_dir: Some("/tmp/cli-whitelist".to_string()),
            ..RelayServeCommand::default()
        };
        let config = build_serve_config(&command, Some(&file)).expect("assembly should succeed");
        assert_eq!(config.listen.to_string(), "127.0.0.1:1111");
        assert_eq!(
            config.authorized_nodes_dir,
            PathBuf::from("/tmp/cli-whitelist")
        );
    }

    #[test]
    fn web_config_defaults_match_standalone_web_serve() {
        let config = build_web_config(&RelayServeCommand {
            web: true,
            ..RelayServeCommand::default()
        })
        .expect("assembly should succeed");
        let standalone = WebServeConfig::from_waitagent_home();
        assert_eq!(config.listen, standalone.listen);
        assert_eq!(config.credentials, standalone.credentials);
        assert_eq!(config.relay_toml_path, standalone.relay_toml_path);
    }

    #[test]
    fn web_config_applies_the_web_listen_override() {
        let config = build_web_config(&RelayServeCommand {
            web: true,
            web_listen: Some("127.0.0.1:9999".to_string()),
            ..RelayServeCommand::default()
        })
        .expect("assembly should succeed");
        assert_eq!(config.listen.to_string(), "127.0.0.1:9999");
    }

    #[test]
    fn web_config_rejects_a_bad_web_listen_address() {
        let error = build_web_config(&RelayServeCommand {
            web: true,
            web_listen: Some("not-an-addr".to_string()),
            ..RelayServeCommand::default()
        })
        .expect_err("a bad --web-listen must fail");
        assert!(
            error.to_string().contains("--web-listen"),
            "the error names the flag: {error}"
        );
    }

    #[test]
    fn file_values_do_not_leak_into_unset_keys() {
        let file = RelayServeTomlConfig {
            token_invite_ttl_secs: Some(60),
            ..RelayServeTomlConfig::default()
        };
        let config = build_serve_config(&RelayServeCommand::default(), Some(&file))
            .expect("assembly should succeed");
        assert_eq!(config.token_ttls.invite, Duration::from_secs(60));
        assert_eq!(
            config.token_ttls.deploy,
            TokenTtlConfig::default().deploy,
            "unset keys keep their defaults"
        );
    }
}
