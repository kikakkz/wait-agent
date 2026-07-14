mod tests {
    use super::super::{
        authority_host_signal, compute_session_sync_delta, deliver_command_to_ready_host,
        exportable_local_sessions_for_socket, local_sessions_by_local_id,
        notify_remote_session_sync_owner, overlay_workspace_runtime_onto_active_local_target_hosts,
        remote_session_exited_envelope, remote_session_published_envelope,
        remote_session_sync_owner_args, remote_session_sync_owner_available,
        remote_session_sync_owner_socket_path, signal_remote_session_sync_owner,
        sync_local_sessions, AuthorityHostSignal, LocalCatalogChangeReason, LocalSessionCatalog,
        LocalTargetExitObserver, OutboundRemoteNodeTransport, RemoteNodeSessionSyncRuntime,
        SessionSyncAuthorityHost, SessionSyncAuthorityPublicationGateway, SessionSyncMode,
        SourcePublicationAckOutcome, SourcePublicationTracker,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_transport::{
        OutboundNodeSessionRequest, RemoteNodeSessionHandle, RemoteNodeTransportEvent,
    };
    use crate::infra::remote_protocol::{
        BootstrapMode, ControlPlanePayload, OpenMirrorRequestPayload, TargetPublicationAckPayload,
        TargetPublicationAckStatus,
    };
    use crate::infra::remote_transport_codec::read_control_plane_envelope;
    use crate::lifecycle::LifecycleError;
    use crate::runtime::remote_authority_target_host_runtime::RemoteAuthorityPublicationGateway;
    use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
    use crate::runtime::remote_runtime_owner_runtime::RemoteTargetSourceBindingResolver;
    use std::collections::HashMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;

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

        let delta = compute_session_sync_delta(&previous, &current, SessionSyncMode::Delta);

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

        let delta = compute_session_sync_delta(&previous, &current, SessionSyncMode::Delta);

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

        let delta = compute_session_sync_delta(&previous, &current, SessionSyncMode::Delta);

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

        let delta = compute_session_sync_delta(&previous, &current, SessionSyncMode::Delta);

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

        let delta = compute_session_sync_delta(&previous, &current, SessionSyncMode::Delta);

        assert_eq!(delta.publish.len(), 1);
        assert!(delta.exit.is_empty());
    }

    #[test]
    fn session_sync_full_baseline_publishes_unchanged_live_sessions() {
        let previous = local_sessions_by_local_id(vec![session("wa-1", "shell-1")]);
        let current = previous.clone();

        let delta = compute_session_sync_delta(&previous, &current, SessionSyncMode::FullBaseline);

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
    fn source_publication_tracker_revisions_ack_and_reconnect_replay() {
        let mut tracker = SourcePublicationTracker::new();
        let mut next_message_id = 0;
        tracker.on_connected();

        let first = tracker
            .on_state_changed(
                "10.0.0.2",
                "server-session-1",
                &mut next_message_id,
                &session("wa-1", "shell-1"),
            )
            .expect("new state should publish");
        assert_eq!(first.revision, 1);
        let Some(Body::TargetPublished(payload)) = first.envelope.body.as_ref() else {
            panic!("expected target published");
        };
        assert_eq!(payload.node_instance_id, "server-session-1");
        assert_eq!(payload.revision, 1);
        assert!(tracker
            .on_state_changed(
                "10.0.0.2",
                "server-session-1",
                &mut next_message_id,
                &session("wa-1", "shell-1"),
            )
            .is_none());

        assert_eq!(tracker.pending_publications().len(), 1);
        tracker.on_disconnected();
        assert!(tracker.pending_publications().is_empty());
        tracker.on_connected();
        assert_eq!(tracker.pending_publications()[0].revision, 1);

        let ack = TargetPublicationAckPayload {
            node_id: "10.0.0.2".to_string(),
            node_instance_id: "server-session-1".to_string(),
            target_id: "remote-peer:10.0.0.2:shell-1".to_string(),
            revision: 1,
            status: TargetPublicationAckStatus::Applied,
            message: None,
        };
        assert_eq!(tracker.on_ack(&ack), SourcePublicationAckOutcome::Cleared);
        assert!(tracker.pending_publications().is_empty());
    }

    #[test]
    fn source_publication_tracker_failed_ack_keeps_pending_and_newer_ack_does_not_clear() {
        let mut tracker = SourcePublicationTracker::new();
        let mut next_message_id = 0;
        tracker.on_connected();
        tracker
            .on_state_changed(
                "10.0.0.2",
                "server-session-1",
                &mut next_message_id,
                &session("wa-1", "shell-1"),
            )
            .expect("new state should publish");

        let failed = TargetPublicationAckPayload {
            node_id: "10.0.0.2".to_string(),
            node_instance_id: "server-session-1".to_string(),
            target_id: "remote-peer:10.0.0.2:shell-1".to_string(),
            revision: 1,
            status: TargetPublicationAckStatus::Failed,
            message: Some("try again".to_string()),
        };
        assert_eq!(
            tracker.on_ack(&failed),
            SourcePublicationAckOutcome::Retained
        );
        assert_eq!(tracker.pending_publications().len(), 1);

        let mut stale = failed.clone();
        stale.revision = 0;
        stale.status = TargetPublicationAckStatus::Applied;
        assert_eq!(tracker.on_ack(&stale), SourcePublicationAckOutcome::Ignored);
        assert_eq!(tracker.pending_publications().len(), 1);
    }

    #[test]
    fn source_publication_tracker_schedules_retry_backoff_and_pauses_while_disconnected() {
        let mut tracker = SourcePublicationTracker::new();
        let mut next_message_id = 0;
        let now = std::time::Instant::now();
        tracker.on_connected();
        let publication = tracker
            .on_state_changed(
                "10.0.0.2",
                "server-session-1",
                &mut next_message_id,
                &session("wa-1", "shell-1"),
            )
            .expect("new state should publish");

        tracker.on_publication_sent(&publication.target_id, publication.revision, now);
        assert!(tracker
            .due_retry_publications(now + Duration::from_millis(249))
            .is_empty());
        assert_eq!(
            tracker.due_retry_publications(now + Duration::from_millis(250))[0].revision,
            1
        );
        assert!(tracker
            .next_retry_delay(now + Duration::from_millis(100))
            .is_some());

        tracker.on_disconnected();
        assert!(tracker
            .due_retry_publications(now + Duration::from_secs(1))
            .is_empty());
        assert!(tracker
            .next_retry_delay(now + Duration::from_secs(1))
            .is_none());

        tracker.on_connected();
        assert_eq!(tracker.pending_publications()[0].revision, 1);
        tracker.on_publication_sent(&publication.target_id, publication.revision, now);
        assert!(tracker
            .due_retry_publications(now + Duration::from_millis(499))
            .is_empty());
        assert_eq!(
            tracker.due_retry_publications(now + Duration::from_millis(500))[0].revision,
            1
        );
    }

    #[test]
    fn source_publication_tracker_target_exit_uses_next_revision() {
        let mut tracker = SourcePublicationTracker::new();
        let mut next_message_id = 0;
        tracker.on_connected();
        tracker
            .on_state_changed(
                "10.0.0.2",
                "server-session-1",
                &mut next_message_id,
                &session("wa-1", "shell-1"),
            )
            .expect("new state should publish");
        let exit = tracker.on_target_exited(
            "10.0.0.2",
            "server-session-1",
            &mut next_message_id,
            "shell-1",
        );
        assert_eq!(exit.revision, 2);
        let Some(Body::TargetExited(payload)) = exit.envelope.body.as_ref() else {
            panic!("expected target exited");
        };
        assert_eq!(payload.revision, 2);
        assert_eq!(payload.node_instance_id, "server-session-1");
    }

    #[test]
    fn source_publication_tracker_full_baseline_republishes_unchanged_state() {
        let mut tracker = SourcePublicationTracker::new();
        let mut next_message_id = 0;
        tracker.on_connected();
        let session = session("wa-1", "shell-1");

        let first = tracker
            .on_state_changed(
                "10.0.0.2",
                "server-session-1",
                &mut next_message_id,
                &session,
            )
            .expect("new state should publish");
        let second = tracker.on_baseline_state(
            "10.0.0.2",
            "server-session-2",
            &mut next_message_id,
            &session,
        );

        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        let Some(Body::TargetPublished(payload)) = second.envelope.body.as_ref() else {
            panic!("expected target published");
        };
        assert_eq!(payload.revision, 2);
        assert_eq!(payload.node_instance_id, "server-session-2");
        assert_eq!(payload.availability, "online");
    }

    #[test]
    fn sync_local_sessions_reports_target_exit_to_local_lifecycle() {
        let (handle, mut receiver) =
            RemoteNodeSessionHandle::new_for_tests("10.0.0.2", "server-session-1");
        let observer = RecordingLocalTargetExitObserver::default();
        let mut synced_sessions = local_sessions_by_local_id(vec![session("wa-1", "shell-1")]);
        let mut next_message_id = 0;
        let mut publication_tracker = SourcePublicationTracker::new();
        publication_tracker.on_connected();

        sync_local_sessions(
            &FakeGateway { sessions: vec![] },
            "10.0.0.2",
            &handle,
            &observer,
            &mut synced_sessions,
            &mut next_message_id,
            &mut publication_tracker,
            SessionSyncMode::Delta,
        )
        .expect("local session sync should succeed");

        assert_eq!(
            observer
                .exits
                .lock()
                .expect("exit observer mutex should not be poisoned")
                .as_slice(),
            &[("wa-1".to_string(), "shell-1".to_string())]
        );
        assert!(synced_sessions.is_empty());

        let envelope = receiver
            .try_recv()
            .expect("remote target exit envelope should be sent");
        let Some(Body::TargetExited(payload)) = envelope.body else {
            panic!("expected target_exited body");
        };
        assert_eq!(payload.transport_session_id, "shell-1");
    }

    #[test]
    fn target_session_name_from_target_id_uses_last_colon_for_host_port_authority() {
        assert_eq!(
            super::super::target_session_name_from_target_id(
                "remote-peer:10.1.26.84:7474:6a1b816eb1111435"
            )
            .as_deref(),
            Some("6a1b816eb1111435")
        );
    }

    #[test]
    fn authority_host_starting_signal_waits_for_ready_before_delivery() {
        let (reader, writer) = UnixStream::pair().expect("unix stream pair should open");
        let host = SessionSyncAuthorityHost {
            writer: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            writer_ready: Arc::new(Condvar::new()),
            bound_session_instance_id: "session-1".to_string(),
        };
        assert_eq!(authority_host_signal(&host), AuthorityHostSignal::Starting);

        let host_writer = host.writer.clone();
        let host_ready = host.writer_ready.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            *host_writer
                .lock()
                .expect("writer mutex should not be poisoned") = Some(writer);
            host_ready.notify_all();
        });

        let command = open_mirror_command("remote-peer:peer-a:shell-1", "shell-1");
        assert_eq!(
            deliver_command_to_ready_host(&host, command).expect("delivery should complete"),
            AuthorityHostSignal::Ready
        );

        let mut reader = reader;
        let envelope =
            read_control_plane_envelope(&mut reader).expect("delivered command should be readable");
        match envelope.payload {
            ControlPlanePayload::OpenMirrorRequest(payload) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.session_id, "shell-1");
            }
            other => panic!("unexpected payload {}", other.message_type()),
        }
    }

    #[test]
    fn authority_host_closed_signal_is_explicit() {
        let host = SessionSyncAuthorityHost {
            writer: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
            writer_ready: Arc::new(Condvar::new()),
            bound_session_instance_id: "session-1".to_string(),
        };

        assert_eq!(authority_host_signal(&host), AuthorityHostSignal::Closed);
        assert_eq!(
            deliver_command_to_ready_host(
                &host,
                open_mirror_command("remote-peer:peer-a:shell-1", "shell-1")
            )
            .expect("closed host should return a signal"),
            AuthorityHostSignal::Closed
        );
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
            local_target_exit_observer: RecordingLocalTargetExitObserver::default(),
            network: RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            },
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
    fn runtime_retries_unacked_publication_with_same_revision() {
        let receiver_slot = Arc::new(Mutex::new(None));
        let runtime = RemoteNodeSessionSyncRuntime {
            gateway: FakeGateway {
                sessions: vec![session("wa-1", "shell-1")],
            },
            transport: FakeTransport {
                receiver_slot: receiver_slot.clone(),
            },
            local_target_exit_observer: RecordingLocalTargetExitObserver::default(),
            network: RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            },
            reconnect_delay: Duration::from_millis(10),
        };

        let guard = runtime.start().expect("runtime should start");
        let first = wait_for_envelope(&receiver_slot);
        let second = wait_for_envelope(&receiver_slot);
        drop(guard);

        let Some(Body::TargetPublished(first_payload)) = first.body else {
            panic!("expected first target_published body");
        };
        let Some(Body::TargetPublished(second_payload)) = second.body else {
            panic!("expected retry target_published body");
        };
        assert_eq!(first_payload.transport_session_id, "shell-1");
        assert_eq!(second_payload.transport_session_id, "shell-1");
        assert_eq!(first_payload.revision, 1);
        assert_eq!(second_payload.revision, 1);
    }

    #[test]
    fn runtime_reconnect_replays_pending_and_publishes_new_live_baseline() {
        let transport = ControlledReconnectTransport::default();
        let sessions = Arc::new(Mutex::new(vec![session("wa-1", "shell-1")]));
        let runtime = RemoteNodeSessionSyncRuntime {
            gateway: MutableFakeGateway {
                sessions: sessions.clone(),
            },
            transport: transport.clone(),
            local_target_exit_observer: RecordingLocalTargetExitObserver::default(),
            network: RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: Some("node-a".to_string()),
                public_endpoint: None,
            },
            reconnect_delay: Duration::from_millis(10),
        };

        let guard = runtime.start().expect("runtime should start");
        let first = wait_for_controlled_envelope(&transport.receivers, 0);
        let Some(Body::TargetPublished(first_payload)) = first.body else {
            panic!("expected first target_published body");
        };
        assert_eq!(first_payload.transport_session_id, "shell-1");
        assert_eq!(first_payload.revision, 1);
        assert_eq!(first_payload.node_instance_id, "server-session-1");

        sessions
            .lock()
            .expect("fake sessions mutex should not be poisoned")
            .push(session("wa-1", "shell-2"));
        transport.close_session(0, "node-a", "server-session-1");
        wait_for_controlled_receiver_count(&transport.receivers, 2);

        let second = wait_for_controlled_envelope(&transport.receivers, 1);
        let third = wait_for_controlled_envelope(&transport.receivers, 1);
        drop(guard);

        let publications = [second, third]
            .into_iter()
            .map(|envelope| {
                let Some(Body::TargetPublished(payload)) = envelope.body else {
                    panic!("expected target_published body after reconnect");
                };
                payload
            })
            .collect::<Vec<_>>();

        let replay = publications
            .iter()
            .find(|payload| payload.transport_session_id == "shell-1")
            .expect("shell-1 baseline should republish after reconnect");
        assert_eq!(replay.revision, 2);
        assert_eq!(replay.node_instance_id, "server-session-2");

        let baseline = publications
            .iter()
            .find(|payload| payload.transport_session_id == "shell-2")
            .expect("new live shell-2 baseline should publish");
        assert_eq!(baseline.revision, 1);
        assert_eq!(baseline.node_instance_id, "server-session-2");
    }

    #[test]
    fn runtime_observes_catalog_change_while_reconnecting_and_publishes_latest_baseline() {
        let transport = ControlledReconnectTransport::default();
        let mut initial = session("wa-1", "shell-1");
        initial.command_name = Some("bash".to_string());
        initial.task_state = ManagedSessionTaskState::Input;
        let sessions = Arc::new(Mutex::new(vec![initial]));
        let (local_catalog_tx, local_catalog_rx) = mpsc::channel();
        let runtime = RemoteNodeSessionSyncRuntime {
            gateway: MutableFakeGateway {
                sessions: sessions.clone(),
            },
            transport: transport.clone(),
            local_target_exit_observer: RecordingLocalTargetExitObserver::default(),
            network: RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: Some("node-a".to_string()),
                public_endpoint: None,
            },
            reconnect_delay: Duration::from_millis(100),
        };

        let guard = runtime
            .start_with_local_catalog_changes(local_catalog_rx)
            .expect("runtime should start");
        let first = wait_for_controlled_envelope(&transport.receivers, 0);
        let Some(Body::TargetPublished(first_payload)) = first.body else {
            panic!("expected initial target_published body");
        };
        assert_eq!(first_payload.command_name.as_deref(), Some("bash"));
        assert_eq!(first_payload.task_state.as_deref(), Some("input"));

        {
            let mut sessions = sessions
                .lock()
                .expect("fake sessions mutex should not be poisoned");
            sessions[0].command_name = Some("kimi".to_string());
            sessions[0].task_state = ManagedSessionTaskState::Running;
        }
        transport.close_session(0, "node-a", "server-session-1");
        // Wait until the reconnect has created the new outbound session before
        // injecting the local catalog change, so the change is definitely
        // processed against the new session (or while the reconnect loop is
        // waiting) rather than racing with the dying session.
        wait_for_controlled_receiver_count(&transport.receivers, 2);
        local_catalog_tx
            .send(super::super::LocalCatalogChangeRequest::notify(
                LocalCatalogChangeReason::LocalRuntimeChanged,
            ))
            .expect("local catalog change should send while reconnecting");
        let second = wait_for_controlled_envelope(&transport.receivers, 1);
        drop(guard);

        let Some(Body::TargetPublished(second_payload)) = second.body else {
            panic!("expected reconnected target_published body");
        };
        assert_eq!(second_payload.transport_session_id, "shell-1");
        assert_eq!(second_payload.node_instance_id, "server-session-2");
        assert_eq!(second_payload.command_name.as_deref(), Some("kimi"));
        assert_eq!(second_payload.task_state.as_deref(), Some("running"));
        assert_eq!(second_payload.revision, 2);
    }

    #[test]
    fn local_catalog_notify_wakes_sync_without_poll_wait() {
        let socket_name = format!("wa-test-sync-notify-{}", std::process::id());
        let socket_path = remote_session_sync_owner_socket_path(&socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).expect("owner socket should bind");
        let (local_catalog_tx, local_catalog_rx) = mpsc::channel();
        let (shutdown_tx, _shutdown_rx) = mpsc::channel();
        let _owner_worker =
            super::super::serve_owner_commands(listener, local_catalog_tx, shutdown_tx);

        let notifier = thread::spawn({
            let socket_path = socket_path.clone();
            move || {
                notify_remote_session_sync_owner(
                    &socket_path,
                    LocalCatalogChangeReason::LocalTargetExited {
                        target_session_name: "shell-1".to_string(),
                    },
                )
                .expect("notify should be acknowledged")
            }
        });

        let request = local_catalog_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("owner notify should arrive without polling");
        assert_eq!(
            request.reason,
            LocalCatalogChangeReason::LocalTargetExited {
                target_session_name: "shell-1".to_string(),
            }
        );
        request
            .ack_tx
            .expect("owner command should request processing ack")
            .send(Ok(super::super::LocalCatalogChangeAck::Queued))
            .expect("owner command ack should send");
        notifier.join().expect("notifier should join");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn local_catalog_notify_waits_for_owner_processing_ack() {
        let socket_name = format!("wa-test-sync-notify-ack-{}", std::process::id());
        let socket_path = remote_session_sync_owner_socket_path(&socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).expect("owner socket should bind");
        let (local_catalog_tx, local_catalog_rx) = mpsc::channel();
        let (shutdown_tx, _shutdown_rx) = mpsc::channel();
        let _owner_worker =
            super::super::serve_owner_commands(listener, local_catalog_tx, shutdown_tx);

        let notify_finished = Arc::new(AtomicBool::new(false));
        let notifier = thread::spawn({
            let socket_path = socket_path.clone();
            let notify_finished = notify_finished.clone();
            move || {
                notify_remote_session_sync_owner(
                    &socket_path,
                    LocalCatalogChangeReason::LocalRuntimeChanged,
                )
                .expect("notify should be acknowledged after processing");
                notify_finished.store(true, Ordering::SeqCst);
            }
        });

        let request = local_catalog_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("owner notify should arrive without polling");
        assert_eq!(
            request.reason,
            LocalCatalogChangeReason::LocalRuntimeChanged
        );
        thread::sleep(Duration::from_millis(50));
        assert!(
            !notify_finished.load(Ordering::SeqCst),
            "owner notify must wait for processing ack"
        );
        request
            .ack_tx
            .expect("owner command should request processing ack")
            .send(Ok(super::super::LocalCatalogChangeAck::Published))
            .expect("owner command ack should send");

        notifier.join().expect("notifier should join");
        assert!(notify_finished.load(Ordering::SeqCst));
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn local_catalog_signal_does_not_wait_for_owner_processing_ack() {
        let socket_name = format!("wa-test-sync-signal-no-ack-{}", std::process::id());
        let socket_path = remote_session_sync_owner_socket_path(&socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).expect("owner socket should bind");
        let (local_catalog_tx, local_catalog_rx) = mpsc::channel();
        let (shutdown_tx, _shutdown_rx) = mpsc::channel();
        let _owner_worker =
            super::super::serve_owner_commands(listener, local_catalog_tx, shutdown_tx);

        signal_remote_session_sync_owner(
            &socket_path,
            LocalCatalogChangeReason::LocalRuntimeChanged,
        )
        .expect("signal should write owner command without waiting for ack");

        let request = local_catalog_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("owner signal should arrive without polling");
        assert_eq!(
            request.reason,
            LocalCatalogChangeReason::LocalRuntimeChanged
        );
        assert!(
            request.ack_tx.is_some(),
            "owner still owns processing ack even when caller does not wait"
        );
        request
            .ack_tx
            .expect("owner command should request processing ack")
            .send(Ok(super::super::LocalCatalogChangeAck::Published))
            .expect("owner command ack should send");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn session_sync_authority_gateway_signals_local_runtime_change() {
        let socket_name = format!("wa-test-sync-gateway-{}", std::process::id());
        let socket_path = remote_session_sync_owner_socket_path(&socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).expect("owner socket should bind");
        let (local_catalog_tx, local_catalog_rx) = mpsc::channel();
        let (shutdown_tx, _shutdown_rx) = mpsc::channel();
        let _owner_worker =
            super::super::serve_owner_commands(listener, local_catalog_tx, shutdown_tx);
        let gateway = SessionSyncAuthorityPublicationGateway::new(RemoteNetworkConfig {
            port: 7474,
            connect: Some("127.0.0.1:7474".to_string()),
            node_id: None,
            public_endpoint: None,
        });

        gateway
            .signal_local_runtime_changed(&socket_name)
            .expect("runtime change should signal session sync owner");

        let request = local_catalog_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("runtime change signal should arrive without polling");
        assert_eq!(
            request.reason,
            LocalCatalogChangeReason::LocalRuntimeChanged
        );
        assert!(
            request.ack_tx.is_some(),
            "owner still owns processing ack even when gateway signal does not wait"
        );
        request
            .ack_tx
            .expect("owner command should request processing ack")
            .send(Ok(super::super::LocalCatalogChangeAck::Published))
            .expect("owner command ack should send");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn local_catalog_change_triggers_catalog_diff_sync_without_poll_wait() {
        let receiver_slot = Arc::new(Mutex::new(None));
        let sessions = Arc::new(Mutex::new(vec![session("wa-1", "shell-1")]));
        let (local_catalog_tx, local_catalog_rx) = mpsc::channel();
        let runtime = RemoteNodeSessionSyncRuntime {
            gateway: MutableFakeGateway {
                sessions: sessions.clone(),
            },
            transport: FakeTransport {
                receiver_slot: receiver_slot.clone(),
            },
            local_target_exit_observer: RecordingLocalTargetExitObserver::default(),
            network: RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            },
            reconnect_delay: Duration::from_millis(10),
        };

        let guard = runtime
            .start_with_local_catalog_changes(local_catalog_rx)
            .expect("runtime should start");
        let first = wait_for_envelope(&receiver_slot);
        let Some(Body::TargetPublished(payload)) = first.body else {
            panic!("expected initial target_published body");
        };
        assert_eq!(payload.transport_session_id, "shell-1");

        sessions
            .lock()
            .expect("fake sessions mutex should not be poisoned")
            .clear();
        local_catalog_tx
            .send(super::super::LocalCatalogChangeRequest::notify(
                LocalCatalogChangeReason::LocalTargetExited {
                    target_session_name: "shell-1".to_string(),
                },
            ))
            .expect("local catalog change should send");

        let second = wait_for_envelope(&receiver_slot);
        let Some(Body::TargetExited(payload)) = second.body else {
            panic!("expected target_exited body after catalog diff");
        };
        assert_eq!(payload.transport_session_id, "shell-1");
        drop(guard);
    }

    #[test]
    fn local_runtime_change_triggers_catalog_diff_sync_without_poll_wait() {
        let receiver_slot = Arc::new(Mutex::new(None));
        let mut initial = session("wa-1", "shell-1");
        initial.command_name = Some("bash".to_string());
        initial.current_path = Some(PathBuf::from("/tmp/start"));
        initial.task_state = ManagedSessionTaskState::Input;
        let sessions = Arc::new(Mutex::new(vec![initial]));
        let (local_catalog_tx, local_catalog_rx) = mpsc::channel();
        let runtime = RemoteNodeSessionSyncRuntime {
            gateway: MutableFakeGateway {
                sessions: sessions.clone(),
            },
            transport: FakeTransport {
                receiver_slot: receiver_slot.clone(),
            },
            local_target_exit_observer: RecordingLocalTargetExitObserver::default(),
            network: RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            },
            reconnect_delay: Duration::from_millis(10),
        };

        let guard = runtime
            .start_with_local_catalog_changes(local_catalog_rx)
            .expect("runtime should start");
        let first = wait_for_envelope(&receiver_slot);
        let Some(Body::TargetPublished(payload)) = first.body else {
            panic!("expected initial target_published body");
        };
        assert_eq!(payload.transport_session_id, "shell-1");
        assert_eq!(payload.command_name.as_deref(), Some("bash"));

        {
            let mut sessions = sessions
                .lock()
                .expect("fake sessions mutex should not be poisoned");
            sessions[0].command_name = Some("kimi".to_string());
            sessions[0].current_path = Some(PathBuf::from("/tmp/live"));
            sessions[0].task_state = ManagedSessionTaskState::Running;
        }
        local_catalog_tx
            .send(super::super::LocalCatalogChangeRequest::notify(
                LocalCatalogChangeReason::LocalRuntimeChanged,
            ))
            .expect("local runtime change should send");

        let second = wait_for_envelope(&receiver_slot);
        let Some(Body::TargetPublished(payload)) = second.body else {
            panic!("expected target_published body after runtime change");
        };
        assert_eq!(payload.transport_session_id, "shell-1");
        assert_eq!(payload.command_name.as_deref(), Some("kimi"));
        assert_eq!(payload.current_path.as_deref(), Some("/tmp/live"));
        assert_eq!(payload.revision, 2);
        drop(guard);
    }

    #[test]
    fn reconnect_wait_preserves_local_catalog_change_signal() {
        let (tx, rx) = mpsc::channel();
        tx.send(super::super::SessionSyncEvent::LocalCatalogChanged(
            super::super::LocalCatalogChangeRequest::notify(
                LocalCatalogChangeReason::LocalRuntimeChanged,
            ),
        ))
        .expect("local catalog event should send");

        match super::super::wait_for_reconnect_delay_or_stop(&rx, Duration::from_millis(1)) {
            super::super::ReconnectWaitOutcome::LocalCatalogChanged(request) => {
                assert_eq!(
                    request.reason,
                    LocalCatalogChangeReason::LocalRuntimeChanged
                );
            }
            other => panic!("expected local catalog change outcome, got {other:?}"),
        }
    }

    #[test]
    fn pending_runtime_change_after_session_open_publishes_latest_state() {
        let (handle, mut receiver) =
            RemoteNodeSessionHandle::new_for_tests("node-a", "server-session-1");
        let observer = RecordingLocalTargetExitObserver::default();
        let mut previous = session("wa-1", "shell-1");
        previous.command_name = Some("bash".to_string());
        previous.task_state = ManagedSessionTaskState::Input;
        let mut current = previous.clone();
        current.command_name = Some("codex".to_string());
        current.task_state = ManagedSessionTaskState::Input;
        let mut synced_sessions = local_sessions_by_local_id(vec![previous]);
        let mut next_message_id = 0;
        let mut publication_tracker = SourcePublicationTracker::new();
        publication_tracker.on_connected();

        let should_reconnect = super::super::sync_local_sessions_after_catalog_transport_event(
            &FakeGateway {
                sessions: vec![current],
            },
            "node-a",
            Some(&handle),
            &observer,
            &mut synced_sessions,
            &mut next_message_id,
            &mut publication_tracker,
            "pending local catalog change",
        );

        assert!(!should_reconnect);
        let envelope = receiver
            .try_recv()
            .expect("pending runtime change should publish latest state");
        let Some(Body::TargetPublished(payload)) = envelope.body else {
            panic!("expected target_published body");
        };
        assert_eq!(payload.transport_session_id, "shell-1");
        assert_eq!(payload.command_name.as_deref(), Some("codex"));
        assert_eq!(payload.task_state.as_deref(), Some("input"));
        assert_eq!(payload.revision, 1);
    }

    #[test]
    fn catalog_transport_event_syncs_newly_created_target_into_baseline() {
        let (handle, mut receiver) =
            RemoteNodeSessionHandle::new_for_tests("10.0.0.2", "server-session-1");
        let observer = RecordingLocalTargetExitObserver::default();
        let mut synced_sessions = local_sessions_by_local_id(vec![session("wa-1", "shell-1")]);
        let mut next_message_id = 0;
        let mut publication_tracker = SourcePublicationTracker::new();
        publication_tracker.on_connected();

        let should_reconnect = super::super::sync_local_sessions_after_catalog_transport_event(
            &FakeGateway {
                sessions: vec![session("wa-1", "shell-1"), session("wa-1", "shell-2")],
            },
            "10.0.0.2",
            Some(&handle),
            &observer,
            &mut synced_sessions,
            &mut next_message_id,
            &mut publication_tracker,
            "test create-session",
        );

        assert!(!should_reconnect);
        assert!(synced_sessions.contains_key("local-tmux:wa-1:shell-2"));
        let envelope = receiver
            .try_recv()
            .expect("newly created target should be published immediately");
        let Some(Body::TargetPublished(payload)) = envelope.body else {
            panic!("expected target_published body");
        };
        assert_eq!(payload.transport_session_id, "shell-2");
    }

    #[test]
    fn exportable_local_sessions_for_socket_keeps_workspace_sessions_on_current_socket() {
        let resolver = FakeResolver::default();
        let sessions = exportable_local_sessions_for_socket(
            vec![
                session_with_role("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
                session_with_role("wa-1", "shell-1", WorkspaceSessionRole::TargetHost),
                session_with_role("wa-2", "shell-2", WorkspaceSessionRole::TargetHost),
            ],
            "wa-1",
            &resolver,
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
            display_command_name: None,
            current_path: Some(PathBuf::from("/tmp/cached")),
            task_state: ManagedSessionTaskState::Running,
        };
        let resolver =
            FakeResolver::default().with_target("wa-1", "shell-1", remote_target.clone());

        let sessions = exportable_local_sessions_for_socket(vec![local_target], "wa-1", &resolver);

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
    fn overlay_workspace_runtime_does_not_override_explicit_target_runtime() {
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
        assert_eq!(projected.command_name.as_deref(), Some("bash"));
        assert_eq!(
            projected.current_path.as_deref(),
            Some(Path::new("/tmp/host"))
        );
        assert_eq!(projected.task_state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn overlay_workspace_runtime_does_not_override_confirm_target_runtime() {
        let sessions = overlay_workspace_runtime_onto_active_local_target_hosts(
            vec![
                ManagedSessionRecord {
                    command_name: Some("codex".to_string()),
                    current_path: Some(PathBuf::from("/tmp/workspace")),
                    task_state: ManagedSessionTaskState::Running,
                    ..session_with_role("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome)
                },
                ManagedSessionRecord {
                    command_name: Some("codex".to_string()),
                    current_path: Some(PathBuf::from("/tmp/host")),
                    task_state: ManagedSessionTaskState::Confirm,
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
            Some(Path::new("/tmp/host"))
        );
        assert_eq!(projected.task_state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn overlay_workspace_runtime_does_not_project_internal_waitagent_runtime() {
        let sessions = overlay_workspace_runtime_onto_active_local_target_hosts(
            vec![
                ManagedSessionRecord {
                    command_name: Some("waitagent".to_string()),
                    current_path: Some(PathBuf::from("/tmp/workspace")),
                    task_state: ManagedSessionTaskState::Running,
                    ..session_with_role("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome)
                },
                ManagedSessionRecord {
                    command_name: Some("bash".to_string()),
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
        assert_eq!(projected.command_name.as_deref(), Some("bash"));
        assert_eq!(
            projected.current_path.as_deref(),
            Some(Path::new("/tmp/target"))
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
    fn remote_session_sync_owner_available_requires_acknowledged_ping() {
        let socket_name = format!("wa-test-sync-owner-{}", std::process::id());
        let socket_path = remote_session_sync_owner_socket_path(&socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        assert!(!remote_session_sync_owner_available(&socket_path));
        let listener = UnixListener::bind(&socket_path).expect("owner socket should bind");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("ping client should connect");
            let mut request = String::new();
            stream
                .read_to_string(&mut request)
                .expect("ping request should read");
            assert_eq!(request.trim(), "ping");
            stream.write_all(b"ok\n").expect("ping ack should write");
        });
        assert!(remote_session_sync_owner_available(&socket_path));
        handle.join().expect("fake owner should join");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn remote_session_sync_owner_args_include_ready_socket_when_requested() {
        let network = RemoteNetworkConfig {
            port: 9001,
            connect: Some("10.0.0.8:7474".to_string()),
            node_id: None,
            public_endpoint: None,
        };

        let args = remote_session_sync_owner_args(
            "wa-1",
            &network,
            Some(Path::new("/tmp/sync-ready.sock")),
        );

        assert!(args.iter().any(|arg| arg == "--ready-socket"));
        assert!(args.iter().any(|arg| arg == "/tmp/sync-ready.sock"));
    }

    #[derive(Clone, Default)]
    struct RecordingLocalTargetExitObserver {
        exits: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl LocalTargetExitObserver for RecordingLocalTargetExitObserver {
        fn observe_local_target_exit(
            &self,
            socket_name: &str,
            target_session_name: &str,
        ) -> Result<(), LifecycleError> {
            self.exits
                .lock()
                .expect("exit observer mutex should not be poisoned")
                .push((socket_name.to_string(), target_session_name.to_string()));
            Ok(())
        }
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
    struct MutableFakeGateway {
        sessions: Arc<Mutex<Vec<ManagedSessionRecord>>>,
    }

    impl LocalSessionCatalog for MutableFakeGateway {
        type Error = &'static str;

        fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            Ok(self
                .sessions
                .lock()
                .expect("fake sessions mutex should not be poisoned")
                .clone())
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

    struct FakeGuard {
        _event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    }

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
            Ok(FakeGuard {
                _event_tx: event_tx,
            })
        }
    }

    #[derive(Clone, Default)]
    struct ControlledReconnectTransport {
        receivers: Arc<
            Mutex<
                Vec<
                    tokio::sync::mpsc::UnboundedReceiver<
                        crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope,
                    >,
                >,
            >,
        >,
        event_txs: Arc<Mutex<Vec<mpsc::Sender<RemoteNodeTransportEvent>>>>,
        connect_count: Arc<AtomicUsize>,
    }

    struct ControlledReconnectGuard {
        _event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    }

    impl ControlledReconnectTransport {
        fn close_session(&self, index: usize, node_id: &str, session_instance_id: &str) {
            let event_tx = self
                .event_txs
                .lock()
                .expect("controlled transport event tx mutex should not be poisoned")
                .get(index)
                .cloned()
                .expect("controlled transport session should exist");
            event_tx
                .send(RemoteNodeTransportEvent::SessionClosed {
                    node_id: node_id.to_string(),
                    session_instance_id: session_instance_id.to_string(),
                })
                .expect("controlled transport close event should send");
        }
    }

    impl OutboundRemoteNodeTransport for ControlledReconnectTransport {
        type Guard = ControlledReconnectGuard;
        type Error = &'static str;

        fn connect_outbound(
            &self,
            request: OutboundNodeSessionRequest,
            event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
        ) -> Result<Self::Guard, Self::Error> {
            let session_index = self.connect_count.fetch_add(1, Ordering::SeqCst) + 1;
            let session_instance_id = format!("server-session-{session_index}");
            let (handle, receiver) =
                RemoteNodeSessionHandle::new_for_tests(request.node_id, session_instance_id);
            self.receivers
                .lock()
                .expect("controlled transport receiver mutex should not be poisoned")
                .push(receiver);
            self.event_txs
                .lock()
                .expect("controlled transport event tx mutex should not be poisoned")
                .push(event_tx.clone());
            event_tx
                .send(RemoteNodeTransportEvent::SessionOpened { session: handle })
                .map_err(|_| "failed to deliver session open event")?;
            Ok(ControlledReconnectGuard {
                _event_tx: event_tx,
            })
        }
    }

    fn open_mirror_command(target_id: &str, session_id: &str) -> RemoteAuthorityCommand {
        RemoteAuthorityCommand::OpenMirror(OpenMirrorRequestPayload {
            session_id: session_id.to_string(),
            target_id: target_id.to_string(),
            console_id: "console-a".to_string(),
            cols: 80,
            rows: 24,
            raw_pty_passthrough: true,
            bootstrap_mode: BootstrapMode::Full,
        })
    }

    fn wait_for_envelope(
        receiver_slot: &Arc<
            Mutex<
                Option<
                    tokio::sync::mpsc::UnboundedReceiver<
                        crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope,
                    >,
                >,
            >,
        >,
    ) -> crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope {
        let start = std::time::Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("timed out waiting for outbound session sync envelope");
            }
            if let Some(envelope) = try_take_envelope(receiver_slot) {
                return envelope;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_controlled_envelope(
        receivers: &Arc<
            Mutex<
                Vec<
                    tokio::sync::mpsc::UnboundedReceiver<
                        crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope,
                    >,
                >,
            >,
        >,
        index: usize,
    ) -> crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope {
        let start = std::time::Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("timed out waiting for controlled outbound session sync envelope");
            }
            let envelope = receivers
                .lock()
                .expect("controlled transport receiver mutex should not be poisoned")
                .get_mut(index)
                .and_then(|receiver| receiver.try_recv().ok());
            if let Some(envelope) = envelope {
                return envelope;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_controlled_receiver_count(
        receivers: &Arc<
            Mutex<
                Vec<
                    tokio::sync::mpsc::UnboundedReceiver<
                        crate::infra::remote_grpc_proto::v1::NodeSessionEnvelope,
                    >,
                >,
            >,
        >,
        expected: usize,
    ) {
        let start = std::time::Instant::now();
        loop {
            let count = receivers
                .lock()
                .expect("controlled transport receiver mutex should not be poisoned")
                .len();
            if count >= expected {
                return;
            }
            if start.elapsed() > Duration::from_secs(1) {
                panic!(
                    "timed out waiting for controlled transport receiver count {expected}, saw {count}"
                );
            }
            thread::sleep(Duration::from_millis(10));
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

    #[derive(Clone, Default)]
    struct FakeResolver {
        targets: HashMap<(String, String), Vec<ManagedSessionRecord>>,
    }

    impl FakeResolver {
        fn with_target(
            mut self,
            socket_name: &str,
            session_name: &str,
            target: ManagedSessionRecord,
        ) -> Self {
            self.targets
                .entry((socket_name.to_string(), session_name.to_string()))
                .or_default()
                .push(target);
            self
        }
    }

    impl RemoteTargetSourceBindingResolver for FakeResolver {
        fn list_remote_targets_for_source_binding(
            &self,
            source_socket_name: &str,
            source_session_name: &str,
        ) -> Result<Vec<ManagedSessionRecord>, LifecycleError> {
            Ok(self
                .targets
                .get(&(
                    source_socket_name.to_string(),
                    source_session_name.to_string(),
                ))
                .cloned()
                .unwrap_or_default())
        }
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
            display_command_name: None,
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
