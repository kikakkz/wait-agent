use crate::domain::session_catalog::ManagedSessionRecord;
use crate::event::{EventBusMessage, EventGroup};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromeSurface {
    SidebarPane,
    FooterPane,
    FullscreenStatusLine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalRuntimeEvent {
    SessionCatalog(SessionCatalogEvent),
    Chrome(ChromeEvent),
}

impl EventBusMessage for LocalRuntimeEvent {
    fn event_group(&self) -> EventGroup {
        match self {
            Self::SessionCatalog(_) => EventGroup::Session,
            Self::Chrome(_) => EventGroup::Console,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCatalogEvent {
    SnapshotUpdated {
        active_socket: String,
        active_session: String,
        active_target: Option<String>,
        sessions: Vec<ManagedSessionRecord>,
        listener_display: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeEvent {
    SidebarSelectionChanged {
        session_id: String,
    },
    SurfaceResized {
        surface: ChromeSurface,
        width: usize,
        height: usize,
    },
    FullscreenChanged {
        is_fullscreen: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::{ChromeEvent, ChromeSurface, LocalRuntimeEvent, SessionCatalogEvent};
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::event::{EventBusMessage, EventGroup};
    use std::path::PathBuf;

    #[test]
    fn session_catalog_events_are_grouped_as_session_updates() {
        let event = LocalRuntimeEvent::SessionCatalog(SessionCatalogEvent::SnapshotUpdated {
            active_socket: "wa-1".to_string(),
            active_session: "sess-1".to_string(),
            active_target: Some("wa-1:sess-1".to_string()),
            sessions: vec![session("wa-1", "sess-1")],
            listener_display: Some("192.168.1.22:7474".to_string()),
        });

        assert_eq!(event.event_group(), EventGroup::Session);
    }

    #[test]
    fn chrome_events_are_grouped_as_console_updates() {
        let event = LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
            surface: ChromeSurface::FooterPane,
            width: 80,
            height: 1,
        });

        assert_eq!(event.event_group(), EventGroup::Console);
    }

    fn session(socket: &str, session: &str) -> ManagedSessionRecord {
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
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }
}
