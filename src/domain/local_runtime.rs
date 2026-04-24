use crate::domain::session_catalog::ManagedSessionRecord;
use crate::event::{EventBusMessage, EventGroup};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalRuntimeProducer {
    TmuxHookBridge,
    SessionCatalogProjector,
    SidebarPaneRuntime,
    FooterPaneRuntime,
    AttachClientRuntime,
    SchedulerRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalRuntimeConsumer {
    WorkspaceController,
    SessionCatalogProjector,
    SidebarPaneRuntime,
    FooterPaneRuntime,
    AttachClientRuntime,
    SchedulerRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalRuntimeEventKind {
    TmuxHook,
    SessionCatalog,
    Chrome,
    Attach,
    Scheduler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TmuxHookName {
    ClientAttached,
    ClientDetached,
    ClientResized,
    ClientSessionChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromeSurface {
    SidebarPane,
    FooterPane,
    FullscreenStatusLine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalRuntimeEvent {
    TmuxHook(TmuxHookEvent),
    SessionCatalog(SessionCatalogEvent),
    Chrome(ChromeEvent),
    Attach(AttachEvent),
    Scheduler(SchedulerEvent),
}

impl LocalRuntimeEvent {
    pub fn kind(&self) -> LocalRuntimeEventKind {
        match self {
            Self::TmuxHook(_) => LocalRuntimeEventKind::TmuxHook,
            Self::SessionCatalog(_) => LocalRuntimeEventKind::SessionCatalog,
            Self::Chrome(_) => LocalRuntimeEventKind::Chrome,
            Self::Attach(_) => LocalRuntimeEventKind::Attach,
            Self::Scheduler(_) => LocalRuntimeEventKind::Scheduler,
        }
    }

    pub fn producer(&self) -> LocalRuntimeProducer {
        match self {
            Self::TmuxHook(_) => LocalRuntimeProducer::TmuxHookBridge,
            Self::SessionCatalog(_) => LocalRuntimeProducer::SessionCatalogProjector,
            Self::Chrome(ChromeEvent::SidebarSelectionChanged { .. }) => {
                LocalRuntimeProducer::SidebarPaneRuntime
            }
            Self::Chrome(_) => LocalRuntimeProducer::FooterPaneRuntime,
            Self::Attach(_) => LocalRuntimeProducer::AttachClientRuntime,
            Self::Scheduler(_) => LocalRuntimeProducer::SchedulerRuntime,
        }
    }

    pub fn consumers(&self) -> Vec<LocalRuntimeConsumer> {
        match self {
            Self::TmuxHook(_) => vec![
                LocalRuntimeConsumer::WorkspaceController,
                LocalRuntimeConsumer::SessionCatalogProjector,
            ],
            Self::SessionCatalog(_) => vec![
                LocalRuntimeConsumer::SidebarPaneRuntime,
                LocalRuntimeConsumer::FooterPaneRuntime,
                LocalRuntimeConsumer::SchedulerRuntime,
            ],
            Self::Chrome(ChromeEvent::SidebarSelectionChanged { .. }) => vec![
                LocalRuntimeConsumer::WorkspaceController,
                LocalRuntimeConsumer::FooterPaneRuntime,
            ],
            Self::Chrome(_) => vec![LocalRuntimeConsumer::WorkspaceController],
            Self::Attach(AttachEvent::ClientResized { .. }) => vec![
                LocalRuntimeConsumer::WorkspaceController,
                LocalRuntimeConsumer::SchedulerRuntime,
            ],
            Self::Attach(_) => vec![
                LocalRuntimeConsumer::WorkspaceController,
                LocalRuntimeConsumer::SchedulerRuntime,
            ],
            Self::Scheduler(SchedulerEvent::FocusChanged { .. }) => vec![
                LocalRuntimeConsumer::SidebarPaneRuntime,
                LocalRuntimeConsumer::FooterPaneRuntime,
            ],
            Self::Scheduler(_) => vec![LocalRuntimeConsumer::WorkspaceController],
        }
    }
}

impl EventBusMessage for LocalRuntimeEvent {
    fn event_group(&self) -> EventGroup {
        match self {
            Self::TmuxHook(_) | Self::Chrome(_) => EventGroup::Console,
            Self::SessionCatalog(_) => EventGroup::Session,
            Self::Attach(_) => EventGroup::Transport,
            Self::Scheduler(_) => EventGroup::Scheduler,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxHookEvent {
    pub hook: TmuxHookName,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCatalogEvent {
    SnapshotUpdated {
        active_socket: String,
        active_session: String,
        sessions: Vec<ManagedSessionRecord>,
    },
    SnapshotPublished {
        active_session_id: String,
        visible_sessions: usize,
    },
    SelectionChanged {
        selected_session_id: String,
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
    RenderRequested {
        surface: ChromeSurface,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    ClientAttached {
        client_id: u64,
    },
    ClientInputRead {
        client_id: u64,
        bytes: usize,
    },
    ClientResized {
        client_id: u64,
        rows: u16,
        cols: u16,
    },
    DaemonOutputReceived {
        bytes: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerEvent {
    FocusChanged {
        session_id: String,
    },
    AutoSwitchRequested {
        from_session_id: String,
        to_session_id: String,
    },
    AutoSwitchCommitted {
        session_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        AttachEvent, ChromeEvent, ChromeSurface, LocalRuntimeConsumer, LocalRuntimeEvent,
        LocalRuntimeEventKind, LocalRuntimeProducer, SchedulerEvent, SessionCatalogEvent,
        TmuxHookEvent, TmuxHookName,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::event::{EventBusMessage, EventGroup};
    use std::path::PathBuf;

    #[test]
    fn local_runtime_event_maps_to_expected_kind_group_and_producer() {
        let event = LocalRuntimeEvent::TmuxHook(TmuxHookEvent {
            hook: TmuxHookName::ClientResized,
            session_id: "sess-1".to_string(),
        });

        assert_eq!(event.kind(), LocalRuntimeEventKind::TmuxHook);
        assert_eq!(event.event_group(), EventGroup::Console);
        assert_eq!(event.producer(), LocalRuntimeProducer::TmuxHookBridge);
        assert_eq!(
            event.consumers(),
            vec![
                LocalRuntimeConsumer::WorkspaceController,
                LocalRuntimeConsumer::SessionCatalogProjector
            ]
        );
    }

    #[test]
    fn session_catalog_events_fan_out_to_sidebar_footer_and_scheduler() {
        let event = LocalRuntimeEvent::SessionCatalog(SessionCatalogEvent::SnapshotUpdated {
            active_socket: "wa-1".to_string(),
            active_session: "sess-1".to_string(),
            sessions: vec![ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "sess-1"),
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: None,
                attached_clients: 1,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: Some(PathBuf::from("/tmp/demo")),
                task_state: ManagedSessionTaskState::Input,
            }],
        });

        assert_eq!(event.event_group(), EventGroup::Session);
        assert_eq!(
            event.producer(),
            LocalRuntimeProducer::SessionCatalogProjector
        );
        assert_eq!(
            event.consumers(),
            vec![
                LocalRuntimeConsumer::SidebarPaneRuntime,
                LocalRuntimeConsumer::FooterPaneRuntime,
                LocalRuntimeConsumer::SchedulerRuntime
            ]
        );
    }

    #[test]
    fn attach_resize_events_target_workspace_control_and_scheduler() {
        let event = LocalRuntimeEvent::Attach(AttachEvent::ClientResized {
            client_id: 7,
            rows: 42,
            cols: 120,
        });

        assert_eq!(event.event_group(), EventGroup::Transport);
        assert_eq!(event.producer(), LocalRuntimeProducer::AttachClientRuntime);
        assert_eq!(
            event.consumers(),
            vec![
                LocalRuntimeConsumer::WorkspaceController,
                LocalRuntimeConsumer::SchedulerRuntime
            ]
        );
    }

    #[test]
    fn scheduler_focus_events_are_projected_back_to_chrome_surfaces() {
        let focus_event = LocalRuntimeEvent::Scheduler(SchedulerEvent::FocusChanged {
            session_id: "sess-9".to_string(),
        });
        let chrome_event = LocalRuntimeEvent::Chrome(ChromeEvent::RenderRequested {
            surface: ChromeSurface::FooterPane,
            reason: "session catalog changed",
        });

        assert_eq!(focus_event.event_group(), EventGroup::Scheduler);
        assert_eq!(
            focus_event.consumers(),
            vec![
                LocalRuntimeConsumer::SidebarPaneRuntime,
                LocalRuntimeConsumer::FooterPaneRuntime
            ]
        );
        assert_eq!(
            chrome_event.producer(),
            LocalRuntimeProducer::FooterPaneRuntime
        );
    }
}
