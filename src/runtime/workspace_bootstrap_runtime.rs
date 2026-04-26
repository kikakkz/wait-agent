use crate::application::workspace_path_service::WorkspacePathService;
use crate::config::AppConfig;
use crate::domain::workspace_paths::WorkspacePaths;
use crate::lifecycle::LifecycleError;
use crate::terminal::TerminalSize;
use std::fs;
use std::io;
use std::os::raw::c_int;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

extern "C" {
    fn setsid() -> c_int;
}

#[derive(Debug, Clone)]
pub struct WorkspaceBootstrapRuntime {
    path_service: WorkspacePathService,
}

impl Default for WorkspaceBootstrapRuntime {
    fn default() -> Self {
        Self::new(WorkspacePathService::new())
    }
}

impl WorkspaceBootstrapRuntime {
    pub fn new(path_service: WorkspacePathService) -> Self {
        Self { path_service }
    }

    pub fn resolve_workspace_dir(&self, value: Option<&str>) -> Result<PathBuf, LifecycleError> {
        self.path_service
            .resolve_workspace_dir(value)
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to canonicalize workspace directory".to_string(),
                    error,
                )
            })
    }

    pub fn workspace_paths(&self, workspace_dir: &Path) -> WorkspacePaths {
        self.path_service.workspace_paths(workspace_dir)
    }

    pub fn runtime_root_dir(&self) -> PathBuf {
        self.path_service.runtime_root_dir()
    }

    pub fn ensure_daemon_running(
        &self,
        config: &AppConfig,
        node_id: Option<&str>,
        connect: Option<&str>,
        paths: &WorkspacePaths,
        size: TerminalSize,
    ) -> Result<(), LifecycleError> {
        let runtime = config.runtime_for_workspace(node_id, connect);
        if daemon_accepts_connections(paths) {
            return Ok(());
        }

        if paths.socket_path.exists() {
            let _ = fs::remove_file(&paths.socket_path);
        }

        if let Some(parent) = paths.socket_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                LifecycleError::Io(
                    "failed to create waitagent runtime directory".to_string(),
                    error,
                )
            })?;
        }

        let current_exe = std::env::current_exe().map_err(|error| {
            LifecycleError::Io("failed to locate current executable".to_string(), error)
        })?;
        let mut command = Command::new(current_exe);
        command
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(&paths.workspace_dir)
            .arg("--rows")
            .arg(size.rows.to_string())
            .arg("--cols")
            .arg(size.cols.to_string())
            .arg("--pixel-width")
            .arg(size.pixel_width.to_string())
            .arg("--pixel-height")
            .arg(size.pixel_height.to_string())
            .arg("--node-id")
            .arg(&runtime.node.node_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir(&paths.workspace_dir);

        if let Some(access_point) = runtime.network.access_point.as_deref() {
            command.arg("--connect").arg(access_point);
        }

        unsafe {
            command.pre_exec(|| {
                if setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }

        command.spawn().map_err(|error| {
            LifecycleError::Io("failed to launch waitagent daemon".to_string(), error)
        })?;
        Ok(())
    }

    pub fn wait_for_daemon_ready(&self, paths: &WorkspacePaths) -> Result<(), LifecycleError> {
        if !wait_for_existing_daemon_ready(paths, DAEMON_START_TIMEOUT, true) {
            return Err(LifecycleError::Protocol(format!(
                "waitagent daemon did not become ready at {}",
                paths.socket_path.display()
            )));
        }
        Ok(())
    }

    pub fn wait_for_attach_target(&self, paths: &WorkspacePaths) {
        wait_for_existing_daemon_ready(paths, DAEMON_START_TIMEOUT, true);
    }
}

fn daemon_is_reachable(paths: &WorkspacePaths) -> bool {
    UnixStream::connect(&paths.socket_path).is_ok()
}

fn daemon_accepts_connections(paths: &WorkspacePaths) -> bool {
    daemon_is_reachable(paths)
}

fn wait_for_existing_daemon_ready(
    paths: &WorkspacePaths,
    timeout: Duration,
    create_parent_dir: bool,
) -> bool {
    if create_parent_dir {
        if let Some(parent) = paths.socket_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
    }

    let started_at = Instant::now();
    while started_at.elapsed() <= timeout {
        if daemon_accepts_connections(paths) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}
