use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::tmux::{TmuxSessionGateway, TmuxWorkspaceHandle};

pub struct SessionService<G> {
    gateway: G,
}

impl<G> SessionService<G>
where
    G: TmuxSessionGateway,
{
    pub fn new(gateway: G) -> Self {
        Self { gateway }
    }

    pub fn list_sessions(&self) -> Result<Vec<ManagedSessionRecord>, G::Error> {
        self.gateway.list_sessions()
    }

    pub fn find_session(&self, target: &str) -> Result<Option<ManagedSessionRecord>, G::Error> {
        self.gateway.find_session(target)
    }

    pub fn attach_workspace(&self, workspace: &TmuxWorkspaceHandle) -> Result<(), G::Error> {
        self.gateway.attach_workspace(workspace)
    }

    pub fn attach_session(&self, session: &ManagedSessionRecord) -> Result<(), G::Error> {
        self.gateway.attach_session(&session.address)
    }

    pub fn detach_workspace_clients(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), G::Error> {
        self.gateway.detach_workspace_clients(workspace)
    }

    pub fn detach_session_clients(&self, session: &ManagedSessionRecord) -> Result<(), G::Error> {
        self.gateway.detach_session_clients(&session.address)
    }

    pub fn detach_current_client(&self) -> Result<(), G::Error> {
        self.gateway.detach_current_client()
    }
}

#[cfg(test)]
mod tests {
    use super::SessionService;
    use crate::domain::session_catalog::{ManagedSessionAddress, ManagedSessionRecord};
    use crate::domain::workspace::WorkspaceInstanceId;
    use crate::infra::tmux::{
        TmuxGateway, TmuxPaneId, TmuxSessionGateway, TmuxSessionName, TmuxSocketName,
        TmuxWindowHandle, TmuxWorkspaceHandle,
    };
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        AttachWorkspace(String),
        AttachSession(String),
        DetachWorkspace(String),
        DetachSession(String),
        DetachCurrentClient,
    }

    #[derive(Debug, Clone)]
    struct FakeGateway {
        calls: Rc<RefCell<Vec<Call>>>,
    }

    impl FakeGateway {
        fn new() -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }
    }

    impl TmuxGateway for FakeGateway {
        type Error = &'static str;

        fn ensure_workspace(
            &self,
            _config: &crate::domain::workspace::WorkspaceInstanceConfig,
        ) -> Result<TmuxWorkspaceHandle, Self::Error> {
            unreachable!("workspace bootstrap is not exercised in this test")
        }

        fn create_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window_name: &str,
        ) -> Result<TmuxWindowHandle, Self::Error> {
            unreachable!("not used in this test")
        }

        fn split_pane_right(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
            _width_percent: u8,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used in this test")
        }

        fn split_pane_bottom(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
            _height_percent: u8,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used in this test")
        }

        fn select_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
        ) -> Result<(), Self::Error> {
            unreachable!("not used in this test")
        }

        fn select_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used in this test")
        }

        fn toggle_zoom(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used in this test")
        }

        fn enter_copy_mode(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used in this test")
        }
    }

    impl TmuxSessionGateway for FakeGateway {
        fn list_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            Ok(vec![ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1234", "waitagent-1234"),
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: Some("1234".to_string()),
                attached_clients: 1,
            }])
        }

        fn find_session(&self, target: &str) -> Result<Option<ManagedSessionRecord>, Self::Error> {
            let record = self
                .list_sessions()?
                .into_iter()
                .find(|record| record.matches_target(target));
            Ok(record)
        }

        fn attach_workspace(&self, workspace: &TmuxWorkspaceHandle) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::AttachWorkspace(
                workspace.session_name.as_str().to_string(),
            ));
            Ok(())
        }

        fn attach_session(&self, address: &ManagedSessionAddress) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::AttachSession(address.qualified_target()));
            Ok(())
        }

        fn detach_workspace_clients(
            &self,
            workspace: &TmuxWorkspaceHandle,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::DetachWorkspace(
                workspace.session_name.as_str().to_string(),
            ));
            Ok(())
        }

        fn detach_session_clients(
            &self,
            address: &ManagedSessionAddress,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::DetachSession(address.qualified_target()));
            Ok(())
        }

        fn detach_current_client(&self) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::DetachCurrentClient);
            Ok(())
        }
    }

    fn workspace_handle() -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("1234"),
            socket_name: TmuxSocketName::new("wa-1234"),
            session_name: TmuxSessionName::new("waitagent-1234"),
        }
    }

    #[test]
    fn session_service_delegates_native_attach_and_detach_calls() {
        let gateway = FakeGateway::new();
        let service = SessionService::new(gateway.clone());
        let workspace = workspace_handle();
        let session = service
            .find_session("waitagent-1234")
            .expect("session lookup should succeed")
            .expect("session should exist");

        service
            .attach_workspace(&workspace)
            .expect("workspace attach should succeed");
        service
            .attach_session(&session)
            .expect("session attach should succeed");
        service
            .detach_workspace_clients(&workspace)
            .expect("workspace detach should succeed");
        service
            .detach_session_clients(&session)
            .expect("session detach should succeed");
        service
            .detach_current_client()
            .expect("current client detach should succeed");

        assert_eq!(
            gateway.calls(),
            vec![
                Call::AttachWorkspace("waitagent-1234".to_string()),
                Call::AttachSession("wa-1234:waitagent-1234".to_string()),
                Call::DetachWorkspace("waitagent-1234".to_string()),
                Call::DetachSession("wa-1234:waitagent-1234".to_string()),
                Call::DetachCurrentClient,
            ]
        );
    }
}
