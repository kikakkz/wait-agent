//! `waitagent relay serve` runtime: starts the relay transport gate (TLS
//! listener with mTLS whitelist auth, see `infra::relay_server`) and runs
//! until the process is killed. Graceful signal shutdown arrives with the
//! admin socket slice; the register/heartbeat protocol arrives with the
//! connection-table slice.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::cli::{RelayServeCommand, RemoteNetworkConfig};
use crate::error::AppError;
use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_server::{self, RelayServeConfig, DEFAULT_RELAY_LISTEN_PORT};
use crate::lifecycle::LifecycleError;

pub fn run(command: RelayServeCommand, network: &RemoteNetworkConfig) -> Result<(), AppError> {
    let listen_text = command
        .listen
        .clone()
        .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_RELAY_LISTEN_PORT}"));
    let listen: SocketAddr = listen_text.parse().map_err(|error| {
        AppError::Lifecycle(LifecycleError::Protocol(format!(
            "invalid --listen address {listen_text:?}: {error}"
        )))
    })?;

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
        let authorized = relay_server::authorized_node_fingerprints(&whitelist_dir).map_err(
            |error| {
                AppError::Lifecycle(LifecycleError::Io(
                    format!("scan {}", whitelist_dir.display()),
                    error,
                ))
            },
        )?;
        let handle = relay_server::start(config).await.map_err(|error| {
            AppError::Lifecycle(LifecycleError::Protocol(format!(
                "relay listener: {error}"
            )))
        })?;
        println!("relay listening on {}", handle.local_addr());
        println!("relay identity fingerprint: {fingerprint}");
        println!(
            "authorized nodes: {} ({})",
            authorized.len(),
            whitelist_dir.display()
        );
        println!("active connections: {}", handle.active_connections());
        println!(
            "transport gate only: authenticated connections are held open until the register/heartbeat protocol lands"
        );
        // Run until the process is killed; the admin socket slice adds
        // graceful shutdown.
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let _ = rx.await;
        Ok(())
    })
}
