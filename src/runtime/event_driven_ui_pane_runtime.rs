use crate::application::local_runtime_event_service::{
    LocalRuntimeEventBus, LocalRuntimeEventPublisher, LocalRuntimeEventSubscriber,
};
use crate::application::session_catalog_projection_service::SessionCatalogProjectionService;
use crate::domain::local_runtime::{
    ChromeEvent, ChromeSurface, LocalRuntimeEvent, SessionCatalogEvent,
};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::event::EventEnvelope;
use crate::runtime::event_driven_chrome_runtime::{
    EventDrivenChromeRenderUpdate, EventDrivenChromeRuntime,
};
use std::sync::mpsc::Receiver;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct EventDrivenUiPaneRuntime {
    bus: LocalRuntimeEventBus,
    event_rx: Receiver<EventEnvelope<LocalRuntimeEvent>>,
    session_catalog_projection: SessionCatalogProjectionService,
    chrome_runtime: EventDrivenChromeRuntime,
    state: EventDrivenUiPaneState,
    pending_sidebar_input: Vec<u8>,
}

impl Default for EventDrivenUiPaneRuntime {
    fn default() -> Self {
        let mut bus = LocalRuntimeEventBus::new();
        let (_subscriber_id, event_rx) = bus.subscribe();
        Self {
            bus,
            event_rx,
            session_catalog_projection: SessionCatalogProjectionService::new(),
            chrome_runtime: EventDrivenChromeRuntime::new(),
            state: EventDrivenUiPaneState::default(),
            pending_sidebar_input: Vec::new(),
        }
    }
}

impl EventDrivenUiPaneRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish_session_snapshot(
        &mut self,
        active_socket: &str,
        active_session: &str,
        active_target: Option<&str>,
        sessions: Vec<ManagedSessionRecord>,
        listener_display: Option<&str>,
    ) -> EventDrivenChromeRenderUpdate {
        self.session_catalog_projection.publish_snapshot(
            &mut self.bus,
            active_socket,
            active_session,
            active_target,
            sessions,
            listener_display,
        );
        self.drain_pending_events(now_millis())
    }

    pub fn publish_surface_resize(
        &mut self,
        surface: ChromeSurface,
        width: usize,
        height: usize,
    ) -> EventDrivenChromeRenderUpdate {
        self.publish(LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface,
            width,
            height,
        }))
    }

    pub fn publish_fullscreen_changed(
        &mut self,
        is_fullscreen: bool,
    ) -> EventDrivenChromeRenderUpdate {
        self.publish(LocalRuntimeEvent::Chrome(ChromeEvent::FullscreenChanged {
            is_fullscreen,
        }))
    }

    pub fn apply_sidebar_input_bytes(&mut self, bytes: &[u8]) -> EventDrivenSidebarInputOutcome {
        let mut outcome = EventDrivenSidebarInputOutcome::default();

        for action in sidebar_actions(&mut self.pending_sidebar_input, bytes) {
            match action {
                SidebarInputAction::Previous => {
                    if let Some(target) = self.state.step_selection(-1) {
                        merge_render_update(
                            &mut outcome.render,
                            self.publish(LocalRuntimeEvent::Chrome(
                                ChromeEvent::SidebarSelectionChanged { target },
                            )),
                        );
                    }
                }
                SidebarInputAction::Next => {
                    if let Some(target) = self.state.step_selection(1) {
                        merge_render_update(
                            &mut outcome.render,
                            self.publish(LocalRuntimeEvent::Chrome(
                                ChromeEvent::SidebarSelectionChanged { target },
                            )),
                        );
                    }
                }
                SidebarInputAction::Submit => {
                    outcome.activation = self.state.activation();
                }
            }
        }

        outcome
    }

    #[cfg(test)]
    pub fn selected_target(&self) -> Option<String> {
        self.state.selected_target()
    }

    fn publish(&mut self, event: LocalRuntimeEvent) -> EventDrivenChromeRenderUpdate {
        self.bus.publish(event);
        self.drain_pending_events(now_millis())
    }

    fn drain_pending_events(&mut self, now_millis: u128) -> EventDrivenChromeRenderUpdate {
        let mut update = EventDrivenChromeRenderUpdate::default();
        while let Ok(envelope) = self.event_rx.try_recv() {
            self.state.observe(&envelope.payload);
            merge_render_update(
                &mut update,
                self.chrome_runtime
                    .apply_event(&envelope.payload, now_millis),
            );
        }
        update
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EventDrivenUiPaneState {
    active_socket: String,
    active_session: String,
    active_target: Option<String>,
    sessions: Vec<ManagedSessionRecord>,
    listener_display: Option<String>,
    selected_target: Option<String>,
}

impl EventDrivenUiPaneState {
    fn observe(&mut self, event: &LocalRuntimeEvent) {
        match event {
            LocalRuntimeEvent::SessionCatalog(SessionCatalogEvent::SnapshotUpdated {
                active_socket,
                active_session,
                active_target,
                sessions,
                listener_display,
            }) => {
                self.active_socket = active_socket.clone();
                self.active_session = active_session.clone();
                self.active_target = active_target.clone();
                self.sessions = sessions.clone();
                self.listener_display = listener_display.clone();
                self.ensure_active_target();
                self.ensure_selected_session();
            }
            LocalRuntimeEvent::Chrome(ChromeEvent::SidebarSelectionChanged { target }) => {
                if self.sessions.is_empty() {
                    self.selected_target = None;
                } else if self
                    .sessions
                    .iter()
                    .any(|session| session.address.qualified_target() == *target)
                {
                    self.selected_target = Some(target.clone());
                }
            }
            _ => {}
        }
    }

    fn ensure_selected_session(&mut self) {
        if self.sessions.is_empty() {
            self.selected_target = None;
            return;
        }

        let selection_is_still_valid = self.selected_target.as_ref().map(|target| {
            self.sessions
                .iter()
                .any(|session| session.address.qualified_target() == *target)
        }) == Some(true);
        if selection_is_still_valid {
            return;
        }

        self.selected_target = self
            .active_session_record()
            .or_else(|| self.sessions.first())
            .map(|session| session.address.qualified_target());
    }

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

    fn step_selection(&self, delta: isize) -> Option<String> {
        if self.sessions.is_empty() {
            return None;
        }

        let current_index = self
            .selected_target
            .as_ref()
            .and_then(|target| {
                self.sessions
                    .iter()
                    .position(|session| session.address.qualified_target() == *target)
            })
            .unwrap_or(0);
        let next_index =
            ((current_index as isize + delta).rem_euclid(self.sessions.len() as isize)) as usize;
        Some(self.sessions[next_index].address.qualified_target())
    }

    fn selected_target(&self) -> Option<String> {
        self.selected_session()
            .map(|session| session.address.qualified_target())
    }

    fn activation(&self) -> Option<EventDrivenSidebarActivation> {
        let selected_target = self.selected_target()?;
        if self.active_target.as_deref() == Some(selected_target.as_str()) {
            return Some(EventDrivenSidebarActivation::SelectMainPane);
        }

        Some(EventDrivenSidebarActivation::ActivateTarget {
            target: selected_target,
        })
    }

    fn selected_session(&self) -> Option<&ManagedSessionRecord> {
        self.selected_target
            .as_ref()
            .and_then(|target| {
                self.sessions
                    .iter()
                    .find(|session| session.address.qualified_target() == *target)
            })
            .or_else(|| self.active_session_record())
            .or_else(|| self.sessions.first())
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
pub struct EventDrivenSidebarInputOutcome {
    pub render: EventDrivenChromeRenderUpdate,
    pub activation: Option<EventDrivenSidebarActivation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventDrivenSidebarActivation {
    SelectMainPane,
    ActivateTarget { target: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarInputAction {
    Previous,
    Next,
    Submit,
}

fn sidebar_actions(pending: &mut Vec<u8>, bytes: &[u8]) -> Vec<SidebarInputAction> {
    pending.extend_from_slice(bytes);
    let mut actions = Vec::new();

    loop {
        if pending.starts_with(b"\x1b[A") || pending.starts_with(b"\x1bOA") {
            pending.drain(..3);
            actions.push(SidebarInputAction::Previous);
        } else if pending.starts_with(b"\x1b[B") || pending.starts_with(b"\x1bOB") {
            pending.drain(..3);
            actions.push(SidebarInputAction::Next);
        } else if pending.starts_with(b"\x1bOM") {
            pending.drain(..3);
            actions.push(SidebarInputAction::Submit);
        } else if pending.starts_with(b"\x1b[13u") {
            pending.drain(..5);
            actions.push(SidebarInputAction::Submit);
        } else if pending.first() == Some(&b'\r') || pending.first() == Some(&b'\n') {
            pending.drain(..1);
            actions.push(SidebarInputAction::Submit);
        } else if is_partial_sidebar_sequence(pending) || pending.is_empty() {
            break;
        } else {
            pending.drain(..1);
        }
    }

    actions
}

fn is_partial_sidebar_sequence(pending: &[u8]) -> bool {
    [
        b"\x1b[".as_slice(),
        b"\x1bO".as_slice(),
        b"\x1b[1".as_slice(),
        b"\x1b[13".as_slice(),
    ]
    .iter()
    .any(|pattern| pattern.starts_with(pending))
}

fn merge_render_update(
    update: &mut EventDrivenChromeRenderUpdate,
    next: EventDrivenChromeRenderUpdate,
) {
    update.invalidate_sidebar |= next.invalidate_sidebar;
    update.invalidate_footer |= next.invalidate_footer;
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

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{EventDrivenSidebarActivation, EventDrivenUiPaneRuntime};
    use crate::domain::local_runtime::ChromeSurface;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use std::path::PathBuf;

    #[test]
    fn snapshot_and_surface_events_drive_sidebar_footer_and_status_buffers() {
        let mut runtime = EventDrivenUiPaneRuntime::new();

        runtime.publish_surface_resize(ChromeSurface::SidebarPane, 28, 9);
        runtime.publish_surface_resize(ChromeSurface::FooterPane, 96, 1);
        runtime.publish_surface_resize(ChromeSurface::FullscreenStatusLine, 120, 1);

        let update = runtime.publish_session_snapshot(
            "wa-1",
            "sess-1",
            Some("wa-1:sess-1"),
            vec![session("wa-1", "sess-1", "bash")],
            Some("192.168.1.22:7474"),
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
    fn sidebar_navigation_emits_selection_event_and_rerenders_sidebar() {
        let mut runtime = EventDrivenUiPaneRuntime::new();
        runtime.publish_surface_resize(ChromeSurface::SidebarPane, 28, 9);
        runtime.publish_session_snapshot(
            "wa-1",
            "sess-1",
            Some("wa-1:sess-1"),
            vec![
                session("wa-1", "sess-1", "bash"),
                session("wa-2", "sess-2", "codex"),
            ],
            None,
        );

        let outcome = runtime.apply_sidebar_input_bytes(b"\x1b[B");

        assert!(matches!(outcome.activation, None));
        assert!(outcome
            .render
            .sidebar
            .as_ref()
            .map(|buffer| buffer.contains("> codex@local"))
            .unwrap_or(false));
        assert_eq!(runtime.selected_target().as_deref(), Some("wa-2:sess-2"));
    }

    #[test]
    fn sidebar_selection_uses_qualified_target_when_remote_sessions_share_session_id() {
        let mut runtime = EventDrivenUiPaneRuntime::new();
        runtime.publish_surface_resize(ChromeSurface::SidebarPane, 40, 9);
        runtime.publish_session_snapshot(
            "wa-1",
            "target-1",
            Some("wa-1:target-1"),
            vec![
                session("wa-1", "target-1", "bash"),
                remote_session("10.1.29.165", "pty1", "codex"),
                remote_session("10.1.29.166", "pty1", "claude"),
            ],
            None,
        );

        runtime.apply_sidebar_input_bytes(b"\x1b[B");
        runtime.apply_sidebar_input_bytes(b"\x1b[B");

        assert_eq!(
            runtime.selected_target().as_deref(),
            Some("10.1.29.166:pty1")
        );
    }

    #[test]
    fn sidebar_submit_returns_attach_or_main_pane_activation() {
        let mut runtime = EventDrivenUiPaneRuntime::new();
        runtime.publish_surface_resize(ChromeSurface::SidebarPane, 28, 9);
        runtime.publish_session_snapshot(
            "wa-1",
            "sess-1",
            Some("wa-1:sess-1"),
            vec![
                session("wa-1", "sess-1", "bash"),
                session("wa-2", "sess-2", "codex"),
            ],
            None,
        );

        let current = runtime.apply_sidebar_input_bytes(b"\r");
        assert_eq!(
            current.activation,
            Some(EventDrivenSidebarActivation::SelectMainPane)
        );

        runtime.apply_sidebar_input_bytes(b"\x1b[B");
        let other = runtime.apply_sidebar_input_bytes(b"\r");
        assert_eq!(
            other.activation,
            Some(EventDrivenSidebarActivation::ActivateTarget {
                target: "wa-2:sess-2".to_string(),
            })
        );
    }

    #[test]
    fn refreshed_snapshot_updates_active_target_without_resetting_selection() {
        let mut runtime = EventDrivenUiPaneRuntime::new();
        runtime.publish_surface_resize(ChromeSurface::SidebarPane, 28, 9);
        runtime.publish_surface_resize(ChromeSurface::FooterPane, 96, 1);
        runtime.publish_session_snapshot(
            "wa-1",
            "sess-1",
            Some("wa-1:sess-1"),
            vec![
                session("wa-1", "sess-1", "bash"),
                session("wa-1", "sess-2", "codex"),
            ],
            None,
        );

        runtime.apply_sidebar_input_bytes(b"\x1b[B");
        let update = runtime.publish_session_snapshot(
            "wa-1",
            "sess-1",
            Some("wa-1:sess-2"),
            vec![
                session("wa-1", "sess-1", "bash"),
                session("wa-1", "sess-2", "codex"),
            ],
            None,
        );

        assert_eq!(runtime.state.active_target.as_deref(), Some("wa-1:sess-2"));
        assert_eq!(runtime.selected_target().as_deref(), Some("wa-1:sess-2"));
        assert!(update
            .sidebar
            .as_ref()
            .map(|buffer| buffer.contains("codex@local"))
            .unwrap_or(false));
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

    fn remote_session(authority_id: &str, session: &str, command: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session),
            selector: Some(format!("{authority_id}:{session}")),
            availability: crate::domain::session_catalog::SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some(command.to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        }
    }
}
