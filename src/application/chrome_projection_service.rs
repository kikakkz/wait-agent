use crate::domain::chrome::{ChromeSurfaceSize, FooterViewModel, SidebarViewModel};
use crate::domain::local_runtime::{
    ChromeEvent, ChromeSurface, LocalRuntimeEvent, SessionCatalogEvent,
};
use crate::domain::session_catalog::ManagedSessionRecord;

#[derive(Debug, Default, Clone)]
pub struct ChromeProjectionService {
    state: ChromeProjectionState,
}

impl ChromeProjectionService {
    pub fn apply_event(&mut self, event: &LocalRuntimeEvent) -> ChromeProjectionUpdate {
        match event {
            LocalRuntimeEvent::SessionCatalog(SessionCatalogEvent::SnapshotUpdated {
                active_socket,
                active_session,
                active_target,
                sessions,
            }) => {
                self.state.active_socket = active_socket.clone();
                self.state.active_session = active_session.clone();
                self.state.active_target = active_target.clone();
                self.state.sessions = sessions.clone();
                self.state.ensure_active_target();
                self.ensure_selected_target();
                self.emit_both()
            }
            LocalRuntimeEvent::Chrome(ChromeEvent::SidebarSelectionChanged { session_id }) => {
                self.state.selected_target = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.address.session_id() == session_id)
                    .map(|session| session.address.qualified_target());
                self.emit_sidebar()
            }
            LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
                surface,
                width,
                height,
            }) => match surface {
                ChromeSurface::SidebarPane => {
                    self.state.sidebar_surface = ChromeSurfaceSize::new(*width, *height);
                    self.emit_sidebar()
                }
                ChromeSurface::FooterPane => {
                    self.state.footer_width = *width;
                    self.emit_footer()
                }
                ChromeSurface::FullscreenStatusLine => {
                    self.state.fullscreen_footer_width = *width;
                    self.emit_footer()
                }
            },
            LocalRuntimeEvent::Chrome(ChromeEvent::FullscreenChanged { is_fullscreen }) => {
                self.state.fullscreen = *is_fullscreen;
                self.emit_footer()
            }
        }
    }

    fn ensure_selected_target(&mut self) {
        let selected_is_still_valid = self.state.selected_target.as_ref().map(|target| {
            self.state
                .sessions
                .iter()
                .any(|session| session.address.qualified_target() == *target)
        }) == Some(true);
        if selected_is_still_valid {
            return;
        }

        self.state.selected_target = self
            .state
            .active_session_record()
            .or_else(|| self.state.sessions.first())
            .map(|session| session.address.qualified_target());
    }

    fn emit_both(&self) -> ChromeProjectionUpdate {
        ChromeProjectionUpdate {
            sidebar: self.build_sidebar_view_model(),
            footer: self.build_footer_view_model(),
        }
    }

    fn emit_sidebar(&self) -> ChromeProjectionUpdate {
        ChromeProjectionUpdate {
            sidebar: self.build_sidebar_view_model(),
            footer: None,
        }
    }

    fn emit_footer(&self) -> ChromeProjectionUpdate {
        ChromeProjectionUpdate {
            sidebar: None,
            footer: self.build_footer_view_model(),
        }
    }

    fn build_sidebar_view_model(&self) -> Option<SidebarViewModel> {
        if self.state.sidebar_surface.width == 0 || self.state.sidebar_surface.height == 0 {
            return None;
        }

        Some(SidebarViewModel {
            active_socket: self.state.active_socket.clone(),
            active_session: self.state.active_session.clone(),
            active_target: self.state.active_target.clone(),
            selected_target: self.state.selected_target.clone(),
            sessions: self.state.sessions.clone(),
            surface: self.state.sidebar_surface,
        })
    }

    fn build_footer_view_model(&self) -> Option<FooterViewModel> {
        let width = if self.state.fullscreen {
            self.state
                .fullscreen_footer_width
                .max(self.state.footer_width)
        } else {
            self.state.footer_width
        };
        if width == 0 {
            return None;
        }

        Some(FooterViewModel {
            active_socket: self.state.active_socket.clone(),
            active_session: self.state.active_session.clone(),
            active_target: self.state.active_target.clone(),
            sessions: self.state.sessions.clone(),
            width,
            fullscreen: self.state.fullscreen,
        })
    }
}

#[derive(Debug, Default, Clone)]
struct ChromeProjectionState {
    active_socket: String,
    active_session: String,
    active_target: Option<String>,
    selected_target: Option<String>,
    sessions: Vec<ManagedSessionRecord>,
    sidebar_surface: ChromeSurfaceSize,
    footer_width: usize,
    fullscreen_footer_width: usize,
    fullscreen: bool,
}

impl ChromeProjectionState {
    fn ensure_active_target(&mut self) {
        let active_is_still_valid = self.active_target.as_ref().map(|target| {
            self.sessions
                .iter()
                .any(|session| session.address.qualified_target() == *target)
        }) == Some(true);
        if active_is_still_valid {
            return;
        }

        self.active_target = self
            .sessions
            .iter()
            .find(|session| {
                session.address.server_id() == self.active_socket
                    && session.address.session_id() == self.active_session
            })
            .map(|session| session.address.qualified_target());
    }

    fn active_session_record(&self) -> Option<&ManagedSessionRecord> {
        self.active_target.as_ref().and_then(|target| {
            self.sessions
                .iter()
                .find(|session| session.address.qualified_target() == *target)
        })
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChromeProjectionUpdate {
    pub sidebar: Option<SidebarViewModel>,
    pub footer: Option<FooterViewModel>,
}

#[cfg(test)]
mod tests {
    use super::ChromeProjectionService;
    use crate::domain::local_runtime::{
        ChromeEvent, ChromeSurface, LocalRuntimeEvent, SessionCatalogEvent,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use std::path::PathBuf;

    #[test]
    fn snapshot_event_produces_sidebar_and_footer_models_once_surfaces_are_known() {
        let mut service = ChromeProjectionService::default();
        service.apply_event(&LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface: ChromeSurface::SidebarPane,
            width: 24,
            height: 18,
        }));
        service.apply_event(&LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface: ChromeSurface::FooterPane,
            width: 80,
            height: 1,
        }));

        let update = service.apply_event(&LocalRuntimeEvent::SessionCatalog(
            SessionCatalogEvent::SnapshotUpdated {
                active_socket: "wa-1".to_string(),
                active_session: "sess-1".to_string(),
                active_target: Some("wa-1:sess-1".to_string()),
                sessions: vec![session("wa-1", "sess-1", "bash")],
            },
        ));

        assert_eq!(
            update.sidebar.as_ref().map(|view| view.surface.width),
            Some(24)
        );
        assert_eq!(update.footer.as_ref().map(|view| view.width), Some(80));
        assert_eq!(
            update
                .sidebar
                .as_ref()
                .and_then(|view| view.active_target.clone())
                .as_deref(),
            Some("wa-1:sess-1")
        );
        assert_eq!(
            update
                .sidebar
                .as_ref()
                .and_then(|view| view.selected_target.clone())
                .as_deref(),
            Some("wa-1:sess-1")
        );
    }

    #[test]
    fn sidebar_selection_event_only_updates_sidebar_projection() {
        let mut service = ChromeProjectionService::default();
        service.apply_event(&LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface: ChromeSurface::SidebarPane,
            width: 24,
            height: 18,
        }));
        service.apply_event(&LocalRuntimeEvent::SessionCatalog(
            SessionCatalogEvent::SnapshotUpdated {
                active_socket: "wa-1".to_string(),
                active_session: "sess-1".to_string(),
                active_target: Some("wa-1:sess-1".to_string()),
                sessions: vec![
                    session("wa-1", "sess-1", "bash"),
                    session("wa-2", "sess-2", "codex"),
                ],
            },
        ));

        let update = service.apply_event(&LocalRuntimeEvent::Chrome(
            ChromeEvent::SidebarSelectionChanged {
                session_id: "sess-2".to_string(),
            },
        ));

        assert!(update.footer.is_none());
        assert_eq!(
            update
                .sidebar
                .as_ref()
                .and_then(|view| view.selected_target.clone())
                .as_deref(),
            Some("wa-2:sess-2")
        );
    }

    #[test]
    fn fullscreen_flag_switches_footer_projection_mode() {
        let mut service = ChromeProjectionService::default();
        service.apply_event(&LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface: ChromeSurface::FooterPane,
            width: 80,
            height: 1,
        }));
        service.apply_event(&LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface: ChromeSurface::FullscreenStatusLine,
            width: 120,
            height: 1,
        }));
        service.apply_event(&LocalRuntimeEvent::SessionCatalog(
            SessionCatalogEvent::SnapshotUpdated {
                active_socket: "wa-1".to_string(),
                active_session: "sess-1".to_string(),
                active_target: Some("wa-1:sess-1".to_string()),
                sessions: vec![session("wa-1", "sess-1", "bash")],
            },
        ));

        let update =
            service.apply_event(&LocalRuntimeEvent::Chrome(ChromeEvent::FullscreenChanged {
                is_fullscreen: true,
            }));

        assert_eq!(update.footer.as_ref().map(|view| view.width), Some(120));
        assert_eq!(
            update.footer.as_ref().map(|view| view.fullscreen),
            Some(true)
        );
    }

    #[test]
    fn later_snapshots_update_active_and_selected_targets() {
        let mut service = ChromeProjectionService::default();
        service.apply_event(&LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface: ChromeSurface::SidebarPane,
            width: 24,
            height: 18,
        }));
        service.apply_event(&LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface: ChromeSurface::FooterPane,
            width: 80,
            height: 1,
        }));
        service.apply_event(&LocalRuntimeEvent::SessionCatalog(
            SessionCatalogEvent::SnapshotUpdated {
                active_socket: "wa-1".to_string(),
                active_session: "sess-1".to_string(),
                active_target: Some("wa-1:sess-1".to_string()),
                sessions: vec![
                    session("wa-1", "sess-1", "bash"),
                    session("wa-1", "sess-2", "codex"),
                ],
            },
        ));

        let update = service.apply_event(&LocalRuntimeEvent::SessionCatalog(
            SessionCatalogEvent::SnapshotUpdated {
                active_socket: "wa-1".to_string(),
                active_session: "sess-1".to_string(),
                active_target: Some("wa-1:sess-2".to_string()),
                sessions: vec![
                    session("wa-1", "sess-1", "bash"),
                    session("wa-1", "sess-2", "codex"),
                ],
            },
        ));

        assert_eq!(
            update
                .sidebar
                .as_ref()
                .and_then(|view| view.active_target.clone())
                .as_deref(),
            Some("wa-1:sess-2")
        );
        assert_eq!(
            update
                .footer
                .as_ref()
                .and_then(|view| view.active_target.clone())
                .as_deref(),
            Some("wa-1:sess-2")
        );
    }

    fn session(socket: &str, session: &str, command: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket, session),
            selector: Some(format!("{socket}:{session}")),
            availability: crate::domain::session_catalog::SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some(command.to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }
}
