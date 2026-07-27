use crate::cli::RemoteNetworkConfig;
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::ratatui_client_runtime::RatatuiClientRuntime;
use crate::runtime::ratatui_node_runtime::{
    node_is_running, ratatui_socket_dir, ratatui_socket_path, read_socket_info, remove_node_socket,
};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SESSION_NAME: &str = "1";

/// Top-level entry point for `waitagent --ratatui` and its session subcommands.
///
/// Sessions are global per user (scoped by UID, like tmux). The default session
/// name is `"1"`, matching tmux-style numbered sessions.
pub struct RatatuiWorkspaceRuntime {
    network: RemoteNetworkConfig,
}

impl RatatuiWorkspaceRuntime {
    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self { network })
    }

    /// Default entry: attach to (or create) the default session "1" and run the TUI.
    pub fn run_workspace_entry(&self) -> Result<(), LifecycleError> {
        self.attach_or_create(DEFAULT_SESSION_NAME)
    }

    /// Attach to a named session, creating it if it does not exist, and run the TUI.
    pub fn attach(&self, session_name: impl AsRef<str>) -> Result<(), LifecycleError> {
        self.attach_or_create(session_name.as_ref())
    }

    /// Detach all clients from a session.
    pub fn detach(&self, session_name: impl AsRef<str>) -> Result<(), LifecycleError> {
        let session_name = session_name.as_ref();
        ERROR_LOG.log(format!(
            "[ratatui-workspace] detach session={}",
            session_name
        ));

        if !node_is_running(session_name) {
            return Err(LifecycleError::Protocol(format!(
                "ratatui session `{session_name}` is not running"
            )));
        }

        let socket_path = ratatui_socket_path(session_name);
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

        println!("detached clients from ratatui session `{session_name}`");
        Ok(())
    }

    /// Stop a session's node server.
    pub fn stop(&self, session_name: impl AsRef<str>) -> Result<(), LifecycleError> {
        let session_name = session_name.as_ref();
        ERROR_LOG.log(format!("[ratatui-workspace] stop session={}", session_name));

        let info = read_socket_info(session_name).ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "ratatui session `{session_name}` is not running or info file is missing"
            ))
        })?;

        // Gracefully terminate the node server process.
        unsafe {
            libc::kill(info.pid as i32, libc::SIGTERM);
        }

        // Wait briefly for the process to exit and the socket to disappear.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !node_is_running(session_name) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        remove_node_socket(session_name);
        println!("stopped ratatui session `{session_name}`");
        Ok(())
    }

    /// List ratatui sessions for the current user.
    pub fn list(&self) -> Result<(), LifecycleError> {
        let sessions = list_ratatui_sessions();
        if sessions.is_empty() {
            println!("no ratatui sessions running");
            return Ok(());
        }

        for session in sessions {
            let status = if session.client_count > 0 {
                "attached"
            } else {
                "detached"
            };
            println!(
                "{}: {}, up {}",
                session.session_name,
                status,
                format_uptime(session.created_at_unix_secs),
            );
        }
        Ok(())
    }

    fn attach_or_create(&self, session_name: &str) -> Result<(), LifecycleError> {
        ERROR_LOG.log(format!(
            "[ratatui-workspace] entry session={}",
            session_name
        ));

        if !node_is_running(session_name) {
            self.start_node_server(session_name)?;
        } else {
            ERROR_LOG.log(format!(
                "[ratatui-workspace] existing node found for session {session_name}"
            ));
        }

        RatatuiClientRuntime::from_session(session_name.to_string())?.run()
    }

    fn start_node_server(&self, session_name: &str) -> Result<(), LifecycleError> {
        let executable = current_waitagent_executable()?;
        let socket_path = ratatui_socket_path(session_name);

        // Remove any stale socket left by a previous crash.
        remove_node_socket(session_name);

        ERROR_LOG.log(format!(
            "[ratatui-workspace] spawning node server executable={:?} socket={} session={}",
            executable,
            socket_path.display(),
            session_name
        ));

        let mut command = Command::new(&executable);

        // Inherit global network flags first so the CLI parser consumes them
        // before the subcommand.
        for arg in self.network.to_cli_args() {
            command.arg(arg);
        }

        command
            .arg("__ratatui-node-server")
            .arg("--session-name")
            .arg(session_name)
            .arg("--listener-display")
            .arg(self.network.advertised_listener_label())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(connect) = &self.network.connect {
            command.arg("--connect-endpoint").arg(connect);
        }

        // Detach the child into its own session so it survives client exit.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let _child = command.spawn().map_err(|error| {
            LifecycleError::Io("failed to spawn ratatui node server".to_string(), error)
        })?;

        // Wait briefly for the server socket to appear.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket_path.exists() && node_is_running(session_name) {
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
struct RatatuiSessionListEntry {
    session_name: String,
    client_count: usize,
    created_at_unix_secs: Option<u64>,
}

fn list_ratatui_sessions() -> Vec<RatatuiSessionListEntry> {
    let mut sessions = Vec::new();
    let Ok(entries) = std::fs::read_dir(ratatui_socket_dir()) else {
        return sessions;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.ends_with(".sock.info") {
            continue;
        }
        if let Ok(json) = std::fs::read_to_string(&path) {
            if let Ok(info) = serde_json::from_str::<
                crate::runtime::ratatui_node_runtime::RatatuiSocketInfo,
            >(&json)
            {
                sessions.push(RatatuiSessionListEntry {
                    session_name: info.session_name,
                    client_count: info.client_count,
                    created_at_unix_secs: Some(info.created_at_unix_secs),
                });
            }
        }
    }

    sessions.sort_by(|a, b| a.session_name.cmp(&b.session_name));
    sessions
}

fn format_uptime(created_at_unix_secs: Option<u64>) -> String {
    let Some(created_at_unix_secs) = created_at_unix_secs else {
        return "-".to_string();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(created_at_unix_secs);
    let elapsed = now.saturating_sub(created_at_unix_secs);
    format_duration(elapsed)
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
