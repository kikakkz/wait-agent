mod tests {
    use super::super::{
        apply_owner_lifecycle_event, discover_authority_socket_paths, enqueue_ingress_event,
        extract_target_component, handle_internal_event, has_active_ingress_session_for_node,
        next_ingress_event, remote_node_ingress_owner_args, route_transport_envelope,
        ActiveAuthoritySocketBridge, ActiveNodeIngressSession, IngressServerEvent, InternalEvent,
        OwnerLifecycleEvent, RemoteNodeIngressServerRuntime,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_proto::v1::{
        MirrorBootstrapChunk, MirrorBootstrapComplete, NodeSessionEnvelope, RouteContext,
        TargetOutput,
    };
    use crate::infra::remote_grpc_transport::RemoteNodeSessionHandle;
    use crate::infra::remote_protocol::{
        BootstrapMode, ControlPlanePayload, OpenMirrorRequestPayload, ProtocolEnvelope,
        RawPtyOutputPayload, REMOTE_PROTOCOL_VERSION,
    };
    use crate::infra::remote_transport_codec::write_control_plane_envelope;
    use crate::runtime::remote_authority_transport_runtime::{
        authority_transport_socket_path, RemoteAuthorityTransportRuntime,
    };
    use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
    use std::collections::{BTreeSet, VecDeque};
    use std::fs;
    use std::net::Shutdown;
    use std::path::PathBuf;
    use std::process;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn owner_registry_empty_snapshot_does_not_clear_saw_workspace() {
        let mut live_workspace_sockets = BTreeSet::from(["wa-live".to_string()]);
        let mut saw_workspace = true;

        apply_owner_lifecycle_event(
            &mut live_workspace_sockets,
            &mut saw_workspace,
            OwnerLifecycleEvent::WorkspaceRegistryChanged(BTreeSet::new()),
        );

        assert!(saw_workspace);
        assert!(live_workspace_sockets.is_empty());
    }

    #[test]
    fn local_create_session_event_is_prioritized_over_periodic_publication() {
        let (_reply_tx, reply_rx) = mpsc::channel();
        let mut high = VecDeque::new();
        let mut low = VecDeque::new();

        enqueue_ingress_event(
            &mut high,
            &mut low,
            IngressServerEvent::Transport(
                crate::infra::remote_grpc_transport::RemoteNodeTransportEvent::EnvelopeReceived {
                    node_id: "peer-a".to_string(),
                    session_instance_id: "session-1".to_string(),
                    envelope: target_published_envelope_for("shell-a"),
                },
            ),
        );
        enqueue_ingress_event(
            &mut high,
            &mut low,
            IngressServerEvent::Internal(InternalEvent::LocalCreateSession {
                envelope: create_session_request_envelope("request-1"),
                reply_tx: _reply_tx,
            }),
        );

        let first = next_ingress_event(&mut high, &mut low).expect("first event should exist");
        match first {
            IngressServerEvent::Internal(InternalEvent::LocalCreateSession {
                envelope, ..
            }) => {
                assert_eq!(
                    grpc_create_request_id(&envelope).as_deref(),
                    Some("request-1")
                );
            }
            _ => panic!("local create-session should be handled before publication sync"),
        }
        drop(reply_rx);
    }

    #[test]
    fn register_workspace_socket_event_updates_ingress_registry() {
        let mut sessions = std::collections::HashMap::new();
        let mut registered = BTreeSet::new();
        let (internal_tx, _internal_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let mut retry_scheduled = false;

        handle_internal_event(
            &mut sessions,
            &mut registered,
            internal_tx,
            &mut retry_scheduled,
            InternalEvent::RegisterWorkspaceSocket {
                socket_name: "wa-test".to_string(),
                reply_tx,
            },
        );

        assert!(registered.contains("wa-test"));
        assert!(reply_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok());
    }

    #[test]
    fn unregister_workspace_socket_event_updates_ingress_registry() {
        let mut sessions = std::collections::HashMap::new();
        let mut registered = BTreeSet::from(["wa-test".to_string()]);
        let (internal_tx, _internal_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let mut retry_scheduled = false;

        handle_internal_event(
            &mut sessions,
            &mut registered,
            internal_tx,
            &mut retry_scheduled,
            InternalEvent::UnregisterWorkspaceSocket {
                socket_name: "wa-test".to_string(),
                reply_tx,
            },
        );

        assert!(!registered.contains("wa-test"));
        assert!(reply_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok());
    }

    #[test]
    fn owner_lifecycle_exits_after_last_registered_workspace_unregisters() {
        let mut live_workspace_sockets = BTreeSet::new();
        let mut saw_workspace = false;

        apply_owner_lifecycle_event(
            &mut live_workspace_sockets,
            &mut saw_workspace,
            OwnerLifecycleEvent::WorkspaceRegistered("wa-test".to_string()),
        );
        assert!(saw_workspace);
        assert_eq!(
            live_workspace_sockets.into_iter().collect::<Vec<_>>(),
            vec!["wa-test".to_string()]
        );

        let mut live_workspace_sockets = BTreeSet::from(["wa-test".to_string()]);
        apply_owner_lifecycle_event(
            &mut live_workspace_sockets,
            &mut saw_workspace,
            OwnerLifecycleEvent::WorkspaceUnregistered("wa-test".to_string()),
        );
        assert!(saw_workspace);
        assert!(live_workspace_sockets.is_empty());
    }

    #[test]
    fn socket_discovery_event_is_prioritized_over_periodic_publication() {
        let mut high = VecDeque::new();
        let mut low = VecDeque::new();

        enqueue_ingress_event(
            &mut high,
            &mut low,
            IngressServerEvent::Transport(
                crate::infra::remote_grpc_transport::RemoteNodeTransportEvent::EnvelopeReceived {
                    node_id: "peer-a".to_string(),
                    session_instance_id: "session-1".to_string(),
                    envelope: target_published_envelope_for("shell-a"),
                },
            ),
        );
        enqueue_ingress_event(
            &mut high,
            &mut low,
            IngressServerEvent::Internal(InternalEvent::SocketDirChanged),
        );

        let first = next_ingress_event(&mut high, &mut low).expect("first event should exist");
        assert!(matches!(
            first,
            IngressServerEvent::Internal(InternalEvent::SocketDirChanged)
        ));
    }

    #[test]
    fn socket_discovery_retry_is_coalesced_while_timer_is_pending() {
        use super::super::schedule_socket_discovery_retry;
        let (internal_tx, internal_rx) = mpsc::channel();
        let mut retry_scheduled = false;

        schedule_socket_discovery_retry(internal_tx.clone(), 2, &mut retry_scheduled);
        assert!(retry_scheduled);
        schedule_socket_discovery_retry(internal_tx, 2, &mut retry_scheduled);

        assert!(internal_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok());
        assert!(internal_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
    }

    #[test]
    fn authority_socket_ready_reply_reports_registration_state() {
        use super::super::{
            authority_socket_ready_reply, AuthoritySocketReadyStatus, BridgeRefreshOutcome,
        };
        let path = PathBuf::from("/tmp/waitagent-authority.sock");

        assert_eq!(
            authority_socket_ready_reply(
                "peer-a",
                &path,
                BridgeRefreshOutcome {
                    connected: 1,
                    pending: 0,
                    already_registered: 0,
                    invalid: 0,
                },
            )
            .status,
            AuthoritySocketReadyStatus::Registered
        );
        assert_eq!(
            authority_socket_ready_reply(
                "peer-a",
                &path,
                BridgeRefreshOutcome {
                    connected: 0,
                    pending: 1,
                    already_registered: 0,
                    invalid: 0,
                },
            )
            .status,
            AuthoritySocketReadyStatus::Pending
        );
        assert_eq!(
            authority_socket_ready_reply(
                "peer-a",
                &path,
                BridgeRefreshOutcome {
                    connected: 0,
                    pending: 0,
                    already_registered: 0,
                    invalid: 1,
                },
            )
            .status,
            AuthoritySocketReadyStatus::Error
        );
    }

    #[test]
    fn extracts_target_component_for_authority_socket_file() {
        let _guard = crate::test_support::integration_test_lock();
        let component = extract_target_component(
            "waitagent-remote-65fb3bc8828a0a50-b1df888881737297-d8273c888e3c986c.sock",
            "peer-a",
        );

        assert_eq!(component.as_deref(), Some("d8273c888e3c986c"));
    }

    #[test]
    fn extracts_target_component_for_scoped_remote_main_slot_socket_file() {
        let _guard = crate::test_support::integration_test_lock();
        let component = extract_target_component(
            "waitagent-remote-b27520f164626822-b1df888881737297-d8273c888e3c986c.sock",
            "peer-a",
        );

        assert_eq!(component.as_deref(), Some("d8273c888e3c986c"));
    }

    #[test]
    fn authority_socket_discovery_filters_to_authority() {
        let _guard = crate::test_support::integration_test_lock();
        // Clean up any stray files from other tests that use the same authority hash
        for entry in fs::read_dir(std::env::temp_dir()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.contains("-b1df888881737297-") || name.contains("-b1df89888173744a-") {
                let _ = fs::remove_file(entry.path());
            }
        }

        let matching_a =
            temp_dir_path("waitagent-remote-b1ec3ae6b7a67e00-b1df888881737297-d8273c888e3c986c");
        let matching_b =
            temp_dir_path("waitagent-remote-44df01ed9b438425-b1df888881737297-d8273f888e3c9d85");
        let matching_scoped =
            temp_dir_path("waitagent-remote-926d755099191094-b1df888881737297-d8273e888e3c9bd2");
        let different_authority =
            temp_dir_path("waitagent-remote-ebb26774420f3fb2-b1df89888173744a-0082fb09e9ea2a17");
        fs::write(&matching_a, b"").expect("matching file should write");
        fs::write(&matching_b, b"").expect("second matching file should write");
        fs::write(&matching_scoped, b"").expect("scoped matching file should write");
        fs::write(&different_authority, b"").expect("other authority file should write");

        let paths = discover_authority_socket_paths("peer-a")
            .expect("authority socket discovery should succeed");
        assert!(paths.contains(&matching_a));
        assert!(paths.contains(&matching_b));
        assert!(paths.contains(&matching_scoped));
        assert!(!paths.contains(&different_authority));

        let _ = fs::remove_file(matching_a);
        let _ = fs::remove_file(matching_b);
        let _ = fs::remove_file(matching_scoped);
        let _ = fs::remove_file(different_authority);
    }

    #[test]
    fn ingress_runtime_is_explicitly_scoped_to_one_workspace_socket() {
        let _guard = crate::test_support::integration_test_lock();
        let runtime = RemoteNodeIngressServerRuntime::from_build_env_with_network_and_socket(
            RemoteNetworkConfig::default(),
            "wa-socket-a",
        )
        .expect("runtime should build");

        let _ = runtime;
    }

    #[test]
    fn ingress_server_bridges_bootstrap_and_output_into_live_authority_socket() {
        let _guard = crate::test_support::integration_test_lock();
        let node_id = "peer-a";
        let socket_path =
            temp_dir_path("waitagent-remote-39cc9903ed327149-b1df888881737297-19fb7615081f4059");
        let socket_path_for_accept = socket_path.clone();
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("authority socket should bind");
        let accept_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("authority client should connect");
            crate::runtime::remote_node_transport_runtime::read_client_hello(&mut stream)
                .expect("client hello should decode");
            crate::runtime::remote_node_transport_runtime::write_server_hello(
                &mut stream,
                "waitagent-test",
            )
            .expect("server hello should encode");
            let reader = stream.try_clone().expect("stream clone should succeed");
            (reader, stream)
        });

        let transport = Arc::new(
            RemoteAuthorityTransportRuntime::connect(&socket_path, node_id)
                .expect("bridge transport should connect"),
        );
        let active_session_handle = RemoteNodeSessionHandle::new_for_tests(node_id, "session-1").0;
        let mut active = ActiveNodeIngressSession {
            session: active_session_handle,
            bridges: std::collections::HashMap::from([(
                socket_path.clone(),
                ActiveAuthoritySocketBridge {
                    target_component: "19fb7615081f4059".to_string(),
                    transport: transport.clone(),
                },
            )]),
            published_fingerprints: std::collections::HashMap::new(),
        };
        let publication_runtime =
            RemoteTargetPublicationRuntime::new_for_route_tests_without_remote_runtime_owner()
                .expect("publication runtime should build");

        route_transport_envelope(
            &publication_runtime,
            node_id,
            mirror_bootstrap_chunk_envelope(),
            Some(&mut active),
            &BTreeSet::new(),
            &mut super::super::ReceiverPublicationRevisionTable::default(),
        )
        .expect("bootstrap chunk should route");
        route_transport_envelope(
            &publication_runtime,
            node_id,
            mirror_bootstrap_complete_envelope(),
            Some(&mut active),
            &BTreeSet::new(),
            &mut super::super::ReceiverPublicationRevisionTable::default(),
        )
        .expect("bootstrap complete should route");
        route_transport_envelope(
            &publication_runtime,
            node_id,
            target_output_envelope(),
            Some(&mut active),
            &BTreeSet::new(),
            &mut super::super::ReceiverPublicationRevisionTable::default(),
        )
        .expect("target output should route");

        let (mut authority_stream, authority_writer) =
            accept_thread.join().expect("accept thread should join");
        let bootstrap_chunk = crate::infra::remote_transport_codec::read_control_plane_envelope(
            &mut authority_stream,
        )
        .expect("bootstrap chunk should arrive");
        match bootstrap_chunk.payload {
            crate::infra::remote_protocol::ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                assert_eq!(payload.session_id, "shell-1");
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.chunk_seq, 1);
                assert_eq!(payload.output_bytes, b"bootstrap");
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        let bootstrap_complete = crate::infra::remote_transport_codec::read_control_plane_envelope(
            &mut authority_stream,
        )
        .expect("bootstrap complete should arrive");
        match bootstrap_complete.payload {
            crate::infra::remote_protocol::ControlPlanePayload::MirrorBootstrapComplete(
                payload,
            ) => {
                assert_eq!(payload.session_id, "shell-1");
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.last_chunk_seq, 1);
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        let target_output = crate::infra::remote_transport_codec::read_control_plane_envelope(
            &mut authority_stream,
        )
        .expect("target output should arrive");
        match target_output.payload {
            crate::infra::remote_protocol::ControlPlanePayload::TargetOutput(payload) => {
                assert_eq!(payload.session_id, "shell-1");
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.output_seq, 7);
                assert_eq!(payload.output_bytes, b"a");
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        let _ = authority_stream.shutdown(Shutdown::Both);
        let _ = authority_writer.shutdown(Shutdown::Both);
        let _ = fs::remove_file(socket_path_for_accept);
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn target_exited_routes_only_to_matching_authority_bridge() {
        let _guard = crate::test_support::integration_test_lock();
        let node_id = unique_node_id("peer-target-exit");
        let socket_path_a = authority_transport_socket_path(
            "test-socket-a",
            "test-session-a",
            &format!("remote-peer:{node_id}:shell-a"),
        );
        let socket_path_b = authority_transport_socket_path(
            "test-socket-b",
            "test-session-b",
            &format!("remote-peer:{node_id}:shell-b"),
        );

        let listener_a = std::os::unix::net::UnixListener::bind(&socket_path_a)
            .expect("authority socket a should bind");
        let listener_b = std::os::unix::net::UnixListener::bind(&socket_path_b)
            .expect("authority socket b should bind");
        let accept_a = std::thread::spawn(move || collect_authority_connections(listener_a));
        let accept_b = std::thread::spawn(move || collect_authority_connections(listener_b));

        let transport_a = Arc::new(
            RemoteAuthorityTransportRuntime::connect(&socket_path_a, &node_id)
                .expect("bridge transport a should connect"),
        );
        let transport_b = Arc::new(
            RemoteAuthorityTransportRuntime::connect(&socket_path_b, &node_id)
                .expect("bridge transport b should connect"),
        );
        let active_session_handle =
            RemoteNodeSessionHandle::new_for_tests(node_id.clone(), "session-1").0;
        let mut active = ActiveNodeIngressSession {
            session: active_session_handle,
            bridges: std::collections::HashMap::from([
                (
                    socket_path_a.clone(),
                    ActiveAuthoritySocketBridge {
                        target_component: crate::runtime::remote_authority_transport_runtime::authority_target_component(&node_id, "shell-a"),
                        transport: transport_a,
                    },
                ),
                (
                    socket_path_b.clone(),
                    ActiveAuthoritySocketBridge {
                        target_component: crate::runtime::remote_authority_transport_runtime::authority_target_component(&node_id, "shell-b"),
                        transport: transport_b,
                    },
                ),
            ]),
            published_fingerprints: std::collections::HashMap::new(),
        };
        let publication_runtime =
            RemoteTargetPublicationRuntime::new_for_route_tests_without_remote_runtime_owner()
                .expect("publication runtime should build");

        route_transport_envelope(
            &publication_runtime,
            &node_id,
            target_exited_envelope_for("shell-a"),
            Some(&mut active),
            &BTreeSet::new(),
            &mut super::super::ReceiverPublicationRevisionTable::default(),
        )
        .expect("target exited should route");

        let mut writers_a = accept_a.join().expect("accept a should join");
        let mut writers_b = accept_b.join().expect("accept b should join");

        let forwarded_a = read_envelope_from_any(&mut writers_a)
            .expect("matching authority bridge should receive target exited");
        match forwarded_a.payload {
            crate::infra::remote_protocol::ControlPlanePayload::TargetExited(payload) => {
                assert_eq!(payload.transport_session_id, "shell-a");
            }
            other => panic!("unexpected payload on matching bridge: {other:?}"),
        }

        assert!(
            read_envelope_from_any(&mut writers_b).is_none(),
            "non-matching authority bridge should not receive target exited"
        );

        for writer in &writers_a {
            let _ = writer.shutdown(Shutdown::Both);
        }
        for writer in &writers_b {
            let _ = writer.shutdown(Shutdown::Both);
        }
        let _ = fs::remove_file(socket_path_a);
        let _ = fs::remove_file(socket_path_b);
    }

    #[test]
    fn revision_table_acks_and_rejects_stale_target_publications() {
        let node_id = "peer-revision";
        let (session_handle, mut outbound_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id, "session-1");
        let mut active = ActiveNodeIngressSession {
            session: session_handle,
            bridges: std::collections::HashMap::new(),
            published_fingerprints: std::collections::HashMap::new(),
        };
        let publication_runtime =
            RemoteTargetPublicationRuntime::new_for_route_tests_without_remote_runtime_owner()
                .expect("publication runtime should build");
        let mut revisions = super::super::ReceiverPublicationRevisionTable::default();

        route_transport_envelope(
            &publication_runtime,
            node_id,
            target_published_envelope_with_revision("shell-rev", 2, Some("codex")),
            Some(&mut active),
            &BTreeSet::new(),
            &mut revisions,
        )
        .expect("new revision should apply");
        let ack = outbound_rx
            .try_recv()
            .expect("applied publication should be acked");
        assert_publication_ack(&ack, 2, "applied");

        route_transport_envelope(
            &publication_runtime,
            node_id,
            target_published_envelope_with_revision("shell-rev", 1, Some("bash")),
            Some(&mut active),
            &BTreeSet::new(),
            &mut revisions,
        )
        .expect("stale revision should be rejected without failing route");
        let ack = outbound_rx
            .try_recv()
            .expect("stale publication should be acked");
        assert_publication_ack(&ack, 1, "stale_revision");

        route_transport_envelope(
            &publication_runtime,
            node_id,
            target_exited_envelope_with_revision("shell-rev", 1),
            Some(&mut active),
            &BTreeSet::new(),
            &mut revisions,
        )
        .expect("stale exit revision should be rejected without failing route");
        let ack = outbound_rx.try_recv().expect("stale exit should be acked");
        assert_publication_ack(&ack, 1, "stale_revision");
    }

    #[test]
    fn session_closed_removes_only_matching_session_instance() {
        let _guard = crate::test_support::integration_test_lock();
        use super::super::run_node_ingress_server_loop;
        use crate::infra::remote_grpc_transport::{
            RemoteNodeSessionHandle, RemoteNodeTransportEvent,
        };
        use std::sync::mpsc;
        use std::thread;

        let node_id = unique_node_id("peer-session-close");
        let old_socket_path = authority_transport_socket_path(
            "test-socket-old",
            "test-session-old",
            &format!("remote-peer:{node_id}:shell-old"),
        );
        let new_socket_path = authority_transport_socket_path(
            "test-socket-new",
            "test-session-new",
            &format!("remote-peer:{node_id}:shell-new"),
        );

        let old_listener = std::os::unix::net::UnixListener::bind(&old_socket_path)
            .expect("old authority socket should bind");
        let new_listener = std::os::unix::net::UnixListener::bind(&new_socket_path)
            .expect("new authority socket should bind");

        let old_accept = thread::spawn(move || collect_authority_connections(old_listener));
        let new_accept = thread::spawn(move || collect_authority_connections(new_listener));

        let publication_runtime =
            RemoteTargetPublicationRuntime::new_for_route_tests_without_remote_runtime_owner()
                .expect("publication runtime should build");
        let (transport_tx, transport_rx) = mpsc::channel();
        let (internal_tx, internal_rx) = mpsc::channel();
        let worker_internal_tx = internal_tx.clone();
        let worker = thread::spawn(move || {
            run_node_ingress_server_loop(
                publication_runtime,
                RemoteNetworkConfig::default(),
                transport_rx,
                internal_rx,
                worker_internal_tx,
                false,
            );
        });

        let (old_session, _old_outbound_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id.clone(), "session-old");
        let (new_session, _new_outbound_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id.clone(), "session-new");

        transport_tx
            .send(RemoteNodeTransportEvent::SessionOpened {
                session: old_session.clone(),
            })
            .expect("old session should open");
        transport_tx
            .send(RemoteNodeTransportEvent::SessionOpened {
                session: new_session.clone(),
            })
            .expect("new session should open");
        internal_tx
            .send(InternalEvent::SocketDirChanged)
            .expect("authority socket event should send");
        let old_writers = old_accept.join().expect("old accept should join");
        let mut new_writers = new_accept.join().expect("new accept should join");
        assert!(
            !new_writers.is_empty(),
            "new session should have established at least one authority bridge"
        );

        transport_tx
            .send(RemoteNodeTransportEvent::SessionClosed {
                node_id: node_id.clone(),
                session_instance_id: "session-old".to_string(),
            })
            .expect("old session should close");
        transport_tx
            .send(RemoteNodeTransportEvent::EnvelopeReceived {
                node_id: node_id.clone(),
                session_instance_id: "session-new".to_string(),
                envelope: NodeSessionEnvelope {
                    message_id: "target-output-after-old-close".to_string(),
                    sent_at: None,
                    session_instance_id: "session-new".to_string(),
                    correlation_id: None,
                    route: Some(RouteContext {
                        authority_node_id: Some(node_id.clone()),
                        target_id: Some(format!("remote-peer:{node_id}:shell-new")),
                        attachment_id: None,
                        console_id: None,
                        console_host_id: None,
                        session_id: Some("shell-new".to_string()),
                    }),
                    body: Some(Body::TargetOutput(TargetOutput {
                        target_id: format!("remote-peer:{node_id}:shell-new"),
                        output_seq: 9,
                        stream: "pty".to_string(),
                        session_id: "shell-new".to_string(),
                        output_bytes: b"survives".to_vec(),
                    })),
                },
            })
            .expect("new session envelope should route");

        let forwarded = read_target_output_from_any(&mut new_writers)
            .expect("new authority socket should still receive output");
        match forwarded.payload {
            crate::infra::remote_protocol::ControlPlanePayload::TargetOutput(payload) => {
                assert_eq!(payload.session_id, "shell-new");
                assert_eq!(payload.output_seq, 9);
                assert_eq!(payload.output_bytes, b"survives");
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        for writer in &old_writers {
            let _ = writer.shutdown(Shutdown::Both);
        }
        for writer in &new_writers {
            let _ = writer.shutdown(Shutdown::Both);
        }
        internal_tx
            .send(InternalEvent::Shutdown)
            .expect("shutdown event should send");
        drop(transport_tx);
        drop(internal_tx);
        let _ = worker.join();
        let _ = fs::remove_file(old_socket_path);
        let _ = fs::remove_file(new_socket_path);
    }

    #[test]
    fn authority_bridge_commands_are_routed_back_to_their_bound_session() {
        let _guard = crate::test_support::integration_test_lock();
        use super::super::run_node_ingress_server_loop;
        use crate::infra::remote_grpc_transport::{
            RemoteNodeSessionHandle, RemoteNodeTransportEvent,
        };
        use std::sync::mpsc;
        use std::thread;

        let node_id = unique_node_id("peer-command-reconnect");
        let socket_path = authority_transport_socket_path(
            "test-socket-command",
            "test-session-command",
            &format!("remote-peer:{node_id}:shell-command"),
        );
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("socket should bind");
        let accept = thread::spawn(move || collect_authority_connections(listener));

        let publication_runtime =
            RemoteTargetPublicationRuntime::new_for_route_tests_without_remote_runtime_owner()
                .expect("publication runtime should build");
        let (transport_tx, transport_rx) = mpsc::channel();
        let (internal_tx, internal_rx) = mpsc::channel();
        let worker_internal_tx = internal_tx.clone();
        let worker = thread::spawn(move || {
            run_node_ingress_server_loop(
                publication_runtime,
                RemoteNetworkConfig::default(),
                transport_rx,
                internal_rx,
                worker_internal_tx,
                false,
            );
        });

        let (old_session, mut old_outbound_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id.clone(), "session-old");
        let (new_session, mut new_outbound_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id.clone(), "session-new");
        transport_tx
            .send(RemoteNodeTransportEvent::SessionOpened {
                session: old_session,
            })
            .expect("old session should open");
        internal_tx
            .send(InternalEvent::SocketDirChanged)
            .expect("authority socket event should send");

        transport_tx
            .send(RemoteNodeTransportEvent::SessionOpened {
                session: new_session,
            })
            .expect("new session should open");

        let mut writers = accept.join().expect("accept should join");
        assert_eq!(writers.len(), 2, "each session should get its own bridge");

        // Bridge order is not deterministic, so identify each bridge by writing a command
        // and observing which outbound receiver receives it.
        let (old_bridge_index, new_bridge_index) = {
            write_control_plane_envelope(
                &mut writers[0],
                &open_mirror_request_envelope(
                    &format!("remote-peer:{node_id}:shell-command"),
                    "shell-command",
                ),
            )
            .expect("probe command should write");
            let probe = recv_outbound_with_deadline(&mut old_outbound_rx)
                .or_else(|| recv_outbound_with_deadline(&mut new_outbound_rx))
                .expect("probe command should reach exactly one session");
            if probe.session_instance_id == "session-old" {
                (0, 1)
            } else {
                (1, 0)
            }
        };

        // Command on the old bridge routes back to the old session.
        write_control_plane_envelope(
            &mut writers[old_bridge_index],
            &open_mirror_request_envelope(
                &format!("remote-peer:{node_id}:shell-command"),
                "shell-command",
            ),
        )
        .expect("old bridge command should write");
        let forwarded = recv_outbound_with_deadline(&mut old_outbound_rx)
            .expect("command on old bridge should reach old session");
        assert_eq!(forwarded.session_instance_id, "session-old");

        // Command on the new bridge routes back to the new session.
        write_control_plane_envelope(
            &mut writers[new_bridge_index],
            &open_mirror_request_envelope(
                &format!("remote-peer:{node_id}:shell-command"),
                "shell-command",
            ),
        )
        .expect("new bridge command should write");
        let forwarded = recv_outbound_with_deadline(&mut new_outbound_rx)
            .expect("command on new bridge should reach new session");
        assert_eq!(forwarded.session_instance_id, "session-new");

        // After the old session closes, commands on its bridge are dropped.
        transport_tx
            .send(RemoteNodeTransportEvent::SessionClosed {
                node_id: node_id.clone(),
                session_instance_id: "session-old".to_string(),
            })
            .expect("old session should close");
        write_control_plane_envelope(
            &mut writers[old_bridge_index],
            &open_mirror_request_envelope(
                &format!("remote-peer:{node_id}:shell-command"),
                "shell-command",
            ),
        )
        .expect("post-close command should write");
        assert!(
            old_outbound_rx.try_recv().is_err(),
            "command must not be sent through closed session"
        );
        assert!(
            new_outbound_rx.try_recv().is_err(),
            "command on old bridge must not leak to new session"
        );

        for writer in &writers {
            let _ = writer.shutdown(Shutdown::Both);
        }
        internal_tx
            .send(InternalEvent::Shutdown)
            .expect("shutdown event should send");
        drop(transport_tx);
        drop(internal_tx);
        let _ = worker.join();
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn authority_host_output_follows_latest_active_session_after_reconnect() {
        let node_id = "peer-output-reconnect";
        let (old_session, mut old_outbound_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id, "session-old");
        let (new_session, mut new_outbound_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id, "session-new");
        let mut sessions = std::collections::HashMap::from([
            (
                "session-old".to_string(),
                ActiveNodeIngressSession {
                    session: old_session,
                    bridges: std::collections::HashMap::new(),
                    published_fingerprints: std::collections::HashMap::new(),
                },
            ),
            (
                "session-new".to_string(),
                ActiveNodeIngressSession {
                    session: new_session,
                    bridges: std::collections::HashMap::new(),
                    published_fingerprints: std::collections::HashMap::new(),
                },
            ),
        ]);
        let mut registered = BTreeSet::new();
        let (internal_tx, _internal_rx) = mpsc::channel();
        let mut retry_scheduled = false;

        handle_internal_event(
            &mut sessions,
            &mut registered,
            internal_tx,
            &mut retry_scheduled,
            InternalEvent::AuthorityHostOutput {
                node_id: node_id.to_string(),
                session_instance_id: "session-new".to_string(),
                envelope: raw_pty_output_envelope(
                    node_id,
                    &format!("remote-peer:{node_id}:shell-output"),
                    "shell-output",
                ),
            },
        );

        let forwarded = recv_outbound_with_deadline(&mut new_outbound_rx)
            .expect("authority host output should be sent to latest active session");
        match forwarded.body {
            Some(Body::RawPtyOutput(payload)) => {
                assert_eq!(payload.session_id, "shell-output");
                assert_eq!(
                    payload.target_id,
                    format!("remote-peer:{node_id}:shell-output")
                );
                assert_eq!(payload.output_bytes, b"from-authority-host");
            }
            other => panic!("unexpected forwarded body: {other:?}"),
        }
        assert!(
            old_outbound_rx.try_recv().is_err(),
            "authority host output must not be sent through stale session"
        );
    }

    #[test]
    fn closed_session_instance_drops_late_envelopes() {
        let _guard = crate::test_support::integration_test_lock();
        use super::super::run_node_ingress_server_loop;
        use crate::infra::remote_grpc_transport::{
            RemoteNodeSessionHandle, RemoteNodeTransportEvent,
        };
        use std::sync::mpsc;
        use std::thread;

        let node_id = unique_node_id("peer-closed-late");
        let socket_path = authority_transport_socket_path(
            "test-socket-closed",
            "test-session-closed",
            &format!("remote-peer:{node_id}:shell-closed"),
        );
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("socket should bind");
        let accept = thread::spawn(move || collect_authority_connections(listener));

        let publication_runtime =
            RemoteTargetPublicationRuntime::new_for_route_tests_without_remote_runtime_owner()
                .expect("publication runtime should build");
        let (transport_tx, transport_rx) = mpsc::channel();
        let (internal_tx, internal_rx) = mpsc::channel();
        let worker_internal_tx = internal_tx.clone();
        let worker = thread::spawn(move || {
            run_node_ingress_server_loop(
                publication_runtime,
                RemoteNetworkConfig::default(),
                transport_rx,
                internal_rx,
                worker_internal_tx,
                false,
            );
        });

        let (session, _outbound_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id.clone(), "session-old");
        transport_tx
            .send(RemoteNodeTransportEvent::SessionOpened {
                session: session.clone(),
            })
            .expect("session should open");
        internal_tx
            .send(InternalEvent::SocketDirChanged)
            .expect("authority socket event should send");
        let mut writers = accept.join().expect("accept should join");
        assert!(
            !writers.is_empty(),
            "session should establish authority bridge"
        );

        transport_tx
            .send(RemoteNodeTransportEvent::SessionClosed {
                node_id: node_id.clone(),
                session_instance_id: "session-old".to_string(),
            })
            .expect("session should close");
        transport_tx
            .send(RemoteNodeTransportEvent::EnvelopeReceived {
                node_id: node_id.clone(),
                session_instance_id: "session-old".to_string(),
                envelope: NodeSessionEnvelope {
                    message_id: "late-target-output".to_string(),
                    sent_at: None,
                    session_instance_id: "session-old".to_string(),
                    correlation_id: None,
                    route: Some(RouteContext {
                        authority_node_id: Some(node_id.clone()),
                        target_id: Some(format!("remote-peer:{node_id}:shell-closed")),
                        attachment_id: None,
                        console_id: None,
                        console_host_id: None,
                        session_id: Some("shell-closed".to_string()),
                    }),
                    body: Some(Body::TargetOutput(TargetOutput {
                        target_id: format!("remote-peer:{node_id}:shell-closed"),
                        output_seq: 10,
                        stream: "pty".to_string(),
                        session_id: "shell-closed".to_string(),
                        output_bytes: b"late".to_vec(),
                    })),
                },
            })
            .expect("late envelope should send");

        assert!(
            read_target_output_from_any(&mut writers).is_none(),
            "late envelope from closed session instance must be dropped"
        );

        for writer in &writers {
            let _ = writer.shutdown(Shutdown::Both);
        }
        internal_tx
            .send(InternalEvent::Shutdown)
            .expect("shutdown event should send");
        drop(transport_tx);
        drop(internal_tx);
        let _ = worker.join();
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn node_cleanup_waits_until_last_ingress_session_is_gone() {
        let node_id = "peer-last-close";
        let other_node_id = "peer-other";
        let (same_node_session, _same_rx) =
            RemoteNodeSessionHandle::new_for_tests(node_id, "session-new");
        let (other_node_session, _other_rx) =
            RemoteNodeSessionHandle::new_for_tests(other_node_id, "session-other");

        let sessions = std::collections::HashMap::from([
            (
                "session-new".to_string(),
                ActiveNodeIngressSession {
                    session: same_node_session,
                    bridges: std::collections::HashMap::new(),
                    published_fingerprints: std::collections::HashMap::new(),
                },
            ),
            (
                "session-other".to_string(),
                ActiveNodeIngressSession {
                    session: other_node_session,
                    bridges: std::collections::HashMap::new(),
                    published_fingerprints: std::collections::HashMap::new(),
                },
            ),
        ]);

        assert!(has_active_ingress_session_for_node(&sessions, node_id));
        assert!(!has_active_ingress_session_for_node(
            &sessions,
            "peer-missing"
        ));

        let sessions = std::collections::HashMap::from([(
            "session-other".to_string(),
            ActiveNodeIngressSession {
                session: RemoteNodeSessionHandle::new_for_tests(other_node_id, "session-other-2").0,
                bridges: std::collections::HashMap::new(),
                published_fingerprints: std::collections::HashMap::new(),
            },
        )]);
        assert!(!has_active_ingress_session_for_node(&sessions, node_id));
    }

    fn recv_outbound_with_deadline(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<NodeSessionEnvelope>,
    ) -> Option<NodeSessionEnvelope> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match receiver.try_recv() {
                Ok(envelope) => return Some(envelope),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            }
        }
    }

    fn collect_authority_connections(
        listener: std::os::unix::net::UnixListener,
    ) -> Vec<std::os::unix::net::UnixStream> {
        listener
            .set_nonblocking(true)
            .expect("listener should become nonblocking");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut streams = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    crate::runtime::remote_node_transport_runtime::read_client_hello(&mut stream)
                        .expect("client hello should decode");
                    crate::runtime::remote_node_transport_runtime::write_server_hello(
                        &mut stream,
                        "waitagent-test",
                    )
                    .expect("server hello should encode");
                    streams.push(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("authority accept failed: {error}"),
            }
        }
        streams
    }

    fn read_envelope_from_any(
        streams: &mut [std::os::unix::net::UnixStream],
    ) -> Option<
        crate::infra::remote_protocol::ProtocolEnvelope<
            crate::infra::remote_protocol::ControlPlanePayload,
        >,
    > {
        for stream in streams {
            let mut reader = stream
                .try_clone()
                .expect("stream clone should succeed for envelope read");
            reader
                .set_read_timeout(Some(std::time::Duration::from_millis(200)))
                .expect("read timeout should set");
            match crate::infra::remote_transport_codec::read_control_plane_envelope(&mut reader) {
                Ok(envelope) => return Some(envelope),
                Err(error) if error.is_read_timeout() => continue,
                Err(error) => panic!("unexpected read error: {error}"),
            }
        }
        None
    }

    fn read_target_output_from_any(
        streams: &mut [std::os::unix::net::UnixStream],
    ) -> Option<
        crate::infra::remote_protocol::ProtocolEnvelope<
            crate::infra::remote_protocol::ControlPlanePayload,
        >,
    > {
        read_envelope_from_any(streams)
    }

    fn grpc_create_request_id(envelope: &NodeSessionEnvelope) -> Option<String> {
        match envelope.body.as_ref() {
            Some(Body::CreateSessionRequest(payload)) => Some(payload.request_id.clone()),
            _ => None,
        }
    }

    fn create_session_request_envelope(request_id: &str) -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: format!("create-session-{request_id}"),
            sent_at: None,
            session_instance_id: String::new(),
            correlation_id: Some(request_id.to_string()),
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: None,
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: None,
            }),
            body: Some(Body::CreateSessionRequest(
                crate::infra::remote_grpc_proto::v1::CreateSessionRequest {
                    request_id: request_id.to_string(),
                    authority_node_id: "peer-a".to_string(),
                    cwd_hint: None,
                    cols: 80,
                    rows: 24,
                },
            )),
        }
    }

    fn assert_publication_ack(envelope: &NodeSessionEnvelope, revision: u64, status: &str) {
        let Some(Body::TargetPublicationAck(payload)) = envelope.body.as_ref() else {
            panic!("expected TargetPublicationAck, got {:?}", envelope.body);
        };
        assert_eq!(payload.node_id, "peer-revision");
        assert_eq!(payload.node_instance_id, "client-session-1");
        assert_eq!(payload.target_id, "remote-peer:peer-revision:shell-rev");
        assert_eq!(payload.revision, revision);
        let actual = crate::infra::remote_grpc_proto::v1::TargetPublicationAckStatus::try_from(
            payload.status,
        )
        .expect("ack status should be known");
        let expected = match status {
            "applied" => crate::infra::remote_grpc_proto::v1::TargetPublicationAckStatus::Applied,
            "stale_revision" => {
                crate::infra::remote_grpc_proto::v1::TargetPublicationAckStatus::StaleRevision
            }
            other => panic!("unexpected expected status {other}"),
        };
        assert_eq!(actual, expected);
    }

    fn open_mirror_request_envelope(
        target_id: &str,
        session_id: &str,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        let payload = ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
            session_id: session_id.to_string(),
            target_id: target_id.to_string(),
            console_id: "console-1".to_string(),
            cols: 80,
            rows: 24,
            raw_pty_passthrough: false,
            bootstrap_mode: BootstrapMode::Full,
        });
        ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: "open-mirror-request-test".to_string(),
            message_type: payload.message_type(),
            timestamp: "0Z".to_string(),
            sender_id: "test-authority".to_string(),
            correlation_id: None,
            session_id: Some(session_id.to_string()),
            target_id: Some(target_id.to_string()),
            attachment_id: None,
            console_id: Some("console-1".to_string()),
            payload,
        }
    }

    fn raw_pty_output_envelope(
        sender_id: &str,
        target_id: &str,
        session_id: &str,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        let payload = ControlPlanePayload::RawPtyOutput(RawPtyOutputPayload {
            session_id: session_id.to_string(),
            target_id: target_id.to_string(),
            output_seq: 1,
            output_bytes: b"from-authority-host".to_vec(),
        });
        ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("{sender_id}-raw-pty-output-test"),
            message_type: payload.message_type(),
            timestamp: "0Z".to_string(),
            sender_id: sender_id.to_string(),
            correlation_id: None,
            session_id: Some(session_id.to_string()),
            target_id: Some(target_id.to_string()),
            attachment_id: None,
            console_id: None,
            payload,
        }
    }

    fn target_published_envelope_with_revision(
        session_id: &str,
        revision: u64,
        command_name: Option<&str>,
    ) -> NodeSessionEnvelope {
        let mut envelope = target_published_envelope_for(session_id);
        if let Some(Body::TargetPublished(payload)) = envelope.body.as_mut() {
            payload.target_id = format!("remote-peer:peer-revision:{session_id}");
            payload.authority_node_id = "peer-revision".to_string();
            payload.transport_session_id = session_id.to_string();
            payload.command_name = command_name.map(str::to_string);
            payload.node_instance_id = "client-session-1".to_string();
            payload.revision = revision;
        }
        envelope.route = Some(RouteContext {
            authority_node_id: Some("peer-revision".to_string()),
            target_id: Some(format!("remote-peer:peer-revision:{session_id}")),
            attachment_id: None,
            console_id: None,
            console_host_id: None,
            session_id: Some(session_id.to_string()),
        });
        envelope
    }

    fn target_exited_envelope_with_revision(
        session_id: &str,
        revision: u64,
    ) -> NodeSessionEnvelope {
        let mut envelope = target_exited_envelope_for(session_id);
        if let Some(Body::TargetExited(payload)) = envelope.body.as_mut() {
            payload.target_id = format!("remote-peer:peer-revision:{session_id}");
            payload.transport_session_id = session_id.to_string();
            payload.node_instance_id = "client-session-1".to_string();
            payload.revision = revision;
        }
        envelope.route = Some(RouteContext {
            authority_node_id: Some("peer-revision".to_string()),
            target_id: Some(format!("remote-peer:peer-revision:{session_id}")),
            attachment_id: None,
            console_id: None,
            console_host_id: None,
            session_id: Some(session_id.to_string()),
        });
        envelope
    }

    fn target_published_envelope_for(session_id: &str) -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: format!("target-published-{session_id}"),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: Some(format!("remote-peer:peer-a:{session_id}")),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some(session_id.to_string()),
            }),
            body: Some(Body::TargetPublished(
                crate::infra::remote_grpc_proto::v1::TargetPublished {
                    target_id: format!("remote-peer:peer-a:{session_id}"),
                    transport_session_id: session_id.to_string(),
                    authority_node_id: "peer-a".to_string(),
                    transport: "tmux".to_string(),
                    selector: Some("shell-a".to_string()),
                    availability: "online".to_string(),
                    command_name: Some("bash".to_string()),
                    current_path: Some("/tmp".to_string()),
                    attached_count: Some(0),
                    session_role: None,
                    workspace_key: None,
                    window_count: Some(1),
                    task_state: Some("running".to_string()),
                    node_instance_id: "client-session-1".to_string(),
                    revision: 7,
                },
            )),
        }
    }

    fn temp_dir_path(file_name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{file_name}-{}-{unique}.sock", process::id()))
    }

    fn unique_node_id(prefix: &str) -> String {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        format!("{prefix}-{unique}")
    }

    #[test]
    fn remote_node_ingress_owner_args_include_ready_socket_when_requested() {
        let network = RemoteNetworkConfig {
            port: 7474,
            connect: Some("10.0.0.8:7474".to_string()),
            node_id: Some("node-a".to_string()),
            public_endpoint: None,
        };
        let args =
            remote_node_ingress_owner_args(&network, Some(std::path::Path::new("/tmp/ready.sock")));

        assert!(args.iter().any(|arg| arg == "--ready-socket"));
        assert!(args.iter().any(|arg| arg == "/tmp/ready.sock"));
    }

    fn mirror_bootstrap_chunk_envelope() -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: "mirror-bootstrap-chunk-1".to_string(),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some("shell-1".to_string()),
            }),
            body: Some(Body::MirrorBootstrapChunk(MirrorBootstrapChunk {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                session_id: "shell-1".to_string(),
                chunk_seq: 1,
                stream: "pty".to_string(),
                output_bytes: b"bootstrap".to_vec(),
            })),
        }
    }

    fn mirror_bootstrap_complete_envelope() -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: "mirror-bootstrap-complete-1".to_string(),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some("shell-1".to_string()),
            }),
            body: Some(Body::MirrorBootstrapComplete(MirrorBootstrapComplete {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                session_id: "shell-1".to_string(),
                last_chunk_seq: 1,
                alternate_screen_active: false,
                application_cursor_keys: false,
                cursor_visible: true,
            })),
        }
    }

    fn target_exited_envelope_for(session_id: &str) -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: format!("target-exited-{session_id}"),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: Some(format!("remote-peer:peer-a:{session_id}")),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some(session_id.to_string()),
            }),
            body: Some(Body::TargetExited(
                crate::infra::remote_grpc_proto::v1::TargetExited {
                    target_id: format!("remote-peer:peer-a:{session_id}"),
                    transport_session_id: session_id.to_string(),
                    node_instance_id: "client-session-1".to_string(),
                    revision: 8,
                },
            )),
        }
    }

    fn target_output_envelope() -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: "target-output-1".to_string(),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: Some(RouteContext {
                authority_node_id: Some("peer-a".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                console_host_id: None,
                session_id: Some("shell-1".to_string()),
            }),
            body: Some(Body::TargetOutput(TargetOutput {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq: 7,
                stream: "pty".to_string(),
                session_id: "shell-1".to_string(),
                output_bytes: b"a".to_vec(),
            })),
        }
    }
}
