use crate::application::workspace_daemon_service::{
    WorkspaceDaemonGateway, WorkspaceDaemonRequest, WorkspaceDaemonResponse,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::workspace_daemon_protocol::{read_frame, write_frame, Frame};
use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Copy)]
pub struct WorkspaceDaemonClientRuntime;

impl WorkspaceDaemonClientRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl WorkspaceDaemonGateway for WorkspaceDaemonClientRuntime {
    fn list_socket_paths(&self, runtime_root_dir: &Path) -> Result<Vec<PathBuf>, LifecycleError> {
        let mut socket_paths = Vec::new();
        for entry in fs::read_dir(runtime_root_dir).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to read waitagent runtime directory {}",
                    runtime_root_dir.display()
                ),
                error,
            )
        })? {
            let entry = entry.map_err(|error| {
                LifecycleError::Io("failed to read waitagent runtime entry".to_string(), error)
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("sock") {
                socket_paths.push(path);
            }
        }
        Ok(socket_paths)
    }

    fn request(
        &self,
        socket_path: &Path,
        request: WorkspaceDaemonRequest,
    ) -> Result<WorkspaceDaemonResponse, LifecycleError> {
        let mut stream = match UnixStream::connect(socket_path) {
            Ok(stream) => stream,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.kind() == io::ErrorKind::ConnectionRefused =>
            {
                return Ok(WorkspaceDaemonResponse::NotRunning);
            }
            Err(error) => {
                return Err(LifecycleError::Io(
                    format!(
                        "failed to connect to waitagent daemon at {}",
                        socket_path.display()
                    ),
                    error,
                ));
            }
        };

        let outbound = match request {
            WorkspaceDaemonRequest::Status => Frame::StatusRequest,
            WorkspaceDaemonRequest::Detach => Frame::DetachRequest,
        };
        write_frame(&mut stream, &outbound)?;

        match read_frame(&mut stream)? {
            Frame::StatusResponse(text) | Frame::Ack(text) | Frame::Error(text) => {
                Ok(WorkspaceDaemonResponse::Running(text))
            }
            other => Err(LifecycleError::Protocol(format!(
                "unexpected daemon response from {}: {:?}",
                socket_path.display(),
                other
            ))),
        }
    }
}
