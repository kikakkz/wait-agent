use crate::application::session_service::SessionService;
use crate::cli::UiPaneCommand;
use crate::domain::local_runtime::ChromeSurface;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxChromeGateway, TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::event_driven_chrome_runtime::EventDrivenChromeRenderUpdate;
use crate::runtime::event_driven_ui_pane_runtime::{
    EventDrivenSidebarInputOutcome, EventDrivenUiPaneRuntime,
};
use std::io;

const WAITAGENT_ACTIVE_TARGET_OPTION: &str = "@waitagent_active_target";

pub struct EventDrivenTmuxPaneRuntime<G> {
    gateway: G,
    session_service: SessionService<G>,
    pane_runtime: EventDrivenUiPaneRuntime,
}

impl EventDrivenTmuxPaneRuntime<EmbeddedTmuxBackend> {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let gateway = EmbeddedTmuxBackend::from_build_env().map_err(tmux_pane_error)?;
        Ok(Self::new(gateway))
    }
}

impl<G> EventDrivenTmuxPaneRuntime<G>
where
    G: TmuxChromeGateway + Clone,
    G::Error: ToString,
{
    pub fn new(gateway: G) -> Self {
        Self {
            session_service: SessionService::new(gateway.clone()),
            gateway,
            pane_runtime: EventDrivenUiPaneRuntime::new(),
        }
    }

    pub fn refresh_sidebar_for_pane(
        &mut self,
        command: &UiPaneCommand,
        pane_target: &str,
    ) -> Result<EventDrivenChromeRenderUpdate, LifecycleError> {
        let (width, height) = self
            .gateway
            .pane_dimensions_on_socket(&command.socket_name, pane_target)
            .map_err(tmux_pane_error)?;
        let mut update =
            self.pane_runtime
                .publish_surface_resize(ChromeSurface::SidebarPane, width, height);
        merge_render_update(&mut update, self.publish_session_snapshot(command)?);
        Ok(update)
    }

    pub fn refresh_footer_for_pane(
        &mut self,
        command: &UiPaneCommand,
        pane_target: &str,
    ) -> Result<EventDrivenChromeRenderUpdate, LifecycleError> {
        let (width, height) = self
            .gateway
            .pane_dimensions_on_socket(&command.socket_name, pane_target)
            .map_err(tmux_pane_error)?;
        let is_fullscreen = self
            .gateway
            .window_zoomed_on_socket(&command.socket_name, pane_target)
            .map_err(tmux_pane_error)?;

        let mut update =
            self.pane_runtime
                .publish_surface_resize(ChromeSurface::FooterPane, width, height);
        merge_render_update(
            &mut update,
            self.pane_runtime
                .publish_surface_resize(ChromeSurface::FullscreenStatusLine, width, 1),
        );
        merge_render_update(
            &mut update,
            self.pane_runtime.publish_fullscreen_changed(is_fullscreen),
        );
        merge_render_update(&mut update, self.publish_session_snapshot(command)?);
        Ok(update)
    }

    pub fn apply_sidebar_input(&mut self, bytes: &[u8]) -> EventDrivenSidebarInputOutcome {
        self.pane_runtime.apply_sidebar_input_bytes(bytes)
    }

    pub fn request_fullscreen_status_render(&mut self) -> EventDrivenChromeRenderUpdate {
        self.pane_runtime.request_render(
            ChromeSurface::FullscreenStatusLine,
            "tmux status line refresh",
        )
    }

    pub fn selected_target(&self) -> Option<String> {
        self.pane_runtime.selected_target()
    }

    fn publish_session_snapshot(
        &mut self,
        command: &UiPaneCommand,
    ) -> Result<EventDrivenChromeRenderUpdate, LifecycleError> {
        let sessions = self
            .session_service
            .list_sessions()
            .map_err(tmux_pane_error)?
            .into_iter()
            .filter(|session| session.address.server_id() == command.socket_name)
            .collect::<Vec<_>>();
        let active_target = self
            .gateway
            .show_session_option(&workspace_handle(command), WAITAGENT_ACTIVE_TARGET_OPTION)
            .map_err(tmux_pane_error)?;
        let visible_sessions = visible_target_sessions(&sessions, &command.session_name);
        Ok(self.pane_runtime.publish_session_snapshot(
            &command.socket_name,
            &command.session_name,
            active_target.as_deref(),
            visible_sessions,
        ))
    }
}

fn visible_target_sessions(
    sessions: &[ManagedSessionRecord],
    workspace_session_name: &str,
) -> Vec<ManagedSessionRecord> {
    let target_hosts = sessions
        .iter()
        .filter(|session| session.is_target_host())
        .cloned()
        .collect::<Vec<_>>();
    if !target_hosts.is_empty() {
        return target_hosts;
    }

    sessions
        .iter()
        .filter(|session| session.address.session_id() == workspace_session_name)
        .cloned()
        .collect()
}

fn workspace_handle(command: &UiPaneCommand) -> TmuxWorkspaceHandle {
    TmuxWorkspaceHandle {
        workspace_id: WorkspaceInstanceId::new(command.session_name.clone()),
        socket_name: TmuxSocketName::new(command.socket_name.clone()),
        session_name: TmuxSessionName::new(command.session_name.clone()),
    }
}

fn merge_render_update(
    update: &mut EventDrivenChromeRenderUpdate,
    next: EventDrivenChromeRenderUpdate,
) {
    if next.sidebar.is_some() {
        update.sidebar = next.sidebar;
    }
    if next.footer.is_some() {
        update.footer = next.footer;
    }
    if next.fullscreen_status.is_some() {
        update.fullscreen_status = next.fullscreen_status;
    }
}

fn tmux_pane_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to publish tmux pane state into the event-driven runtime".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{EventDrivenTmuxPaneRuntime, WAITAGENT_ACTIVE_TARGET_OPTION};
    use crate::cli::UiPaneCommand;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::domain::workspace::{
        WorkspaceInstanceConfig, WorkspaceInstanceId, WorkspaceSessionRole,
    };
    use crate::infra::tmux::{
        TmuxChromeGateway, TmuxGateway, TmuxPaneId, TmuxSessionGateway, TmuxWindowHandle,
        TmuxWorkspaceHandle,
    };
    use std::path::PathBuf;

    #[derive(Debug, Clone)]
    struct FakeGateway {
        sessions: Vec<ManagedSessionRecord>,
        pane_size: (usize, usize),
        zoomed: bool,
        active_target: Option<String>,
    }

    impl TmuxGateway for FakeGateway {
        type Error = &'static str;

        fn ensure_workspace(
            &self,
            _config: &WorkspaceInstanceConfig,
        ) -> Result<TmuxWorkspaceHandle, Self::Error> {
            unreachable!("not used")
        }

        fn create_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window_name: &str,
        ) -> Result<TmuxWindowHandle, Self::Error> {
            unreachable!("not used")
        }

        fn split_pane_right(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
            _width_percent: u8,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used")
        }

        fn split_pane_bottom(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
            _height_percent: u8,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used")
        }

        fn select_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn select_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn toggle_zoom(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn enter_copy_mode(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }
    }

    impl TmuxSessionGateway for FakeGateway {
        fn list_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            Ok(self.sessions.clone())
        }

        fn find_session(&self, target: &str) -> Result<Option<ManagedSessionRecord>, Self::Error> {
            Ok(self
                .sessions
                .iter()
                .find(|session| session.matches_target(target))
                .cloned())
        }

        fn attach_workspace(&self, _workspace: &TmuxWorkspaceHandle) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn attach_session(&self, _address: &ManagedSessionAddress) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn detach_workspace_clients(
            &self,
            _workspace: &TmuxWorkspaceHandle,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn detach_session_clients(
            &self,
            _address: &ManagedSessionAddress,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn detach_current_client(&self) -> Result<(), Self::Error> {
            unreachable!("not used")
        }
    }

    impl TmuxChromeGateway for FakeGateway {
        fn pane_dimensions_on_socket(
            &self,
            _socket_name: &str,
            _pane_target: &str,
        ) -> Result<(usize, usize), Self::Error> {
            Ok(self.pane_size)
        }

        fn window_zoomed_on_socket(
            &self,
            _socket_name: &str,
            _pane_target: &str,
        ) -> Result<bool, Self::Error> {
            Ok(self.zoomed)
        }

        fn show_session_option(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            option_name: &str,
        ) -> Result<Option<String>, Self::Error> {
            assert_eq!(option_name, WAITAGENT_ACTIVE_TARGET_OPTION);
            Ok(self.active_target.clone())
        }
    }

    #[test]
    fn sidebar_refresh_publishes_real_tmux_surface_and_session_snapshot() {
        let mut runtime = EventDrivenTmuxPaneRuntime::new(FakeGateway {
            sessions: vec![session("wa-1", "sess-1", "bash")],
            pane_size: (28, 9),
            zoomed: false,
            active_target: None,
        });

        let update = runtime
            .refresh_sidebar_for_pane(&command(), "%11")
            .expect("sidebar refresh should succeed");

        assert!(update
            .sidebar
            .as_ref()
            .map(|buffer| buffer.contains("Sessions"))
            .unwrap_or(false));
        assert_eq!(runtime.selected_target().as_deref(), Some("wa-1:sess-1"));
    }

    #[test]
    fn footer_refresh_publishes_fullscreen_state_from_tmux() {
        let mut runtime = EventDrivenTmuxPaneRuntime::new(FakeGateway {
            sessions: vec![session("wa-1", "sess-1", "bash")],
            pane_size: (96, 1),
            zoomed: true,
            active_target: None,
        });

        let update = runtime
            .refresh_footer_for_pane(&command(), "%12")
            .expect("footer refresh should succeed");

        assert!(update
            .footer
            .as_ref()
            .map(|buffer| buffer.contains("keys: ^W cmd"))
            .unwrap_or(false));
        assert!(update
            .fullscreen_status
            .as_ref()
            .map(|buffer| buffer.contains("[Ctrl-o] full off"))
            .unwrap_or(false));
    }

    #[test]
    fn sidebar_input_acts_on_state_loaded_from_tmux_refresh() {
        let mut runtime = EventDrivenTmuxPaneRuntime::new(FakeGateway {
            sessions: vec![
                session_with_role("wa-1", "sess-1", "bash", WorkspaceSessionRole::TargetHost),
                session_with_role("wa-1", "sess-2", "codex", WorkspaceSessionRole::TargetHost),
            ],
            pane_size: (28, 9),
            zoomed: false,
            active_target: Some("wa-1:sess-1".to_string()),
        });
        runtime
            .refresh_sidebar_for_pane(&command(), "%11")
            .expect("sidebar refresh should succeed");

        let outcome = runtime.apply_sidebar_input(b"\x1b[B\r");

        assert!(outcome
            .render
            .sidebar
            .as_ref()
            .map(|buffer| buffer.contains("> codex@local"))
            .unwrap_or(false));
        assert_eq!(runtime.selected_target().as_deref(), Some("wa-1:sess-2"));
    }

    #[test]
    fn snapshot_prefers_target_hosts_and_workspace_active_target() {
        let mut runtime = EventDrivenTmuxPaneRuntime::new(FakeGateway {
            sessions: vec![
                session_with_role(
                    "wa-1",
                    "workspace",
                    "bash",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session_with_role(
                    "wa-1",
                    "target-a",
                    "codex",
                    WorkspaceSessionRole::TargetHost,
                ),
                session_with_role("wa-1", "target-b", "bash", WorkspaceSessionRole::TargetHost),
            ],
            pane_size: (28, 9),
            zoomed: false,
            active_target: Some("wa-1:target-b".to_string()),
        });

        let update = runtime
            .refresh_sidebar_for_pane(
                &UiPaneCommand {
                    socket_name: "wa-1".to_string(),
                    session_name: "workspace".to_string(),
                },
                "%11",
            )
            .expect("sidebar refresh should succeed");

        let buffer = update.sidebar.expect("sidebar buffer should render");
        assert!(!buffer.contains("workspace@local"));
        assert!(buffer.contains("> bash@local"));
        assert_eq!(runtime.selected_target().as_deref(), Some("wa-1:target-b"));
    }

    fn command() -> UiPaneCommand {
        UiPaneCommand {
            socket_name: "wa-1".to_string(),
            session_name: "sess-1".to_string(),
        }
    }

    fn session(socket: &str, session: &str, command: &str) -> ManagedSessionRecord {
        session_with_role(
            socket,
            session,
            command,
            WorkspaceSessionRole::WorkspaceChrome,
        )
    }

    fn session_with_role(
        socket: &str,
        session: &str,
        command: &str,
        session_role: WorkspaceSessionRole,
    ) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket, session),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some(WorkspaceInstanceId::new(session).as_str().to_string()),
            session_role: Some(session_role),
            attached_clients: 1,
            window_count: 1,
            command_name: Some(command.to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }
}
