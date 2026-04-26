use crate::application::workspace_path_service::WorkspacePathService;
use crate::domain::workspace_status::DaemonStatusRecord;
use crate::lifecycle::LifecycleError;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceDaemonRequest {
    Status,
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceDaemonResponse {
    Running(String),
    NotRunning,
}

pub trait WorkspaceDaemonGateway {
    fn list_socket_paths(&self, runtime_root_dir: &Path) -> Result<Vec<PathBuf>, LifecycleError>;

    fn request(
        &self,
        socket_path: &Path,
        request: WorkspaceDaemonRequest,
    ) -> Result<WorkspaceDaemonResponse, LifecycleError>;
}

pub struct WorkspaceDaemonService<G> {
    gateway: G,
    path_service: WorkspacePathService,
}

impl<G> WorkspaceDaemonService<G>
where
    G: WorkspaceDaemonGateway,
{
    pub fn new(gateway: G, path_service: WorkspacePathService) -> Self {
        Self {
            gateway,
            path_service,
        }
    }

    pub fn list_daemons(&self) -> Result<Vec<DaemonStatusRecord>, LifecycleError> {
        let runtime_root_dir = self.path_service.runtime_root_dir();
        let mut daemons = Vec::new();
        for socket_path in self.gateway.list_socket_paths(&runtime_root_dir)? {
            let WorkspaceDaemonResponse::Running(status) = self
                .gateway
                .request(&socket_path, WorkspaceDaemonRequest::Status)?
            else {
                continue;
            };

            if let Some(record) = DaemonStatusRecord::parse(&status, socket_path) {
                daemons.push(record);
            }
        }

        daemons.sort_by(|left, right| left.workspace_dir.cmp(&right.workspace_dir));
        Ok(daemons)
    }

    pub fn status_text_for_workspace(
        &self,
        workspace_dir: &Path,
    ) -> Result<String, LifecycleError> {
        self.request_text_for_workspace(workspace_dir, WorkspaceDaemonRequest::Status)
    }

    pub fn detach_text_for_workspace(
        &self,
        workspace_dir: &Path,
    ) -> Result<String, LifecycleError> {
        self.request_text_for_workspace(workspace_dir, WorkspaceDaemonRequest::Detach)
    }

    fn request_text_for_workspace(
        &self,
        workspace_dir: &Path,
        request: WorkspaceDaemonRequest,
    ) -> Result<String, LifecycleError> {
        let paths = self.path_service.workspace_paths(workspace_dir);
        match self.gateway.request(&paths.socket_path, request)? {
            WorkspaceDaemonResponse::Running(text) => Ok(text),
            WorkspaceDaemonResponse::NotRunning => Ok(format!(
                "waitagent daemon not running for {}\nsocket: {}",
                workspace_dir.display(),
                paths.socket_path.display()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        WorkspaceDaemonGateway, WorkspaceDaemonRequest, WorkspaceDaemonResponse,
        WorkspaceDaemonService,
    };
    use crate::application::workspace_path_service::WorkspacePathService;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    #[derive(Default)]
    struct FakeWorkspaceDaemonGateway {
        socket_paths: Vec<PathBuf>,
        responses: HashMap<(PathBuf, WorkspaceDaemonRequest), WorkspaceDaemonResponse>,
        requests: RefCell<Vec<(PathBuf, WorkspaceDaemonRequest)>>,
    }

    impl WorkspaceDaemonGateway for FakeWorkspaceDaemonGateway {
        fn list_socket_paths(
            &self,
            _runtime_root_dir: &Path,
        ) -> Result<Vec<PathBuf>, crate::lifecycle::LifecycleError> {
            Ok(self.socket_paths.clone())
        }

        fn request(
            &self,
            socket_path: &Path,
            request: WorkspaceDaemonRequest,
        ) -> Result<WorkspaceDaemonResponse, crate::lifecycle::LifecycleError> {
            self.requests
                .borrow_mut()
                .push((socket_path.to_path_buf(), request));
            Ok(self
                .responses
                .get(&(socket_path.to_path_buf(), request))
                .cloned()
                .unwrap_or(WorkspaceDaemonResponse::NotRunning))
        }
    }

    #[test]
    fn list_daemons_skips_missing_sockets_and_sorts_by_workspace() {
        let left_socket = PathBuf::from("/tmp/runtime/left.sock");
        let right_socket = PathBuf::from("/tmp/runtime/right.sock");
        let gateway = FakeWorkspaceDaemonGateway {
            socket_paths: vec![right_socket.clone(), left_socket.clone()],
            responses: HashMap::from([
                (
                    (left_socket.clone(), WorkspaceDaemonRequest::Status),
                    WorkspaceDaemonResponse::Running(
                        "workspace: /tmp/a\nnode: local\nchild_pid: 1\nready: yes\nattached_clients: 1\nscreen_size: 24x80"
                            .to_string(),
                    ),
                ),
                (
                    (right_socket.clone(), WorkspaceDaemonRequest::Status),
                    WorkspaceDaemonResponse::NotRunning,
                ),
            ]),
            requests: RefCell::new(Vec::new()),
        };
        let service = WorkspaceDaemonService::new(gateway, WorkspacePathService::new());

        let daemons = service.list_daemons().expect("list should succeed");

        assert_eq!(daemons.len(), 1);
        assert_eq!(daemons[0].workspace_dir, Path::new("/tmp/a"));
    }

    #[test]
    fn status_text_formats_not_running_message_from_workspace_dir() {
        let workspace_dir = Path::new("/tmp/status-demo");
        let socket_path = WorkspacePathService::new()
            .workspace_paths(workspace_dir)
            .socket_path;
        let gateway = FakeWorkspaceDaemonGateway {
            responses: HashMap::from([(
                (socket_path, WorkspaceDaemonRequest::Status),
                WorkspaceDaemonResponse::NotRunning,
            )]),
            ..FakeWorkspaceDaemonGateway::default()
        };
        let service = WorkspaceDaemonService::new(gateway, WorkspacePathService::new());

        let text = service
            .status_text_for_workspace(workspace_dir)
            .expect("status should succeed");

        assert!(text.contains("waitagent daemon not running"));
        assert!(text.contains("/tmp/status-demo"));
    }
}
