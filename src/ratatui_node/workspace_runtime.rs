use crate::cli::RemoteNetworkConfig;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::settings_store::SettingsStore;
use crate::lifecycle::LifecycleError;
use crate::process::current_executable::current_waitagent_executable;
use crate::process::session_leader::spawn_session_leader;
use crate::ratatui_node::client_runtime::RatatuiClientRuntime;
use crate::ratatui_node::node_runtime::{
    node_is_running, ratatui_socket_dir, ratatui_socket_path, remove_node_socket,
    send_node_command, ServerMessageJson,
};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Top-level entry point for `waitagent --ratatui` and its subcommands.
///
/// A ratatui "server" is identified by its listener port. Each port runs a
/// single `__ratatui-node-server` process. Multiple TUI clients can attach to
/// the same server. Sessions inside the server are created via the TUI.
pub struct RatatuiWorkspaceRuntime {
    network: RemoteNetworkConfig,
}

impl RatatuiWorkspaceRuntime {
    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self { network })
    }

    /// Default entry: attach to (or create) the server for the configured port
    /// and run the TUI.
    pub fn run_workspace_entry(&self) -> Result<(), LifecycleError> {
        self.attach_or_create()
    }

    /// Attach to the server for the configured port, creating it if it does not
    /// exist, and run the TUI.
    pub fn attach(&self, _target: Option<String>) -> Result<(), LifecycleError> {
        // `attach` for ratatui currently means "attach to the server selected by
        // --port". An optional index argument is reserved for future use but is
        // not needed because the entry command already implies the port.
        self.attach_or_create()
    }

    /// Detach all clients from a server.
    pub fn detach(&self, target: Option<String>) -> Result<(), LifecycleError> {
        let port = resolve_target_to_port(target.as_deref())?;
        ERROR_LOG.log(format!("[ratatui-workspace] detach port={port}"));

        if !node_is_running(port) {
            return Err(LifecycleError::Protocol(format!(
                "ratatui server on port {port} is not running"
            )));
        }

        let socket_path = ratatui_socket_path(port);
        let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to connect to ratatui node socket {}",
                    socket_path.display()
                ),
                error,
            )
        })?;

        writeln!(stream, "DETACH_ALL").map_err(|error| {
            LifecycleError::Io(
                "failed to send detach command to ratatui node".to_string(),
                error,
            )
        })?;
        stream.flush().map_err(|error| {
            LifecycleError::Io(
                "failed to flush detach command to ratatui node".to_string(),
                error,
            )
        })?;

        // Give the server a moment to process DETACH_ALL before we shut down
        // the write side and wait for the connection to close.
        std::thread::sleep(Duration::from_millis(200));
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut buf = [0u8; 1];
        let _ = stream.read(&mut buf);

        println!("detached clients from ratatui server on port {port}");
        Ok(())
    }

    /// Stop a server.
    pub fn stop(&self, target: Option<String>) -> Result<(), LifecycleError> {
        let port = resolve_target_to_port(target.as_deref())?;
        ERROR_LOG.log(format!("[ratatui-workspace] stop port={port}"));

        if !node_is_running(port) {
            remove_node_socket(port);
            return Err(LifecycleError::Protocol(format!(
                "ratatui server on port {port} is not running"
            )));
        }

        send_node_command(port, "STOP").map_err(|error| {
            LifecycleError::Protocol(format!(
                "failed to send stop command to ratatui server on port {port}: {error}"
            ))
        })?;

        // Wait briefly for the server process to exit and the socket to disappear.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !node_is_running(port) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        remove_node_socket(port);
        println!("stopped ratatui server on port {port}");
        Ok(())
    }

    /// List running ratatui servers for the current user.
    pub fn list(&self) -> Result<(), LifecycleError> {
        let servers = list_ratatui_servers();
        if servers.is_empty() {
            println!("no ratatui servers running");
            return Ok(());
        }

        for (index, server) in servers.iter().enumerate() {
            let idx = index + 1;
            let status = if server.client_count > 0 {
                "attached"
            } else {
                "detached"
            };
            println!(
                "{}: port {}, {}, up {}, {} session{}",
                idx,
                server.port,
                status,
                format_duration(server.uptime_secs),
                server.session_count,
                if server.session_count == 1 { "" } else { "s" }
            );
        }
        Ok(())
    }

    /// Remove stale ratatui server sockets that no longer have a live node.
    pub fn cleanup(&self) -> Result<(), LifecycleError> {
        let mut removed = 0;
        let mut live = 0;
        let socket_dir = ratatui_socket_dir();
        let Ok(entries) = std::fs::read_dir(&socket_dir) else {
            println!("no ratatui sockets to clean in {}", socket_dir.display());
            return Ok(());
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !name.ends_with(".sock") {
                continue;
            }
            let port_str = name.trim_end_matches(".sock");
            let Ok(port) = port_str.parse::<u16>() else {
                continue;
            };
            if node_is_running(port) {
                live += 1;
            } else {
                crate::infra::best_effort::remove_file(&path);
                removed += 1;
            }
        }

        println!("cleaned {removed} stale ratatui sockets (kept {live} live)");
        Ok(())
    }

    /// List sessions inside a server.
    pub fn list_sessions(&self, target: Option<String>) -> Result<(), LifecycleError> {
        let port = resolve_target_to_port(target.as_deref())?;
        let response = send_node_command(port, "LIST_SESSIONS").map_err(|error| {
            LifecycleError::Protocol(format!(
                "failed to list sessions for ratatui server on port {port}: {error}"
            ))
        })?;

        let envelope: ServerMessageJson = serde_json::from_str(&response).map_err(|error| {
            LifecycleError::Protocol(format!(
                "failed to parse session list response for ratatui server on port {port}: {error}"
            ))
        })?;
        let data = match envelope {
            ServerMessageJson::Response(response) if response.ok => response.data,
            ServerMessageJson::Response(response) => {
                return Err(LifecycleError::Protocol(format!(
                    "server returned error for port {port}: {}",
                    response.message.unwrap_or_default()
                )))
            }
            ServerMessageJson::Snapshot(_) => {
                return Err(LifecycleError::Protocol(format!(
                    "unexpected snapshot response for port {port}"
                )))
            }
            ServerMessageJson::History(_) => {
                return Err(LifecycleError::Protocol(format!(
                    "unexpected history response for port {port}"
                )))
            }
        };
        let sessions: Vec<crate::ratatui_node::node_runtime::SessionView> = data
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();

        if sessions.is_empty() {
            println!("no sessions in ratatui server on port {port}");
            return Ok(());
        }

        for session in sessions {
            println!(
                "{} [{}] {}",
                session.id,
                session.task_state,
                session.display_label()
            );
        }
        Ok(())
    }

    fn attach_or_create(&self) -> Result<(), LifecycleError> {
        let port = self.network.port;
        ERROR_LOG.log(format!("[ratatui-workspace] entry port={port}"));

        if !node_is_running(port) {
            self.start_node_server()?;
        } else {
            ERROR_LOG.log(format!(
                "[ratatui-workspace] existing node found for port {port}"
            ));
        }

        RatatuiClientRuntime::from_port(
            port,
            self.network.clone(),
            SettingsStore::new(SettingsStore::default_path()),
        )?
        .run()
    }

    fn start_node_server(&self) -> Result<(), LifecycleError> {
        let executable = current_waitagent_executable()?;
        let socket_path = ratatui_socket_path(self.network.port);
        let port = self.network.port;

        // Remove any stale socket left by a previous crash.
        remove_node_socket(port);

        ERROR_LOG.log(format!(
            "[ratatui-workspace] spawning node server executable={:?} socket={} port={}",
            executable,
            socket_path.display(),
            port
        ));

        let mut command = Command::new(&executable);

        // Inherit global network flags first so the CLI parser consumes them
        // before the subcommand.
        for arg in self.network.to_cli_args() {
            command.arg(arg);
        }

        command
            .arg("__ratatui-node-server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Detach the child into its own session so it survives client exit.
        let _child = spawn_session_leader(&mut command).map_err(|error| {
            LifecycleError::Io("failed to spawn ratatui node server".to_string(), error)
        })?;

        // Wait briefly for the server socket to appear.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket_path.exists() && node_is_running(port) {
                ERROR_LOG.log("[ratatui-workspace] node server ready".to_string());
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        Err(LifecycleError::Protocol(
            "ratatui node server did not become ready within 5 seconds".to_string(),
        ))
    }
}

#[derive(Debug)]
struct RatatuiServerListEntry {
    port: u16,
    client_count: usize,
    uptime_secs: u64,
    session_count: usize,
}

fn list_ratatui_servers() -> Vec<RatatuiServerListEntry> {
    let mut servers = Vec::new();
    let Ok(entries) = std::fs::read_dir(ratatui_socket_dir()) else {
        return servers;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.ends_with(".sock") {
            continue;
        }
        let port_str = name.trim_end_matches(".sock");
        let Ok(port) = port_str.parse::<u16>() else {
            continue;
        };
        match query_server_status(port) {
            Ok(status) => servers.push(RatatuiServerListEntry {
                port,
                client_count: status.client_count,
                uptime_secs: status.uptime_secs,
                session_count: status.session_count,
            }),
            Err(_) => {
                // Stale socket: remove it.
                crate::infra::best_effort::remove_file(&path);
            }
        }
    }

    servers.sort_by(|a, b| a.port.cmp(&b.port));
    servers
}

fn query_server_status(
    port: u16,
) -> Result<crate::ratatui_node::node_runtime::ServerStatus, LifecycleError> {
    let response = send_node_command(port, "STATUS")?;
    let envelope: ServerMessageJson = serde_json::from_str(&response).map_err(|error| {
        LifecycleError::Protocol(format!(
            "failed to parse status response for port {port}: {error}"
        ))
    })?;
    match envelope {
        ServerMessageJson::Response(response) if response.ok => response
            .data
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| {
                LifecycleError::Protocol(format!("status response missing data for port {port}"))
            }),
        ServerMessageJson::Response(response) => Err(LifecycleError::Protocol(format!(
            "server returned error for port {port}: {}",
            response.message.unwrap_or_default()
        ))),
        ServerMessageJson::Snapshot(_) => Err(LifecycleError::Protocol(format!(
            "unexpected snapshot response for port {port}"
        ))),
        ServerMessageJson::History(_) => Err(LifecycleError::Protocol(format!(
            "unexpected history response for port {port}"
        ))),
    }
}

fn resolve_target_to_port(target: Option<&str>) -> Result<u16, LifecycleError> {
    let servers = list_ratatui_servers();

    match target {
        None => {
            if servers.len() == 1 {
                Ok(servers[0].port)
            } else if servers.is_empty() {
                Err(LifecycleError::Protocol(
                    "no ratatui servers are running".to_string(),
                ))
            } else {
                let mut message =
                    "multiple ratatui servers are running; specify an index:\n".to_string();
                for (index, server) in servers.iter().enumerate() {
                    message.push_str(&format!("  {}: port {}\n", index + 1, server.port));
                }
                Err(LifecycleError::Protocol(message))
            }
        }
        Some(value) => {
            let index: usize = value.parse().map_err(|_| {
                LifecycleError::Protocol(format!(
                    "invalid server index `{value}`; expected a number like 1, 2, 3"
                ))
            })?;
            if index == 0 || index > servers.len() {
                return Err(LifecycleError::Protocol(format!(
                    "server index {index} is out of range; run `waitagent --ratatui ls` to see valid indices"
                )));
            }
            Ok(servers[index - 1].port)
        }
    }
}

fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;

    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}
