use crate::domain::chrome::{FooterViewModel, SidebarViewModel};
use crate::ui::footer::FooterUi;
use crate::ui::sidebar::SidebarUi;

#[derive(Debug, Default, Clone)]
pub struct EventDrivenChromeRuntime {
    last_sidebar_buffer: Option<String>,
    last_footer_buffer: Option<String>,
    last_fullscreen_footer_buffer: Option<String>,
}

impl EventDrivenChromeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn render(
        &mut self,
        sidebar: Option<SidebarViewModel>,
        footer: Option<FooterViewModel>,
        invalidate_chrome: bool,
    ) -> EventDrivenChromeRenderUpdate {
        if invalidate_chrome {
            self.last_sidebar_buffer = None;
            self.last_footer_buffer = None;
            self.last_fullscreen_footer_buffer = None;
        }

        let mut update = EventDrivenChromeRenderUpdate {
            invalidate_sidebar: invalidate_chrome,
            invalidate_footer: invalidate_chrome,
            ..Default::default()
        };

        if let Some(model) = sidebar {
            let buffer = SidebarUi::render_view_model(&model);
            if self.last_sidebar_buffer.as_ref() != Some(&buffer) {
                self.last_sidebar_buffer = Some(buffer.clone());
                update.sidebar = Some(buffer);
            }
        }

        if let Some(model) = footer {
            let pane_buffer = FooterUi::render(
                &model.active_socket,
                &model.active_session,
                model.active_target.as_deref(),
                &model.sessions,
                model.width,
                model.listener_display.as_deref(),
                model.connect_endpoint.as_deref(),
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
                fullscreen_width,
                model.listener_display.as_deref(),
                model.connect_endpoint.as_deref(),
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
    use crate::domain::chrome::{ChromeSurfaceSize, FooterViewModel, SidebarViewModel};
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use std::path::PathBuf;

    #[test]
    fn view_models_produce_sidebar_and_footer_buffers() {
        let mut runtime = EventDrivenChromeRuntime::new();

        let update = runtime.render(Some(sidebar_model()), Some(footer_model(false, 80)), false);

        assert!(update
            .sidebar
            .as_ref()
            .map(|buffer| buffer.contains("Sessions"))
            .unwrap_or(false));
        assert!(update
            .footer
            .as_ref()
            .map(|buffer| {
                buffer.contains("Ctrl-N") && buffer.contains("Ctrl-W") && buffer.contains("Ctrl-S")
            })
            .unwrap_or(false));
        assert!(update
            .fullscreen_status
            .as_ref()
            .map(|buffer| { buffer.contains("Ctrl-N") && buffer.contains("Close") })
            .unwrap_or(false));
    }

    #[test]
    fn repeated_equivalent_view_models_do_not_emit_duplicate_buffers() {
        let mut runtime = EventDrivenChromeRuntime::new();

        let first = runtime.render(None, Some(footer_model(false, 80)), false);
        let second = runtime.render(None, Some(footer_model(false, 80)), false);

        assert!(first.footer.is_some());
        assert!(second.footer.is_none());
    }

    #[test]
    fn fullscreen_invalidation_only_happens_on_state_transition() {
        let mut runtime = EventDrivenChromeRuntime::new();

        let first = runtime.render(None, Some(footer_model(true, 120)), true);
        let second = runtime.render(None, Some(footer_model(true, 120)), false);

        assert!(first.invalidate_footer);
        assert!(first.footer.is_some());
        assert!(!second.invalidate_footer);
    }

    fn sidebar_model() -> SidebarViewModel {
        SidebarViewModel {
            active_socket: "wa-1".to_string(),
            active_session: "sess-1".to_string(),
            active_target: Some("wa-1:sess-1".to_string()),
            selected_target: Some("wa-1:sess-1".to_string()),
            sessions: vec![session("wa-1", "sess-1", "bash")],
            surface: ChromeSurfaceSize::new(24, 18),
        }
    }

    fn footer_model(fullscreen: bool, width: usize) -> FooterViewModel {
        FooterViewModel {
            active_socket: "wa-1".to_string(),
            active_session: "sess-1".to_string(),
            active_target: Some("wa-1:sess-1".to_string()),
            sessions: vec![session("wa-1", "sess-1", "bash")],
            listener_display: Some("192.168.1.22:7474".to_string()),
            connect_endpoint: None,
            width,
            fullscreen,
        }
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
