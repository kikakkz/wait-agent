mod tests {
    use super::super::{
        spawn_remote_node_session_listener, RemoteNodePublicationSink, RemoteNodeSessionRuntime,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::remote_protocol::{
        CloseMirrorRequestPayload, ControlPlanePayload, MirrorBootstrapChunkPayload,
        MirrorBootstrapCompletePayload, NodeSessionChannel, OpenMirrorRequestPayload,
        ProtocolEnvelope, RawPtyInputPayload, TargetGeometryChangedPayload, TargetPublishedPayload,
    };
    use crate::runtime::remote_authority_connection_runtime::{
        AuthorityConnectionRequest, AuthorityConnectionStarter, AuthorityTransportEvent,
        QueuedAuthorityStreamSource, QueuedAuthorityStreamStarter,
        RemoteAuthorityConnectionRuntime,
    };
    use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneSink;
    use crate::runtime::remote_node_ingress_runtime::{
        GrpcRemoteNodeIngressSource, RemoteNodeIngressSource,
    };
    use crate::runtime::remote_transport_runtime::{
        RegistryRemoteControlPlaneSink, RemoteConnectionRegistry,
    };
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    struct RecordingPublicationSink {
        tx: Mutex<mpsc::Sender<ProtocolEnvelope<ControlPlanePayload>>>,
    }

    impl RemoteNodePublicationSink for RecordingPublicationSink {
        fn publish(
            &self,
            envelope: ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<(), super::super::RemoteNodeSessionError> {
            self.tx
                .lock()
                .expect("publication sink mutex should not be poisoned")
                .send(envelope)
                .map_err(|_| {
                    super::super::RemoteNodeSessionError::new("publication test receiver closed")
                })
        }
    }

    #[test]
    fn single_outer_session_carries_authority_and_publication() {
        let socket_path = test_socket_path("mixed");
        let (authority_starter, authority_sink) = QueuedAuthorityStreamStarter::channel();
        let (tx, rx) = mpsc::channel();
        let publication_sink: Arc<dyn RemoteNodePublicationSink> =
            Arc::new(RecordingPublicationSink { tx: Mutex::new(tx) });
        let registry = RemoteConnectionRegistry::new();
        let (event_tx, event_rx) = mpsc::channel();
        let _guard = authority_starter
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: PathBuf::from("/tmp/unused.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                event_tx,
            )
            .expect("authority starter should start");
        let _listener = spawn_remote_node_session_listener(
            socket_path.clone(),
            authority_sink,
            publication_sink,
        )
        .expect("node session listener should bind");

        let session = Arc::new(
            RemoteNodeSessionRuntime::connect(&socket_path, "peer-a", None)
                .expect("node session should connect"),
        );

        let session_writer = session.clone();
        let authority_thread = thread::spawn(move || {
            while let Ok(event) = event_rx.recv_timeout(TEST_TIMEOUT) {
                if let AuthorityTransportEvent::Connected {
                    authority_id: _,
                    generation: _,
                } = event
                {
                    break;
                }
            }
            let connection = registry
                .connection_for("peer-a")
                .expect("authority connection should register");
            connection
                .send(&ProtocolEnvelope {
                    protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION
                        .to_string(),
                    message_id: "msg-in".to_string(),
                    message_type: "raw_pty_input",
                    timestamp: "1Z".to_string(),
                    sender_id: "observer".to_string(),
                    correlation_id: None,
                    session_id: Some("shell-1".to_string()),
                    target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                    attachment_id: Some("att-1".to_string()),
                    console_id: Some("console-1".to_string()),
                    payload: ControlPlanePayload::RawPtyInput(RawPtyInputPayload {
                        attachment_id: "att-1".to_string(),
                        session_id: "shell-1".to_string(),
                        target_id: "remote-peer:peer-a:shell-1".to_string(),
                        console_id: "console-1".to_string(),
                        console_host_id: "wa-local".to_string(),
                        input_seq: 1,
                        input_bytes: b"a".to_vec(),
                    }),
                })
                .expect("raw authority input should send");
            session_writer
                .send_target_published(&remote_target("peer-a", "shell-1"), Some("target-host-1"))
                .expect("publication should send");
        });

        match session
            .recv_authority_event()
            .expect("authority command should arrive")
        {
            crate::runtime::remote_node_session_runtime::RemoteNodeAuthorityEvent::Command(
                crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand::RawPtyInput(
                    payload,
                ),
            ) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.input_bytes, b"a");
            }
            other => panic!("unexpected authority event: {other:?}"),
        }
        authority_thread
            .join()
            .expect("authority helper thread should join cleanly");
        let envelope = rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("publication envelope should arrive");
        match envelope.payload {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                node_instance_id,
                revision,
                transport_session_id,
                source_session_name,
                ..
            }) => {
                assert_eq!(node_instance_id, "");
                assert_eq!(revision, 0);
                assert_eq!(transport_session_id, "shell-1");
                assert_eq!(source_session_name.as_deref(), Some("target-host-1"));
            }
            other => panic!("unexpected publication payload: {other:?}"),
        }
    }

    #[test]
    fn grpc_session_runtime_bridges_authority_and_publication_through_ingress_boundary() {
        let bind_addr = unused_local_addr();
        let source = GrpcRemoteNodeIngressSource::new(bind_addr);
        let (authority_source, authority_sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(authority_source);
        let registry = RemoteConnectionRegistry::new();
        let (event_tx, event_rx) = mpsc::channel();
        let _connection_guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-grpc-node-ingress.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                event_tx,
            )
            .expect("queued authority runtime should start");
        let (publication_tx, publication_rx) = mpsc::channel();
        let publication_sink: Arc<dyn RemoteNodePublicationSink> =
            Arc::new(RecordingPublicationSink {
                tx: Mutex::new(publication_tx),
            });
        let _guard = source
            .start(
                std::env::temp_dir().join("unused-grpc-node-session.sock"),
                authority_sink,
                publication_sink,
            )
            .expect("grpc ingress source should start");

        let session = Arc::new(
            RemoteNodeSessionRuntime::connect_grpc_for_tests(
                &format!("http://{bind_addr}"),
                "peer-a",
            )
            .expect("grpc node session should connect"),
        );

        while let Ok(event) = event_rx.recv_timeout(TEST_TIMEOUT) {
            if let AuthorityTransportEvent::Connected {
                authority_id: _,
                generation: _,
            } = event
            {
                break;
            }
        }

        session
            .send_target_output(
                "shell-1",
                "remote-peer:peer-a:shell-1",
                7,
                "pty",
                b"a".to_vec(),
            )
            .expect("target output should send through grpc node session");
        let authority_event = event_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("authority output should arrive");
        match authority_event {
            AuthorityTransportEvent::Envelope {
                authority_id: _,
                generation: _,
                envelope,
            } => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                    assert_eq!(payload.output_seq, 7);
                    assert_eq!(payload.output_bytes, b"a");
                }
                other => panic!("unexpected authority envelope payload: {other:?}"),
            },
            other => panic!("unexpected authority transport event: {other:?}"),
        }

        session
            .send_open_mirror_request(
                "shell-1",
                "remote-peer:peer-a:shell-1",
                "console-1",
                120,
                40,
            )
            .expect("open mirror should send through grpc node session");
        let authority_event = event_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("open mirror request should arrive");
        match authority_event {
            AuthorityTransportEvent::Envelope {
                authority_id: _,
                generation: _,
                envelope,
            } => match envelope.payload {
                ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
                    session_id,
                    target_id,
                    console_id,
                    cols,
                    rows,
                    raw_pty_passthrough: _,
                    bootstrap_mode: _,
                }) => {
                    assert_eq!(session_id, "shell-1");
                    assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                    assert_eq!(console_id, "console-1");
                    assert_eq!(cols, 120);
                    assert_eq!(rows, 40);
                }
                other => panic!("unexpected authority envelope payload: {other:?}"),
            },
            other => panic!("unexpected authority transport event: {other:?}"),
        }

        session
            .send_payload(
                NodeSessionChannel::Authority,
                "shell-1",
                "remote-peer:peer-a:shell-1",
                "authority-msg",
                ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    chunk_seq: 1,
                    stream: "pty",
                    output_bytes: b"bootstrap".to_vec(),
                }),
            )
            .expect("mirror bootstrap chunk should send through grpc node session");
        let authority_event = event_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("mirror bootstrap chunk should arrive");
        match authority_event {
            AuthorityTransportEvent::Envelope {
                authority_id: _,
                generation: _,
                envelope,
            } => match envelope.payload {
                ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
                    session_id,
                    target_id,
                    chunk_seq,
                    stream,
                    ref output_bytes,
                }) => {
                    assert_eq!(session_id, "shell-1");
                    assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                    assert_eq!(chunk_seq, 1);
                    assert_eq!(stream, "pty");
                    assert_eq!(output_bytes, b"bootstrap");
                }
                other => panic!("unexpected authority envelope payload: {other:?}"),
            },
            other => panic!("unexpected authority transport event: {other:?}"),
        }

        session
            .send_payload(
                NodeSessionChannel::Authority,
                "shell-1",
                "remote-peer:peer-a:shell-1",
                "authority-msg",
                ControlPlanePayload::MirrorBootstrapComplete(MirrorBootstrapCompletePayload {
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    last_chunk_seq: 1,
                    alternate_screen_active: false,
                    application_cursor_keys: false,
                    cursor_visible: true,
                }),
            )
            .expect("mirror bootstrap complete should send through grpc node session");
        let authority_event = event_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("mirror bootstrap complete should arrive");
        match authority_event {
            AuthorityTransportEvent::Envelope {
                authority_id: _,
                generation: _,
                envelope,
            } => match envelope.payload {
                ControlPlanePayload::MirrorBootstrapComplete(MirrorBootstrapCompletePayload {
                    session_id,
                    target_id,
                    last_chunk_seq,
                    alternate_screen_active,
                    application_cursor_keys,
                    cursor_visible,
                }) => {
                    assert_eq!(session_id, "shell-1");
                    assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                    assert_eq!(last_chunk_seq, 1);
                    assert!(!alternate_screen_active);
                    assert!(!application_cursor_keys);
                    assert!(cursor_visible);
                }
                other => panic!("unexpected authority envelope payload: {other:?}"),
            },
            other => panic!("unexpected authority transport event: {other:?}"),
        }

        session
            .send_close_mirror_request("shell-1", "remote-peer:peer-a:shell-1")
            .expect("close mirror should send through grpc node session");
        let authority_event = event_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("close mirror request should arrive");
        match authority_event {
            AuthorityTransportEvent::Envelope {
                authority_id: _,
                generation: _,
                envelope,
            } => match envelope.payload {
                ControlPlanePayload::CloseMirrorRequest(CloseMirrorRequestPayload {
                    session_id,
                    target_id,
                }) => {
                    assert_eq!(session_id, "shell-1");
                    assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                }
                other => panic!("unexpected authority envelope payload: {other:?}"),
            },
            other => panic!("unexpected authority transport event: {other:?}"),
        }

        session
            .send_payload(
                NodeSessionChannel::Authority,
                "shell-1",
                "remote-peer:peer-a:shell-1",
                "authority-msg",
                ControlPlanePayload::TargetGeometryChanged(TargetGeometryChangedPayload {
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    cols: 47,
                    rows: 22,
                }),
            )
            .expect("target geometry changed should send through grpc node session");
        let authority_event = event_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("target geometry changed should arrive");
        match authority_event {
            AuthorityTransportEvent::Envelope {
                authority_id: _,
                generation: _,
                envelope,
            } => match envelope.payload {
                ControlPlanePayload::TargetGeometryChanged(TargetGeometryChangedPayload {
                    session_id,
                    target_id,
                    cols,
                    rows,
                }) => {
                    assert_eq!(session_id, "shell-1");
                    assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                    assert_eq!(cols, 47);
                    assert_eq!(rows, 22);
                }
                other => panic!("unexpected authority envelope payload: {other:?}"),
            },
            other => panic!("unexpected authority transport event: {other:?}"),
        }

        RegistryRemoteControlPlaneSink::new(registry.clone())
            .send(&[
                crate::infra::remote_protocol::NodeBoundControlPlaneMessage {
                    node_id: "peer-a".to_string(),
                    envelope: ProtocolEnvelope {
                        protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION
                            .to_string(),
                        message_id: "target-input-1".to_string(),
                        message_type: "raw_pty_input",
                        timestamp: "1Z".to_string(),
                        sender_id: "server".to_string(),
                        correlation_id: None,
                        session_id: Some("shell-1".to_string()),
                        target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                        attachment_id: Some("attach-1".to_string()),
                        console_id: Some("console-1".to_string()),
                        payload: ControlPlanePayload::RawPtyInput(RawPtyInputPayload {
                            attachment_id: "attach-1".to_string(),
                            session_id: "shell-1".to_string(),
                            target_id: "remote-peer:peer-a:shell-1".to_string(),
                            console_id: "console-1".to_string(),
                            console_host_id: "host-1".to_string(),
                            input_seq: 3,
                            input_bytes: b"b".to_vec(),
                        }),
                    },
                },
            ])
            .expect("raw PTY input should route through grpc node session");
        match session
            .recv_authority_event()
            .expect("authority command should arrive over grpc")
        {
            crate::runtime::remote_node_session_runtime::RemoteNodeAuthorityEvent::Command(
                crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand::RawPtyInput(
                    payload,
                ),
            ) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.input_seq, 3);
                assert_eq!(payload.input_bytes, b"b");
            }
            other => panic!("unexpected authority event: {other:?}"),
        }

        session
            .send_target_published(&remote_target("peer-a", "shell-1"), Some("target-host-1"))
            .expect("publication should send through grpc node session");
        let envelope = publication_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("publication envelope should arrive");
        match envelope.payload {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                node_instance_id,
                revision,
                transport_session_id,
                source_session_name,
                selector,
                availability,
                command_name,
                display_command_name: None,
                current_path,
                attached_clients,
                window_count,
                session_role,
                workspace_key,
                task_state,
            }) => {
                assert_eq!(node_instance_id, "");
                assert_eq!(revision, 0);
                assert_eq!(transport_session_id, "shell-1");
                assert_eq!(source_session_name, None);
                assert_eq!(selector.as_deref(), Some("wk:shell"));
                assert_eq!(availability, "online");
                assert_eq!(command_name.as_deref(), Some("codex"));
                assert_eq!(current_path.as_deref(), Some("/tmp/demo"));
                assert_eq!(attached_clients, 2);
                assert_eq!(window_count, 1);
                assert_eq!(session_role, Some("target-host"));
                assert_eq!(workspace_key.as_deref(), Some("wk-1"));
                assert_eq!(task_state, "unknown");
            }
            other => panic!("unexpected publication payload: {other:?}"),
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some("wk:shell".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 2,
            window_count: 1,
            command_name: Some("codex".to_string()),
            display_command_name: None,
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Unknown,
        }
    }

    fn test_socket_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-remote-node-session-test-{}-{}.sock",
            std::process::id(),
            label
        ))
    }

    fn unused_local_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral listener should bind");
        let addr = listener
            .local_addr()
            .expect("ephemeral listener should report local addr");
        drop(listener);
        addr
    }
}
