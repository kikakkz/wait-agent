mod tests {
    use super::super::{
        authority_command_envelope, bridge_shared_live_authority_stream,
        dispatch_authority_command_to_live_route, dispatch_publication_sender_command,
        ensure_live_session_route_without_target_host_or_dispatcher,
        ensure_live_session_route_without_target_host_sidecar, live_authority_session_socket_path,
        stop_live_session_route, LiveSessionRoute, SharedAuthoritySession,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::infra::remote_protocol::{
        ApplyResizePayload, ClientHelloPayload, ControlPlanePayload, NodeSessionChannel,
        NodeSessionEnvelope, ProtocolEnvelope, RawPtyInputPayload, TargetExitedPayload,
        TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, read_node_session_envelope, write_control_plane_envelope,
        write_node_session_envelope,
    };
    use crate::runtime::current_executable::waitagent_test_executable;
    use crate::runtime::remote_authority_transport_runtime::{
        RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
    };
    use crate::runtime::remote_node_transport_runtime::{
        read_client_hello, write_server_hello, NODE_TRANSPORT_CLIENT_VERSION,
    };
    use crate::runtime::remote_target_publication_runtime::{
        PublicationSenderCommand, RemoteTargetPublicationRuntime,
    };
    use crate::runtime::remote_target_publication_transport_runtime::RemoteTargetPublicationTransportRuntime;
    use std::collections::HashMap;
    use std::fs;
    use std::net::Shutdown;
    use std::os::unix::net::UnixListener;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn live_route_buffers_authority_commands_until_host_connects() {
        let _guard = crate::test_support::integration_test_lock();
        let route = Arc::new(LiveSessionRoute {
            socket_name: "wa-1".to_string(),
            target_session_name: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_session_id: "target-1".to_string(),
            socket_path: test_socket_path("buffered-live-route"),
            running: Arc::new(AtomicBool::new(true)),
            writer: Arc::new(Mutex::new(None)),
            pending_commands: Arc::new(Mutex::new(Vec::new())),
        });
        let routes = Arc::new(Mutex::new(HashMap::from([(
            "target-1".to_string(),
            route.clone(),
        )])));
        let command = RemoteAuthorityCommand::OpenMirror(
            crate::infra::remote_protocol::OpenMirrorRequestPayload {
                session_id: "target-1".to_string(),
                target_id: "remote-peer:peer-a:target-1".to_string(),
                console_id: "console-a".to_string(),
                cols: 80,
                rows: 24,
                raw_pty_passthrough: false,
                bootstrap_mode: crate::infra::remote_protocol::BootstrapMode::Full,
            },
        );

        dispatch_authority_command_to_live_route(&routes, &command)
            .expect("buffering open-mirror should succeed without a live writer");

        assert_eq!(
            route
                .pending_commands
                .lock()
                .expect("pending commands mutex should not be poisoned")
                .len(),
            1
        );

        let (mut host_client, host_server) =
            UnixStream::pair().expect("live authority stream pair should open");
        write_control_plane_envelope(
            &mut host_client,
            &ProtocolEnvelope {
                protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                message_id: "hello-1".to_string(),
                message_type: "client_hello",
                timestamp: "0Z".to_string(),
                sender_id: "peer-a".to_string(),
                correlation_id: None,
                session_id: None,
                target_id: None,
                attachment_id: None,
                console_id: None,
                payload: ControlPlanePayload::ClientHello(ClientHelloPayload {
                    node_id: "peer-a".to_string(),
                    client_version: "test".to_string(),
                }),
            },
        )
        .expect("client hello should encode");

        let shared_session = SharedAuthoritySession {
            authority_id: "peer-a".to_string(),
            transport_socket_path: "/tmp/unused.sock".to_string(),
            publication_runtime: RemoteTargetPublicationRuntime::from_build_env()
                .expect("publication runtime should build from env"),
            network: RemoteNetworkConfig::default(),
            running: Arc::new(AtomicBool::new(true)),
            owner_started: Arc::new(AtomicBool::new(true)),
            session: Arc::new(Mutex::new(None)),
            routes: Arc::new(Mutex::new(HashMap::new())),
            pending_exits: Arc::new(Mutex::new(HashMap::new())),
        };

        let bridge = thread::spawn({
            let route = route.clone();
            let shared_session = shared_session.clone();
            move || bridge_shared_live_authority_stream(host_server, shared_session, route)
        });

        let _server_hello =
            read_control_plane_envelope(&mut host_client).expect("server hello should decode");
        let envelope = read_control_plane_envelope(&mut host_client)
            .expect("buffered authority command should flush after host connects");
        let expected = authority_command_envelope(command);
        assert_eq!(envelope.protocol_version, expected.protocol_version);
        assert_eq!(envelope.message_type, expected.message_type);
        assert_eq!(envelope.sender_id, expected.sender_id);
        assert_eq!(envelope.session_id, expected.session_id);
        assert_eq!(envelope.payload, expected.payload);

        route.running.store(false, Ordering::Relaxed);
        let _ = host_client.shutdown(Shutdown::Both);
        let _ = bridge.join();
    }

    #[test]
    fn owner_runtime_reuses_cached_publication_transport_for_publish_and_exit() {
        let _guard = crate::test_support::integration_test_lock();
        let socket_path = test_socket_path("owner-publication-cache");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let accept_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            match read_control_plane_envelope(&mut stream)
                .expect("client hello should decode")
                .payload
            {
                ControlPlanePayload::ClientHello(ClientHelloPayload {
                    node_id,
                    client_version,
                }) => {
                    assert_eq!(node_id, "peer-a");
                    assert_eq!(client_version, NODE_TRANSPORT_CLIENT_VERSION);
                }
                other => panic!("unexpected hello payload: {other:?}"),
            }
            write_server_hello(&mut stream, "waitagent-publication")
                .expect("server hello should encode");
            let published =
                read_node_session_envelope(&mut stream).expect("publish envelope should decode");
            let exited =
                read_node_session_envelope(&mut stream).expect("exit envelope should decode");
            (published, exited)
        });

        let mut transports = HashMap::<String, RemoteTargetPublicationTransportRuntime>::new();
        dispatch_publication_sender_command(
            &socket_path,
            &mut transports,
            PublicationSenderCommand::PublishTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wk:shell".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "confirm",
            },
        )
        .expect("publish command should route through owner transport cache");
        dispatch_publication_sender_command(
            &socket_path,
            &mut transports,
            PublicationSenderCommand::ExitTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
            },
        )
        .expect("exit command should reuse cached owner transport");

        assert_eq!(transports.len(), 1);

        let (published, exited) = accept_thread
            .join()
            .expect("accept thread should join cleanly");
        assert_eq!(published.channel, NodeSessionChannel::Publication);
        match published.envelope.payload {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                node_instance_id,
                revision,
                transport_session_id,
                source_session_name,
                selector,
                availability,
                session_role,
                workspace_key,
                command_name,
                current_path,
                attached_clients,
                window_count,
                task_state,
            }) => {
                assert_eq!(node_instance_id, "");
                assert_eq!(revision, 0);
                assert_eq!(transport_session_id, "shell-1");
                assert_eq!(source_session_name.as_deref(), Some("target-host-1"));
                assert_eq!(selector.as_deref(), Some("wk:shell"));
                assert_eq!(availability, "online");
                assert_eq!(session_role, Some("target-host"));
                assert_eq!(workspace_key.as_deref(), Some("wk-1"));
                assert_eq!(command_name.as_deref(), Some("codex"));
                assert_eq!(current_path.as_deref(), Some("/tmp/demo"));
                assert_eq!(attached_clients, 2);
                assert_eq!(window_count, 3);
                assert_eq!(task_state, "confirm");
            }
            other => panic!("unexpected publish payload: {other:?}"),
        }

        assert_eq!(exited.channel, NodeSessionChannel::Publication);
        assert_eq!(
            exited.envelope.payload,
            ControlPlanePayload::TargetExited(TargetExitedPayload {
                transport_session_id: "shell-1".to_string(),
                node_instance_id: String::new(),
                revision: 0,
                source_session_name: Some("target-host-1".to_string()),
            })
        );

        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn shared_authority_session_reuses_one_node_connection_and_routes_by_target_id() {
        let _guard = crate::test_support::integration_test_lock();
        let socket_name = "wa-shared";
        let target_session_a = "target-sa";
        let target_session_b = "target-sb";
        let target_id_a = "remote-peer:peer-a:target-sa";
        let target_id_b = "remote-peer:peer-a:target-sb";
        let transport_socket_path = test_socket_path("shared-authority-session");
        let listener = UnixListener::bind(&transport_socket_path).expect("listener should bind");
        listener
            .set_nonblocking(true)
            .expect("listener should allow nonblocking accept");
        let accept_count = Arc::new(AtomicUsize::new(0));
        let server_running = Arc::new(AtomicBool::new(true));
        let server_stream = Arc::new(Mutex::new(None::<std::os::unix::net::UnixStream>));
        let server_thread = {
            let accept_count = accept_count.clone();
            let server_running = server_running.clone();
            let server_stream = server_stream.clone();
            thread::spawn(move || {
                while server_running.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            accept_count.fetch_add(1, Ordering::Relaxed);
                            let node_id =
                                read_client_hello(&mut stream).expect("client hello should decode");
                            assert_eq!(node_id, "peer-a");
                            write_server_hello(&mut stream, "waitagent-remote-node-session")
                                .expect("server hello should encode");
                            let mut shared_stream = server_stream
                                .lock()
                                .expect("server stream mutex should not be poisoned");
                            if shared_stream.is_none() {
                                *shared_stream = Some(
                                    stream
                                        .try_clone()
                                        .expect("accepted stream should clone for test server"),
                                );
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let mut live_sessions = HashMap::<String, Arc<LiveSessionRoute>>::new();
        let mut authority_sessions = HashMap::<String, SharedAuthoritySession>::new();
        let publication_runtime =
            RemoteTargetPublicationRuntime::from_build_env().expect("publication runtime");
        let current_executable = waitagent_test_executable();
        ensure_live_session_route_without_target_host_sidecar(
            &current_executable,
            socket_name,
            target_session_a,
            "peer-a",
            target_id_a,
            transport_socket_path.to_string_lossy().as_ref(),
            &RemoteNetworkConfig::default(),
            &publication_runtime,
            &mut live_sessions,
            &mut authority_sessions,
        )
        .expect("first live session route should register");
        ensure_live_session_route_without_target_host_sidecar(
            &current_executable,
            socket_name,
            target_session_b,
            "peer-a",
            target_id_b,
            transport_socket_path.to_string_lossy().as_ref(),
            &RemoteNetworkConfig::default(),
            &publication_runtime,
            &mut live_sessions,
            &mut authority_sessions,
        )
        .expect("second live session route should reuse authority session");

        wait_for_ready_socket(&live_authority_session_socket_path(
            socket_name,
            target_session_a,
        ));
        wait_for_ready_socket(&live_authority_session_socket_path(
            socket_name,
            target_session_b,
        ));
        let transport_a = connect_authority_transport_with_retry(
            live_authority_session_socket_path(socket_name, target_session_a),
            "peer-a",
        );
        let transport_b = connect_authority_transport_with_retry(
            live_authority_session_socket_path(socket_name, target_session_b),
            "peer-a",
        );

        wait_for_condition(Duration::from_secs(1), || {
            accept_count.load(Ordering::Relaxed) == 1
                && server_stream
                    .lock()
                    .expect("server stream mutex should not be poisoned")
                    .is_some()
        });
        assert_eq!(accept_count.load(Ordering::Relaxed), 1);

        {
            let mut server_stream = server_stream
                .lock()
                .expect("server stream mutex should not be poisoned");
            let stream = server_stream
                .as_mut()
                .expect("shared authority stream should be available");
            write_node_session_envelope(
                stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: raw_pty_input_envelope(
                        target_id_a,
                        "attach-a",
                        "console-a",
                        7,
                        b"a".to_vec(),
                    ),
                },
            )
            .expect("raw PTY input should encode");
            write_node_session_envelope(
                stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: apply_resize_envelope(target_id_b, 80, 24),
                },
            )
            .expect("resize command should encode");
        }

        match recv_command_with_timeout(transport_a, Duration::from_secs(1))
            .expect("target-a command should arrive")
        {
            RemoteAuthorityCommand::RawPtyInput(payload) => {
                assert_eq!(payload.target_id, target_id_a);
                assert_eq!(payload.input_seq, 7);
                assert_eq!(payload.input_bytes, b"a");
            }
            other => panic!("unexpected target-a authority command: {other:?}"),
        }
        match recv_command_with_timeout(transport_b, Duration::from_secs(1))
            .expect("target-b command should arrive")
        {
            RemoteAuthorityCommand::ApplyResize(payload) => {
                assert_eq!(payload.target_id, target_id_b);
                assert_eq!(payload.cols, 80);
                assert_eq!(payload.rows, 24);
            }
            other => panic!("unexpected target-b authority command: {other:?}"),
        }

        stop_live_session_route(
            target_session_a,
            &mut live_sessions,
            &mut authority_sessions,
        );
        stop_live_session_route(
            target_session_b,
            &mut live_sessions,
            &mut authority_sessions,
        );
        server_running.store(false, Ordering::Relaxed);
        let _ = server_thread.join();
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn ensure_live_session_route_reuses_existing_session_route_for_same_target() {
        let _guard = crate::test_support::integration_test_lock();
        let socket_name = "wa-reuse";
        let target_session_name = "target-reuse";
        let target_id = "remote-peer:peer-a:target-reuse";
        let transport_socket_path = test_socket_path("shared-authority-reuse");
        let mut live_sessions = HashMap::new();
        let mut authority_sessions = HashMap::<String, SharedAuthoritySession>::new();
        let publication_runtime =
            RemoteTargetPublicationRuntime::from_build_env().expect("publication runtime");
        let current_executable = waitagent_test_executable();

        ensure_live_session_route_without_target_host_or_dispatcher(
            &current_executable,
            socket_name,
            target_session_name,
            "peer-a",
            target_id,
            transport_socket_path.to_string_lossy().as_ref(),
            &RemoteNetworkConfig::default(),
            &publication_runtime,
            &mut live_sessions,
            &mut authority_sessions,
        )
        .expect("first live session route should register");

        let first_route = live_sessions
            .get(target_session_name)
            .expect("route should exist after first register")
            .clone();

        ensure_live_session_route_without_target_host_or_dispatcher(
            &current_executable,
            socket_name,
            target_session_name,
            "peer-a",
            target_id,
            transport_socket_path.to_string_lossy().as_ref(),
            &RemoteNetworkConfig::default(),
            &publication_runtime,
            &mut live_sessions,
            &mut authority_sessions,
        )
        .expect("second live session route should reuse existing route");

        let second_route = live_sessions
            .get(target_session_name)
            .expect("route should still exist after second register")
            .clone();

        assert!(Arc::ptr_eq(&first_route, &second_route));
        assert_eq!(authority_sessions.len(), 1);

        stop_live_session_route(
            target_session_name,
            &mut live_sessions,
            &mut authority_sessions,
        );
    }

    #[test]
    fn shared_authority_session_reconnects_without_dropping_local_authority_bridge() {
        let _guard = crate::test_support::integration_test_lock();
        let socket_name = "wa-reconnect";
        let target_session_name = "target-r";
        let target_id = "remote-peer:peer-a:target-r";
        let transport_socket_path = test_socket_path("shared-authority-reconnect");
        let listener = UnixListener::bind(&transport_socket_path).expect("listener should bind");
        listener
            .set_nonblocking(true)
            .expect("listener should allow nonblocking accept");
        let accept_count = Arc::new(AtomicUsize::new(0));
        let server_running = Arc::new(AtomicBool::new(true));
        let server_stream = Arc::new(Mutex::new(None::<std::os::unix::net::UnixStream>));
        let server_thread = {
            let accept_count = accept_count.clone();
            let server_running = server_running.clone();
            let server_stream = server_stream.clone();
            thread::spawn(move || {
                while server_running.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            accept_count.fetch_add(1, Ordering::Relaxed);
                            let node_id =
                                read_client_hello(&mut stream).expect("client hello should decode");
                            assert_eq!(node_id, "peer-a");
                            write_server_hello(&mut stream, "waitagent-remote-node-session")
                                .expect("server hello should encode");
                            let mut shared_stream = server_stream
                                .lock()
                                .expect("server stream mutex should not be poisoned");
                            *shared_stream = Some(
                                stream
                                    .try_clone()
                                    .expect("accepted stream should clone for test server"),
                            );
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let mut live_sessions = HashMap::new();
        let mut authority_sessions = HashMap::<String, SharedAuthoritySession>::new();
        let publication_runtime =
            RemoteTargetPublicationRuntime::from_build_env().expect("publication runtime");
        let current_executable = waitagent_test_executable();
        ensure_live_session_route_without_target_host_sidecar(
            &current_executable,
            socket_name,
            target_session_name,
            "peer-a",
            target_id,
            transport_socket_path.to_string_lossy().as_ref(),
            &RemoteNetworkConfig::default(),
            &publication_runtime,
            &mut live_sessions,
            &mut authority_sessions,
        )
        .expect("live session route should register");

        wait_for_ready_socket(&live_authority_session_socket_path(
            socket_name,
            target_session_name,
        ));
        let transport_a = Arc::new(connect_authority_transport_with_retry(
            live_authority_session_socket_path(socket_name, target_session_name),
            "peer-a",
        ));

        wait_for_condition(Duration::from_secs(1), || {
            accept_count.load(Ordering::Relaxed) == 1
                && server_stream
                    .lock()
                    .expect("server stream mutex should not be poisoned")
                    .is_some()
        });
        {
            let mut server_stream = server_stream
                .lock()
                .expect("server stream mutex should not be poisoned");
            let stream = server_stream
                .as_mut()
                .expect("shared authority stream should be available");
            write_node_session_envelope(
                stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: raw_pty_input_envelope(
                        target_id,
                        "attach-a",
                        "console-a",
                        1,
                        b"a".to_vec(),
                    ),
                },
            )
            .expect("initial raw PTY input should encode");
        }
        match recv_shared_command_with_timeout(transport_a.clone(), Duration::from_secs(1))
            .expect("initial target-a command should arrive")
        {
            RemoteAuthorityCommand::RawPtyInput(payload) => {
                assert_eq!(payload.target_id, target_id);
                assert_eq!(payload.input_seq, 1);
                assert_eq!(payload.input_bytes, b"a");
            }
            other => panic!("unexpected initial authority command: {other:?}"),
        }

        {
            let mut server_stream = server_stream
                .lock()
                .expect("server stream mutex should not be poisoned");
            let stream = server_stream
                .take()
                .expect("first shared authority stream should be available");
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }

        wait_for_condition(Duration::from_secs(2), || {
            accept_count.load(Ordering::Relaxed) >= 2
                && server_stream
                    .lock()
                    .expect("server stream mutex should not be poisoned")
                    .is_some()
        });
        {
            let mut server_stream = server_stream
                .lock()
                .expect("server stream mutex should not be poisoned");
            let stream = server_stream
                .as_mut()
                .expect("reconnected shared authority stream should be available");
            write_node_session_envelope(
                stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: raw_pty_input_envelope(
                        target_id,
                        "attach-a",
                        "console-a",
                        2,
                        b"b".to_vec(),
                    ),
                },
            )
            .expect("reconnected raw PTY input should encode");
        }
        match recv_shared_command_with_timeout(transport_a, Duration::from_secs(1))
            .expect("reconnected target-a command should arrive")
        {
            RemoteAuthorityCommand::RawPtyInput(payload) => {
                assert_eq!(payload.target_id, target_id);
                assert_eq!(payload.input_seq, 2);
                assert_eq!(payload.input_bytes, b"b");
            }
            other => panic!("unexpected reconnected authority command: {other:?}"),
        }

        stop_live_session_route(
            target_session_name,
            &mut live_sessions,
            &mut authority_sessions,
        );
        server_running.store(false, Ordering::Relaxed);
        let _ = server_thread.join();
        let _ = fs::remove_file(&transport_socket_path);
    }

    fn test_socket_path(label: &str) -> PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "waitagent-remote-node-session-owner-test-{}-{}-{label}.sock",
            std::process::id(),
            now
        ))
    }

    fn raw_pty_input_envelope(
        target_id: &str,
        attachment_id: &str,
        console_id: &str,
        input_seq: u64,
        input_bytes: Vec<u8>,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        let payload = ControlPlanePayload::RawPtyInput(RawPtyInputPayload {
            attachment_id: attachment_id.to_string(),
            session_id: target_id
                .splitn(3, ':')
                .nth(2)
                .unwrap_or(target_id)
                .to_string(),
            target_id: target_id.to_string(),
            console_id: console_id.to_string(),
            console_host_id: "host-a".to_string(),
            input_seq,
            input_bytes,
        });
        ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("raw-pty-input-{input_seq}"),
            message_type: payload.message_type(),
            timestamp: "1Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some(
                target_id
                    .splitn(3, ':')
                    .nth(2)
                    .unwrap_or(target_id)
                    .to_string(),
            ),
            target_id: Some(target_id.to_string()),
            attachment_id: Some(attachment_id.to_string()),
            console_id: Some(console_id.to_string()),
            payload,
        }
    }

    fn apply_resize_envelope(
        target_id: &str,
        cols: usize,
        rows: usize,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        let payload = ControlPlanePayload::ApplyResize(ApplyResizePayload {
            session_id: target_id
                .splitn(3, ':')
                .nth(2)
                .unwrap_or(target_id)
                .to_string(),
            target_id: target_id.to_string(),
            resize_epoch: 1,
            resize_authority_console_id: "console-b".to_string(),
            cols,
            rows,
        });
        ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: "resize-1".to_string(),
            message_type: payload.message_type(),
            timestamp: "1Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some(
                target_id
                    .splitn(3, ':')
                    .nth(2)
                    .unwrap_or(target_id)
                    .to_string(),
            ),
            target_id: Some(target_id.to_string()),
            attachment_id: None,
            console_id: Some("console-b".to_string()),
            payload,
        }
    }

    fn wait_for_ready_socket(socket_path: &PathBuf) {
        wait_for_condition(Duration::from_secs(1), || socket_path.exists());
    }

    fn wait_for_condition(timeout: Duration, predicate: impl Fn() -> bool) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(predicate(), "condition did not become true within timeout");
    }

    fn recv_command_with_timeout(
        transport: RemoteAuthorityTransportRuntime,
        timeout: Duration,
    ) -> Result<RemoteAuthorityCommand, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(transport.recv_command().map_err(|error| error.to_string()));
        });
        rx.recv_timeout(timeout)
            .map_err(|_| "authority command timed out".to_string())?
    }

    fn recv_shared_command_with_timeout(
        transport: Arc<RemoteAuthorityTransportRuntime>,
        timeout: Duration,
    ) -> Result<RemoteAuthorityCommand, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(transport.recv_command().map_err(|error| error.to_string()));
        });
        rx.recv_timeout(timeout)
            .map_err(|_| "authority command timed out".to_string())?
    }

    fn connect_authority_transport_with_retry(
        socket_path: PathBuf,
        node_id: &str,
    ) -> RemoteAuthorityTransportRuntime {
        let start = Instant::now();
        loop {
            match RemoteAuthorityTransportRuntime::connect(&socket_path, node_id) {
                Ok(transport) => return transport,
                Err(error) if start.elapsed() < Duration::from_secs(1) => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("target authority bridge should connect: {error:?}"),
            }
        }
    }
}
