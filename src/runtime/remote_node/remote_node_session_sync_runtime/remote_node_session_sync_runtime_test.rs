mod tests {
    use super::super::{
        compute_session_sync_delta, exportable_local_sessions_for_socket,
        local_sessions_by_local_id, overlay_workspace_runtime_onto_active_local_target_hosts,
        remote_session_exited_envelope, remote_session_published_envelope,
        remote_session_sync_owner_available, remote_session_sync_owner_socket_path,
        LocalSessionCatalog, OutboundRemoteNodeTransport, RemoteNodeSessionSyncRuntime,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::published_target_store::PublishedTargetStore;
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_transport::{
        OutboundNodeSessionRequest, RemoteNodeSessionHandle, RemoteNodeTransportEvent,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, SystemTime};

    #[test]
    fn session_sync_delta_publishes_new_and_removed_sessions() {
        let previous = HashMap::from([(
            "local-tmux:wa-1:shell-old".to_string(),
            session("wa-1", "shell-old"),
        )]);
        let current = local_sessions_by_local_id(vec![
            session("wa-1", "shell-1"),
            session("wa-1", "shell-2"),
        ]);

        let delta = compute_session_sync_delta(&previous, &current);

        assert_eq!(delta.publish.len(), 2);
        assert_eq!(delta.exit.len(), 1);
        assert_eq!(delta.exit[0].address.session_id(), "shell-old");
    }

    #[test]
    fn session_sync_delta_publishes_interactive_shell_input_running_transition() {
        // An Input→Running state change for a shell session IS meaningful
        // (e.g. a command just started producing output) and must be published.
        let mut previous_session = session("wa-1", "shell-1");
        previous_session.command_name = Some("bash".to_string());
        previous_session.task_state = ManagedSessionTaskState::Input;
        let mut current_session = previous_session.clone();
        current_session.task_state = ManagedSessionTaskState::Running;
        let previous = local_sessions_by_local_id(vec![previous_session]);
        let current = local_sessions_by_local_id(vec![current_session]);

        let delta = compute_session_sync_delta(&previous, &current);

        assert_eq!(delta.publish.len(), 1);
        assert!(delta.exit.is_empty());
    }

    #[test]
    fn session_sync_delta_ignores_interactive_shell_prompt_fluctuation() {
        // When the task_state hasn't changed (Running→Running or Input→Input),
        // normalize to Input and skip publication to suppress spurious updates
        // from prompt-character differences between polls.
        let mut session = session("wa-1", "shell-1");
        session.command_name = Some("bash".to_string());
        session.task_state = ManagedSessionTaskState::Input;
        let previous = local_sessions_by_local_id(vec![session.clone()]);
        let current = local_sessions_by_local_id(vec![session]);

        let delta = compute_session_sync_delta(&previous, &current);

        assert!(delta.publish.is_empty());
        assert!(delta.exit.is_empty());
    }

    #[test]
    fn session_sync_delta_publishes_non_shell_state_changes() {
        let mut previous_session = session("wa-1", "shell-1");
        previous_session.command_name = Some("codex".to_string());
        previous_session.task_state = ManagedSessionTaskState::Input;
        let mut current_session = previous_session.clone();
        current_session.task_state = ManagedSessionTaskState::Running;
        let previous = local_sessions_by_local_id(vec![previous_session]);
        let current = local_sessions_by_local_id(vec![current_session]);

        let delta = compute_session_sync_delta(&previous, &current);

        assert_eq!(delta.publish.len(), 1);
        assert!(delta.exit.is_empty());
    }

    #[test]
    fn session_sync_delta_publishes_interactive_shell_non_state_changes() {
        let mut previous_session = session("wa-1", "shell-1");
        previous_session.command_name = Some("bash".to_string());
        previous_session.task_state = ManagedSessionTaskState::Input;
        let mut current_session = previous_session.clone();
        current_session.current_path = Some(PathBuf::from("/tmp/other"));
        current_session.task_state = ManagedSessionTaskState::Running;
        let previous = local_sessions_by_local_id(vec![previous_session]);
        let current = local_sessions_by_local_id(vec![current_session]);

        let delta = compute_session_sync_delta(&previous, &current);

        assert_eq!(delta.publish.len(), 1);
        assert!(delta.exit.is_empty());
    }

    #[test]
    fn remote_session_published_envelope_uses_remote_peer_identity() {
        let envelope = remote_session_published_envelope(
            "10.0.0.2",
            "server-session-1",
            7,
            &session("wa-1", "shell-1"),
        );

        let Some(Body::TargetPublished(payload)) = envelope.body else {
            panic!("expected target_published body");
        };
        assert_eq!(payload.target_id, "remote-peer:10.0.0.2:shell-1");
        assert_eq!(payload.transport_session_id, "shell-1");
    }

    #[test]
    fn remote_session_exited_envelope_uses_remote_peer_identity() {
        let envelope = remote_session_exited_envelope("10.0.0.2", "server-session-1", 8, "shell-1");

        let Some(Body::TargetExited(payload)) = envelope.body else {
            panic!("expected target_exited body");
        };
        assert_eq!(payload.target_id, "remote-peer:10.0.0.2:shell-1");
        assert_eq!(payload.transport_session_id, "shell-1");
    }

    #[test]
    fn runtime_start_publishes_local_sessions_after_session_open() {
        let receiver_slot = Arc::new(Mutex::new(None));
        let runtime = RemoteNodeSessionSyncRuntime {
            gateway: FakeGateway {
                sessions: vec![session("wa-1", "shell-1")],
            },
            transport: FakeTransport {
                receiver_slot: receiver_slot.clone(),
            },
            network: RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
            },
            poll_interval: Duration::from_millis(10),
            reconnect_delay: Duration::from_millis(10),
        };

        let guard = runtime.start().expect("runtime should start");
        let start = std::time::Instant::now();
        let envelope = loop {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("timed out waiting for outbound session sync envelope");
            }
            if let Some(envelope) = try_take_envelope(&receiver_slot) {
                break envelope;
            }
            thread::sleep(Duration::from_millis(10));
        };

        let Some(Body::TargetPublished(payload)) = envelope.body else {
            panic!("expected target_published body");
        };
        assert_eq!(payload.transport_session_id, "shell-1");
        drop(guard);
    }

    #[test]
    fn exportable_local_sessions_for_socket_keeps_workspace_sessions_on_current_socket() {
        let store = PublishedTargetStore::new(test_store_path("export-current-socket"));
        let sessions = exportable_local_sessions_for_socket(
            vec![
                session_with_role("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
                session_with_role("wa-1", "shell-1", WorkspaceSessionRole::TargetHost),
                session_with_role("wa-2", "shell-2", WorkspaceSessionRole::TargetHost),
            ],
            "wa-1",
            &store,
        );

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].address.server_id(), "wa-1");
        assert_eq!(sessions[0].address.session_id(), "workspace");
        assert!(sessions[0].is_workspace_chrome());
        assert_eq!(sessions[1].address.server_id(), "wa-1");
        assert_eq!(sessions[1].address.session_id(), "shell-1");
        assert!(sessions[1].is_target_host());
    }

    #[test]
    fn exportable_local_target_host_keeps_cached_remote_identity_but_uses_live_runtime_metadata() {
        let store = PublishedTargetStore::new(test_store_path("export-target-host-remote"));
        let local_target = ManagedSessionRecord {
            availability: SessionAvailability::Online,
            attached_clients: 2,
            window_count: 3,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/live")),
            task_state: ManagedSessionTaskState::Input,
            ..session("wa-1", "shell-1")
        };
        let remote_target = ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("peer-a", "shell-1"),
            selector: Some("wa-1:shell-1".to_string()),
            availability: SessionAvailability::Offline,
            workspace_dir: None,
            workspace_key: Some("shell-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/cached")),
            task_state: ManagedSessionTaskState::Running,
        };
        store
            .upsert_target_from_source("wa-1", Some("shell-1"), &remote_target)
            .expect("published target should upsert");

        let sessions = exportable_local_sessions_for_socket(vec![local_target], "wa-1", &store);

        assert_eq!(sessions.len(), 1);
        let exported = &sessions[0];
        assert_eq!(exported.address, remote_target.address);
        assert_eq!(exported.selector, remote_target.selector);
        assert_eq!(exported.command_name.as_deref(), Some("codex"));
        assert_eq!(
            exported.current_path.as_deref(),
            Some(Path::new("/tmp/live"))
        );
        assert_eq!(exported.task_state, ManagedSessionTaskState::Input);
        assert_eq!(exported.attached_clients, 2);
        assert_eq!(exported.window_count, 3);
        assert_eq!(exported.availability, SessionAvailability::Online);
    }

    #[test]
    fn overlay_workspace_runtime_projects_active_target_host_runtime() {
        let sessions = overlay_workspace_runtime_onto_active_local_target_hosts(
            vec![
                ManagedSessionRecord {
                    command_name: Some("codex".to_string()),
                    current_path: Some(PathBuf::from("/tmp/workspace")),
                    task_state: ManagedSessionTaskState::Input,
                    ..session_with_role("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome)
                },
                ManagedSessionRecord {
                    command_name: Some("bash".to_string()),
                    current_path: Some(PathBuf::from("/tmp/host")),
                    task_state: ManagedSessionTaskState::Running,
                    ..session("wa-1", "shell-1")
                },
            ],
            "wa-1",
            &HashMap::from([("workspace".to_string(), "wa-1:shell-1".to_string())]),
        );

        let projected = sessions
            .into_iter()
            .find(|session| session.address.session_id() == "shell-1")
            .expect("target-host session should exist");
        assert_eq!(projected.command_name.as_deref(), Some("codex"));
        assert_eq!(
            projected.current_path.as_deref(),
            Some(Path::new("/tmp/workspace"))
        );
        assert_eq!(projected.task_state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn overlay_workspace_runtime_preserves_live_agent_runtime_on_active_target_host() {
        let sessions = overlay_workspace_runtime_onto_active_local_target_hosts(
            vec![
                ManagedSessionRecord {
                    command_name: Some("bash".to_string()),
                    current_path: Some(PathBuf::from("/tmp/workspace")),
                    task_state: ManagedSessionTaskState::Input,
                    ..session_with_role("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome)
                },
                ManagedSessionRecord {
                    command_name: Some("codex".to_string()),
                    current_path: Some(PathBuf::from("/tmp/target")),
                    task_state: ManagedSessionTaskState::Input,
                    ..session("wa-1", "shell-1")
                },
            ],
            "wa-1",
            &HashMap::from([("workspace".to_string(), "wa-1:shell-1".to_string())]),
        );

        let projected = sessions
            .into_iter()
            .find(|session| session.address.session_id() == "shell-1")
            .expect("target-host session should exist");
        assert_eq!(projected.command_name.as_deref(), Some("codex"));
        assert_eq!(
            projected.current_path.as_deref(),
            Some(Path::new("/tmp/target"))
        );
        assert_eq!(projected.task_state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn remote_session_sync_owner_available_observes_bound_owner_socket() {
        let socket_name = format!("wa-test-sync-owner-{}", std::process::id());
        let socket_path = remote_session_sync_owner_socket_path(&socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        assert!(!remote_session_sync_owner_available(&socket_path));
        let listener = UnixListener::bind(&socket_path).expect("owner socket should bind");
        assert!(remote_session_sync_owner_available(&socket_path));
        drop(listener);
        let _ = fs::remove_file(&socket_path);
    }

    #[derive(Clone)]
    struct FakeGateway {
        sessions: Vec<ManagedSessionRecord>,
    }

    impl LocalSessionCatalog for FakeGateway {
        type Error = &'static str;

        fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            Ok(self.sessions.clone())
        }
    }

    #[derive(Clone)]
    struct FakeTransport {
        receiver_slot: Arc<
            Mutex<
                Option<
                    tokio::sync::mpsc::UnboundedReceiver<
                        crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope,
                    >,
                >,
            >,
        >,
    }

    struct FakeGuard;

    impl OutboundRemoteNodeTransport for FakeTransport {
        type Guard = FakeGuard;
        type Error = &'static str;

        fn connect_outbound(
            &self,
            request: OutboundNodeSessionRequest,
            event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
        ) -> Result<Self::Guard, Self::Error> {
            let (handle, receiver) =
                RemoteNodeSessionHandle::new_for_tests(request.node_id, "server-session-1");
            *self
                .receiver_slot
                .lock()
                .expect("receiver slot mutex should not be poisoned") = Some(receiver);
            event_tx
                .send(RemoteNodeTransportEvent::SessionOpened { session: handle })
                .map_err(|_| "failed to deliver session open event")?;
            Ok(FakeGuard)
        }
    }

    fn try_take_envelope(
        receiver_slot: &Arc<
            Mutex<
                Option<
                    tokio::sync::mpsc::UnboundedReceiver<
                        crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope,
                    >,
                >,
            >,
        >,
    ) -> Option<crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope> {
        receiver_slot
            .lock()
            .expect("receiver slot mutex should not be poisoned")
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
    }

    fn session(socket_name: &str, session_id: &str) -> ManagedSessionRecord {
        session_with_role(socket_name, session_id, WorkspaceSessionRole::TargetHost)
    }

    fn session_with_role(
        socket_name: &str,
        session_id: &str,
        session_role: WorkspaceSessionRole,
    ) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket_name, session_id),
            selector: Some(format!("{socket_name}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some(session_id.to_string()),
            session_role: Some(session_role),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Running,
        }
    }

    fn test_store_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "waitagent-session-sync-{name}-{}-{nanos}.tsv",
            std::process::id()
        ))
    }
}
