use crate::application::chrome_projection_service::ChromeProjectionService;
use crate::domain::local_runtime::{ChromeEvent, LocalRuntimeEvent};
use crate::ui::footer::FooterUi;
use crate::ui::sidebar::SidebarUi;

#[derive(Debug, Default, Clone)]
pub struct EventDrivenChromeRuntime {
    projection_service: ChromeProjectionService,
    last_sidebar_buffer: Option<String>,
    last_footer_buffer: Option<String>,
    last_fullscreen_footer_buffer: Option<String>,
    last_fullscreen_state: Option<bool>,
}

impl EventDrivenChromeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_event(
        &mut self,
        event: &LocalRuntimeEvent,
        now_millis: u128,
    ) -> EventDrivenChromeRenderUpdate {
        let fullscreen_changed = match event {
            LocalRuntimeEvent::Chrome(ChromeEvent::FullscreenChanged { is_fullscreen }) => {
                let changed = self.last_fullscreen_state != Some(*is_fullscreen);
                self.last_fullscreen_state = Some(*is_fullscreen);
                changed
            }
            _ => false,
        };

        if fullscreen_changed {
            self.last_sidebar_buffer = None;
            self.last_footer_buffer = None;
            self.last_fullscreen_footer_buffer = None;
        }

        let projection = self.projection_service.apply_event(event);
        let mut update = EventDrivenChromeRenderUpdate {
            invalidate_sidebar: fullscreen_changed,
            invalidate_footer: fullscreen_changed,
            ..Default::default()
        };

        if let Some(model) = projection.sidebar {
            let buffer = SidebarUi::render_view_model(&model, now_millis);
            if self.last_sidebar_buffer.as_ref() != Some(&buffer) {
                self.last_sidebar_buffer = Some(buffer.clone());
                update.sidebar = Some(buffer);
            }
        }

        if let Some(model) = projection.footer {
            let pane_buffer = FooterUi::render(
                &model.active_socket,
                &model.active_session,
                model.active_target.as_deref(),
                &model.sessions,
                model.listener_display.as_deref(),
                model.width,
            );
            if self.last_footer_buffer.as_ref() != Some(&pane_buffer) {
                self.last_footer_buffer = Some(pane_buffer.clone());
                update.footer = Some(pane_buffer);
            }

            let fullscreen_width = model.width.max(120);
            let fullscreen_buffer = FooterUi::render_fullscreen(
                &model.active_socket,
                &model.active_session,
                model.active_target.as_deref(),
                &model.sessions,
                model.listener_display.as_deref(),
                fullscreen_width,
            );
            if self.last_fullscreen_footer_buffer.as_ref() != Some(&fullscreen_buffer) {
                self.last_fullscreen_footer_buffer = Some(fullscreen_buffer.clone());
                update.fullscreen_status = Some(fullscreen_buffer);
            }
        }

        update
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EventDrivenChromeRenderUpdate {
    pub sidebar: Option<String>,
    pub footer: Option<String>,
    pub fullscreen_status: Option<String>,
    pub invalidate_sidebar: bool,
    pub invalidate_footer: bool,
}

#[cfg(test)]
mod tests {
    use super::EventDrivenChromeRuntime;
    use crate::domain::local_runtime::{
        ChromeEvent, ChromeSurface, LocalRuntimeEvent, SessionCatalogEvent,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use std::path::PathBuf;

    #[test]
    fn snapshot_and_surface_events_produce_sidebar_and_footer_buffers() {
        let mut runtime = EventDrivenChromeRuntime::new();

        runtime.apply_event(
            &LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
                surface: ChromeSurface::SidebarPane,
                width: 24,
                height: 18,
            }),
            0,
        );
        runtime.apply_event(
            &LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
                surface: ChromeSurface::FooterPane,
                width: 80,
                height: 1,
            }),
            0,
        );

        let update = runtime.apply_event(
            &LocalRuntimeEvent::SessionCatalog(SessionCatalogEvent::SnapshotUpdated {
                active_socket: "wa-1".to_string(),
                active_session: "sess-1".to_string(),
                active_target: Some("wa-1:sess-1".to_string()),
                sessions: vec![session("wa-1", "sess-1", "bash")],
                listener_display: Some("192.168.1.22:7474".to_string()),
            }),
            0,
        );

        assert!(update
            .sidebar
            .as_ref()
            .map(|buffer| buffer.contains("Sessions"))
            .unwrap_or(false));
        assert!(update
            .footer
            .as_ref()
            .map(|buffer| {
                buffer.contains("keys: ^N new") && buffer.contains("listen: 192.168.1.22:7474")
            })
            .unwrap_or(false));
        assert!(update
            .fullscreen_status
            .as_ref()
            .map(|buffer| {
                buffer.contains("[Ctrl-n] new") && buffer.contains("listen: 192.168.1.22:7474")
            })
            .unwrap_or(false));
    }

    #[test]
    fn repeated_equivalent_events_do_not_emit_duplicate_buffers() {
        let mut runtime = EventDrivenChromeRuntime::new();
        runtime.apply_event(
            &LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
                surface: ChromeSurface::FooterPane,
                width: 80,
                height: 1,
            }),
            0,
        );

        let event = LocalRuntimeEvent::SessionCatalog(SessionCatalogEvent::SnapshotUpdated {
            active_socket: "wa-1".to_string(),
            active_session: "sess-1".to_string(),
            active_target: Some("wa-1:sess-1".to_string()),
            sessions: vec![session("wa-1", "sess-1", "bash")],
            listener_display: None,
        });
        let first = runtime.apply_event(&event, 0);
        let second = runtime.apply_event(&event, 0);

        assert!(first.footer.is_some());
        assert!(second.footer.is_none());
    }

    #[test]
    fn fullscreen_invalidation_only_happens_on_state_transition() {
        let mut runtime = EventDrivenChromeRuntime::new();
        runtime.apply_event(
            &LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
                surface: ChromeSurface::FooterPane,
                width: 80,
                height: 1,
            }),
            0,
        );
        runtime.apply_event(
            &LocalRuntimeEvent::SessionCatalog(SessionCatalogEvent::SnapshotUpdated {
                active_socket: "wa-1".to_string(),
                active_session: "sess-1".to_string(),
                active_target: Some("wa-1:sess-1".to_string()),
                sessions: vec![session("wa-1", "sess-1", "bash")],
                listener_display: None,
            }),
            0,
        );

        let first = runtime.apply_event(
            &LocalRuntimeEvent::Chrome(ChromeEvent::FullscreenChanged {
                is_fullscreen: true,
            }),
            0,
        );
        let second = runtime.apply_event(
            &LocalRuntimeEvent::Chrome(ChromeEvent::FullscreenChanged {
                is_fullscreen: true,
            }),
            0,
        );

        assert!(first.invalidate_footer);
        assert!(first.footer.is_some());
        assert!(!second.invalidate_footer);
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
