mod tests {
    use super::super::{
        discover_authority_socket_paths, extract_target_component, route_transport_envelope,
        ActiveAuthoritySocketBridge, ActiveNodeIngressSession, InternalEvent,
        RemoteNodeIngressServerRuntime,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_proto::v1::{
        MirrorBootstrapChunk, MirrorBootstrapComplete, NodeSessionEnvelope, RouteContext,
        TargetOutput,
    };
    use crate::infra::remote_grpc_transport::RemoteNodeSessionHandle;
    use crate::runtime::remote_authority_transport_runtime::{
        authority_transport_socket_path, RemoteAuthorityTransportRuntime,
    };
    use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
    use std::fs;
    use std::net::Shutdown;
    use std::path::PathBuf;
    use std::process;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        };
        let publication_runtime = RemoteTargetPublicationRuntime::from_build_env()
            .expect("publication runtime should build");

        route_transport_envelope(
            &publication_runtime,
            node_id,
            mirror_bootstrap_chunk_envelope(),
            Some(&mut active),
        )
        .expect("bootstrap chunk should route");
        route_transport_envelope(
            &publication_runtime,
            node_id,
            mirror_bootstrap_complete_envelope(),
            Some(&mut active),
        )
        .expect("bootstrap complete should route");
        route_transport_envelope(
            &publication_runtime,
            node_id,
            target_output_envelope(),
            Some(&mut active),
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
        };
        let publication_runtime = RemoteTargetPublicationRuntime::from_build_env()
            .expect("publication runtime should build");

        route_transport_envelope(
            &publication_runtime,
            &node_id,
            target_exited_envelope_for("shell-a"),
            Some(&mut active),
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

        let publication_runtime = RemoteTargetPublicationRuntime::from_build_env()
            .expect("publication runtime should build");
        let (transport_tx, transport_rx) = mpsc::channel();
        let (internal_tx, internal_rx) = mpsc::channel();
        let worker_internal_tx = internal_tx.clone();
        let worker = thread::spawn(move || {
            run_node_ingress_server_loop(
                publication_runtime,
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
