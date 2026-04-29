use crate::application::local_runtime_event_service::LocalRuntimeEventPublisher;
use crate::domain::local_runtime::{LocalRuntimeEvent, SessionCatalogEvent};
use crate::domain::session_catalog::ManagedSessionRecord;

#[derive(Debug, Default, Clone)]
pub struct SessionCatalogProjectionService;

impl SessionCatalogProjectionService {
    pub fn new() -> Self {
        Self
    }

    pub fn publish_snapshot<P>(
        &self,
        publisher: &mut P,
        active_socket: &str,
        active_session: &str,
        active_target: Option<&str>,
        sessions: Vec<ManagedSessionRecord>,
    ) -> usize
    where
        P: LocalRuntimeEventPublisher,
    {
        publisher.publish(LocalRuntimeEvent::SessionCatalog(
            SessionCatalogEvent::SnapshotUpdated {
                active_socket: active_socket.to_string(),
                active_session: active_session.to_string(),
                active_target: active_target.map(str::to_string),
                sessions,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::SessionCatalogProjectionService;
    use crate::application::local_runtime_event_service::{
        LocalRuntimeEventBus, LocalRuntimeEventSubscriber,
    };
    use crate::domain::local_runtime::{LocalRuntimeEvent, SessionCatalogEvent};
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use std::path::PathBuf;

    #[test]
    fn publish_snapshot_emits_session_catalog_snapshot_event() {
        let service = SessionCatalogProjectionService::new();
        let mut bus = LocalRuntimeEventBus::new();
        let (_subscriber_id, rx) = bus.subscribe();

        assert_eq!(
            service.publish_snapshot(
                &mut bus,
                "wa-1",
                "sess-1",
                Some("wa-1:sess-1"),
                vec![session("wa-1", "sess-1")],
            ),
            1
        );

        let envelope = rx.recv().expect("snapshot event should be delivered");
        match envelope.payload {
            LocalRuntimeEvent::SessionCatalog(SessionCatalogEvent::SnapshotUpdated {
                active_socket,
                active_session,
                active_target,
                sessions,
            }) => {
                assert_eq!(active_socket, "wa-1");
                assert_eq!(active_session, "sess-1");
                assert_eq!(active_target.as_deref(), Some("wa-1:sess-1"));
                assert_eq!(sessions.len(), 1);
            }
            other => panic!("unexpected event payload: {other:?}"),
        }
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
