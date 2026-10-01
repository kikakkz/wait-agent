//! `waitagent relay` runtimes: `serve` starts the relay (TLS listener with
//! mTLS whitelist auth plus the local admin socket, see
//! `infra::relay_server` / `infra::relay_admin`) and runs until the admin
//! `shutdown` command or a listener error stops it; `status` and `shutdown`
//! speak the admin protocol over the owner-control socket and print clear
//! guidance when the relay is not running — nothing here implicitly starts
//! a daemon.

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cli::{
    RelayServeCommand, RelayShutdownCommand, RelayStatusCommand, RemoteNetworkConfig,
};
use crate::error::AppError;
use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_admin::{relay_admin_addr, relay_not_running_guidance};
use crate::infra::relay_server::{self, RelayServeConfig, DEFAULT_RELAY_LISTEN_PORT};
use crate::lifecycle::LifecycleError;
use crate::platform::remote_ipc::RemoteControlAsyncStream;

fn default_listen_text() -> String {
    format!("0.0.0.0:{DEFAULT_RELAY_LISTEN_PORT}")
}

fn parse_listen(listen: &Option<String>) -> Result<(String, SocketAddr), AppError> {
    let listen_text = listen.clone().unwrap_or_else(default_listen_text);
    let addr: SocketAddr = listen_text.parse().map_err(|error| {
        AppError::Lifecycle(LifecycleError::Protocol(format!(
            "invalid --listen address {listen_text:?}: {error}"
        )))
    })?;
    Ok((listen_text, addr))
}

pub fn run(command: RelayServeCommand, network: &RemoteNetworkConfig) -> Result<(), AppError> {
    let (_listen_text, listen) = parse_listen(&command.listen)?;

    let mut config = RelayServeConfig::new(listen);
    if let Some(dir) = command.authorized_nodes_dir {
        config.authorized_nodes_dir = PathBuf::from(dir);
    }
    if let (Some(key_path), Some(cert_path)) = (&network.node_key_path, &network.node_cert_path) {
        config.credentials = NodeCredentialPaths {
            key_path: key_path.into(),
            cert_path: cert_path.into(),
        };
    }

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
        println!("relay identity fingerprint: {fingerprint}");
        println!(
            "authorized nodes: {} ({})",
            authorized.len(),
            whitelist_dir.display()
        );
        println!("admin socket: {}", handle.admin_addr());
        println!("active connections: {}", handle.active_connections());
        handle.wait_until_stopped().await;
        println!("relay stopped");
        Ok(())
    })
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
fn admin_request(listen: &Option<String>, request: &str) -> Result<String, AppError> {
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
}
