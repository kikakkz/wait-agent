mod tests {
    use super::super::{
        authority_input_fifo_path, authority_output_ingest_socket_path,
        pump_reader_to_ingest_socket, read_output_chunk_frame, remote_authority_error,
        remote_authority_output_pump_shell_command, remote_authority_target_host_args,
        render_bootstrap_replay, write_output_chunk_frame, LifecycleError,
        RemoteAuthorityPublicationGateway, RemoteAuthorityTargetHostRuntime,
        RemoteTargetPtyGateway, RemoteTargetTerminalFlags,
    };
    use crate::cli::RemoteAuthorityTargetHostCommand;
    use crate::cli::RemoteNetworkConfig;
    use crate::infra::remote_protocol::{
        ApplyResizePayload, BootstrapMode, ClientHelloPayload, ControlPlanePayload,
        NodeSessionChannel, NodeSessionEnvelope, OpenMirrorRequestPayload, ProtocolEnvelope,
        RawPtyInputPayload, TargetOutputPayload,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, read_node_session_envelope, write_node_session_envelope,
    };
    use crate::infra::tmux::TmuxPaneId;
    use crate::runtime::remote_node_session_owner_runtime::{
        live_authority_session_socket_path, spawn_live_authority_session_bridge,
    };
    use crate::runtime::remote_node_session_runtime::RemoteNodeSessionRuntime;
    use crate::runtime::remote_node_transport_runtime::write_server_hello;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::Cursor;
    use std::io::Read;
    use std::net::Shutdown;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone, Default)]
    struct FakeGateway {
        resize_calls: Arc<Mutex<Vec<(usize, usize)>>>,
        pipe_calls: Arc<Mutex<Vec<String>>>,
        clear_calls: Arc<Mutex<usize>>,
        capture_bootstrap_screen: Arc<Mutex<String>>,
        cursor_position: Arc<Mutex<(usize, usize)>>,
        terminal_flags: Arc<Mutex<RemoteTargetTerminalFlags>>,
    }

    impl RemoteTargetPtyGateway for FakeGateway {
        type Error = &'static str;

        fn target_presentation_pane(
            &self,
            _socket_name: &str,
            _target_session_name: &str,
        ) -> Result<TmuxPaneId, Self::Error> {
            Ok(TmuxPaneId::new("%7"))
        }

        fn resize_pty(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            cols: usize,
            rows: usize,
        ) -> Result<(), Self::Error> {
            self.resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .push((cols, rows));
            Ok(())
        }

        fn clear_output_pipe(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            let mut clear_calls = self
                .clear_calls
                .lock()
                .expect("clear calls mutex should not be poisoned");
            *clear_calls += 1;
            Ok(())
        }

        fn capture_bootstrap_screen(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            _visible_only: bool,
        ) -> Result<String, Self::Error> {
            Ok(self
                .capture_bootstrap_screen
                .lock()
                .expect("capture bootstrap screen mutex should not be poisoned")
                .clone())
        }

        fn capture_cursor_position(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
        ) -> Result<(usize, usize), Self::Error> {
            Ok(*self
                .cursor_position
                .lock()
                .expect("cursor position mutex should not be poisoned"))
        }

        fn capture_terminal_flags(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
        ) -> Result<RemoteTargetTerminalFlags, Self::Error> {
            Ok(*self
                .terminal_flags
                .lock()
                .expect("terminal flags mutex should not be poisoned"))
        }

        fn set_output_pipe(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.pipe_calls
                .lock()
                .expect("pipe calls mutex should not be poisoned")
                .push(command.to_string());
            Ok(())
        }

        fn send_keys_to_pane(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            _keys: &str,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    struct FakeLiveSession {
        session: Arc<RemoteNodeSessionRuntime>,
        socket_path: PathBuf,
        running: Arc<AtomicBool>,
    }

    #[derive(Clone, Default)]
    struct FakePublicationGateway {
        live_session: Arc<Mutex<Option<FakeLiveSession>>>,
    }

    impl RemoteAuthorityPublicationGateway for FakePublicationGateway {
        fn ensure_live_session_registered(
            &self,
            socket_name: &str,
            target_session_name: &str,
            authority_id: &str,
            _target_id: &str,
            transport_socket_path: &str,
        ) -> Result<PathBuf, LifecycleError> {
            let session = Arc::new(
                RemoteNodeSessionRuntime::connect(transport_socket_path, authority_id, None)
                    .map_err(remote_authority_error)?,
            );
            let running = Arc::new(AtomicBool::new(true));
            let socket_path = live_authority_session_socket_path(socket_name, target_session_name);
            spawn_live_authority_session_bridge(
                socket_path.clone(),
                session.clone(),
                running.clone(),
            );
            super::super::wait_for_ready_socket(&socket_path)?;
            *self
                .live_session
                .lock()
                .expect("live session mutex should not be poisoned") = Some(FakeLiveSession {
                session,
                socket_path: socket_path.clone(),
                running,
            });
            Ok(socket_path)
        }

        fn ensure_live_session_unregistered(
            &self,
            _socket_name: &str,
            target_session_name: &str,
        ) -> Result<(), LifecycleError> {
            assert_eq!(target_session_name, "target-1");
            if let Some(live_session) = self
                .live_session
                .lock()
                .expect("live session mutex should not be poisoned")
                .take()
            {
                live_session.running.store(false, Ordering::Relaxed);
                live_session.session.shutdown();
                let _ = UnixStream::connect(&live_session.socket_path);
                let _ = fs::remove_file(live_session.socket_path);
            }
            Ok(())
        }
    }

    #[test]
    fn authority_output_pump_shell_command_quotes_ingest_socket_path() {
        let command = remote_authority_output_pump_shell_command(
            "/tmp/wait agent",
            Path::new("/tmp/output path.sock"),
            Path::new("/tmp/input path.fifo"),
            "wa-test-socket",
            "%42",
        );

        assert_eq!(
            command,
            "'/tmp/wait agent' '__remote-authority-output-pump' '--ingest-socket-path' '/tmp/output path.sock' '--input-fifo-path' '/tmp/input path.fifo' '--socket-name' 'wa-test-socket' '--pane' '%42'"
        );
    }

    #[test]
    fn authority_target_host_args_include_network_and_route_metadata() {
        let args = remote_authority_target_host_args(
            "wa-1",
            "target-1",
            "shell-1",
            "peer-a",
            "remote-peer:peer-a:target-1",
            "/tmp/transport.sock",
            &RemoteNetworkConfig {
                port: 9001,
                connect: Some("10.0.0.8:7474".to_string()),
            },
        );

        assert_eq!(
            args,
            vec![
                "--port",
                "9001",
                "--connect",
                "10.0.0.8:7474",
                "__remote-authority-target-host",
                "--socket-name",
                "wa-1",
                "--target-session-name",
                "target-1",
                "--transport-session-id",
                "shell-1",
                "--authority-id",
                "peer-a",
                "--target-id",
                "remote-peer:peer-a:target-1",
                "--transport-socket-path",
                "/tmp/transport.sock",
            ]
        );
    }

    #[test]
    fn output_pump_reader_forwards_framed_chunks() {
        let socket_path = ingest_socket_path("pump");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            read_output_chunk_frame(&mut stream).expect("frame should decode")
        });

        pump_reader_to_ingest_socket(
            Cursor::new(b"hello".to_vec()),
            socket_path.to_string_lossy().as_ref(),
        )
        .expect("pump should forward bytes");

        let bytes = server.join().expect("server should join cleanly");
        assert_eq!(bytes, b"hello");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn authority_host_runtime_routes_transport_commands_into_gateway_and_output_back_to_transport()
    {
        let socket_name = unique_test_socket_name("wa-1");
        let transport_socket_path = transport_socket_path("host");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway {
            terminal_flags: Arc::new(Mutex::new(RemoteTargetTerminalFlags {
                alternate_screen_active: false,
                application_cursor_keys: false,
                cursor_visible: true,
            })),
            ..FakeGateway::default()
        };
        let fake_publication_gateway = FakePublicationGateway::default();
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway.clone(),
            fake_publication_gateway,
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: socket_name.clone(),
            target_session_name: "target-1".to_string(),
            transport_session_id: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
        };
        let ingest_socket_path = authority_output_ingest_socket_path(
            command.transport_socket_path.as_str(),
            &command.target_id,
        );
        let input_fifo_path =
            authority_input_fifo_path(command.transport_socket_path.as_str(), &command.target_id);
        let server_ingest_socket_path = ingest_socket_path.clone();
        let (server_tx, server_rx) = std::sync::mpsc::channel();
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        let input_reader_path = input_fifo_path.clone();
        thread::spawn(move || {
            wait_for_path(&input_reader_path);
            let mut fifo = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&input_reader_path)
                .expect("input fifo should open");
            let mut bytes = [0_u8; 1];
            fifo.read_exact(&mut bytes)
                .expect("input fifo should receive target input");
            let _ = input_tx.send(bytes.to_vec());
        });
        let server_input_fifo_path = input_fifo_path.clone();
        thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .expect("transport stream should accept read timeout");
            let hello = read_control_plane_envelope(&mut stream).expect("hello should decode");
            let registered = match hello.payload {
                ControlPlanePayload::ClientHello(ClientHelloPayload { node_id, .. }) => node_id,
                other => panic!("unexpected hello payload: {other:?}"),
            };
            assert_eq!(registered, "peer-a");
            write_server_hello(&mut stream, "waitagent-remote-node-session")
                .expect("server hello should encode");
            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: open_mirror_envelope(),
                },
            )
            .expect("open mirror should encode");
            wait_for_path(&server_input_fifo_path);
            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: raw_pty_input_envelope(),
                },
            )
            .expect("raw PTY input should encode");
            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: apply_resize_envelope(),
                },
            )
            .expect("apply resize should encode");
            let mut output_payload = None;
            let mut accepted_payload = None;
            let mut bootstrap_chunk_payload = None;
            let mut bootstrap_complete_payload = None;
            while accepted_payload.is_none()
                || bootstrap_complete_payload.is_none()
                || output_payload.is_none()
            {
                let envelope = read_node_session_envelope(&mut stream).unwrap_or_else(|error| {
                    panic!(
                        "node session should decode while waiting for accepted/bootstrap/output; accepted={} bootstrap_complete={} output={} error={error:?}",
                        accepted_payload.is_some(),
                        bootstrap_complete_payload.is_some(),
                        output_payload.is_some(),
                    )
                });
                match envelope.envelope.payload {
                    payload @ ControlPlanePayload::OpenMirrorAccepted(_) => {
                        if accepted_payload.is_none() {
                            accepted_payload = Some(payload);
                            wait_for_path(&server_ingest_socket_path);
                            let mut output_stream = UnixStream::connect(&server_ingest_socket_path)
                                .expect("ingest socket should accept");
                            write_output_chunk_frame(&mut output_stream, b"hello")
                                .expect("output chunk should encode");
                            drop(output_stream);
                        }
                    }
                    payload @ ControlPlanePayload::MirrorBootstrapChunk(_) => {
                        if bootstrap_chunk_payload.is_none() {
                            bootstrap_chunk_payload = Some(payload);
                        }
                    }
                    payload @ ControlPlanePayload::MirrorBootstrapComplete(_) => {
                        if bootstrap_complete_payload.is_none() {
                            bootstrap_complete_payload = Some(payload);
                        }
                    }
                    payload @ ControlPlanePayload::TargetOutput(_) => {
                        if output_payload.is_none() {
                            output_payload = Some(payload);
                            write_node_session_envelope(
                                &mut stream,
                                &NodeSessionEnvelope {
                                    channel: NodeSessionChannel::Authority,
                                    envelope: close_mirror_envelope(),
                                },
                            )
                            .expect("close mirror should encode");
                        }
                    }
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
            }
            stream
                .shutdown(Shutdown::Write)
                .expect("server shutdown should succeed");
            let _ = server_tx.send((
                accepted_payload.expect("accepted payload should be collected"),
                bootstrap_chunk_payload,
                bootstrap_complete_payload.expect("bootstrap complete payload should be collected"),
                output_payload.expect("output payload should be collected"),
            ));
        });

        let (runtime_tx, runtime_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = runtime_tx.send(runtime.run_target_host(command));
        });

        let (accepted, bootstrap_chunk, bootstrap_complete, output) = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete within timeout");
        runtime_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("runtime should complete within timeout")
            .expect("runtime should finish cleanly");

        assert_eq!(
            input_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("input fifo should receive target input"),
            b"a".to_vec()
        );
        assert_eq!(
            fake_gateway
                .resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .clone(),
            vec![(80, 24), (160, 50)]
        );
        assert_eq!(
            accepted,
            ControlPlanePayload::OpenMirrorAccepted(
                crate::infra::remote_protocol::OpenMirrorAcceptedPayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                    availability: "online",
                }
            )
        );
        assert_eq!(
            bootstrap_chunk,
            Some(ControlPlanePayload::MirrorBootstrapChunk(
                crate::infra::remote_protocol::MirrorBootstrapChunkPayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                    chunk_seq: 1,
                    stream: "pty",
                    output_bytes: b"\x1b[2J\x1b[H\x1b[1;1H".to_vec(),
                }
            ))
        );
        assert_eq!(
            bootstrap_complete,
            ControlPlanePayload::MirrorBootstrapComplete(
                crate::infra::remote_protocol::MirrorBootstrapCompletePayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                    last_chunk_seq: 1,
                    alternate_screen_active: false,
                    application_cursor_keys: false,
                    cursor_visible: true,
                }
            )
        );
        match output {
            ControlPlanePayload::TargetOutput(TargetOutputPayload {
                output_seq,
                ref output_bytes,
                ..
            }) => {
                assert_eq!(output_seq, 1);
                assert_eq!(output_bytes, b"hello");
            }
            other => panic!("unexpected authority output payload: {other:?}"),
        }
        assert!(fake_gateway
            .pipe_calls
            .lock()
            .expect("pipe calls mutex should not be poisoned")[0]
            .contains("__remote-authority-output-pump"));
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn authority_host_runtime_sends_bootstrap_screen_with_ansi_sequences() {
        let socket_name = unique_test_socket_name("wa-ansi");
        let transport_socket_path = transport_socket_path("host-ansi");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway {
            capture_bootstrap_screen: Arc::new(Mutex::new("\u{1b}[32mbash\u{1b}[0m".to_string())),
            cursor_position: Arc::new(Mutex::new((4, 0))),
            terminal_flags: Arc::new(Mutex::new(RemoteTargetTerminalFlags {
                alternate_screen_active: false,
                application_cursor_keys: false,
                cursor_visible: true,
            })),
            ..FakeGateway::default()
        };
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway,
            FakePublicationGateway::default(),
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: socket_name.clone(),
            target_session_name: "target-1".to_string(),
            transport_session_id: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
        };
        let (server_tx, server_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            let hello = read_control_plane_envelope(&mut stream).expect("hello should decode");
            match hello.payload {
                ControlPlanePayload::ClientHello(ClientHelloPayload { .. }) => {}
                other => panic!("unexpected hello payload: {other:?}"),
            }
            write_server_hello(&mut stream, "waitagent-remote-node-session")
                .expect("server hello should encode");
            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: open_mirror_envelope(),
                },
            )
            .expect("open mirror should encode");

            let mut bootstrap_chunk = None;
            let mut bootstrap_complete = None;
            while bootstrap_chunk.is_none() || bootstrap_complete.is_none() {
                let envelope =
                    read_node_session_envelope(&mut stream).expect("node session should decode");
                match envelope.envelope.payload {
                    payload @ ControlPlanePayload::MirrorBootstrapChunk(_) => {
                        if bootstrap_chunk.is_none() {
                            bootstrap_chunk = Some(payload);
                        }
                    }
                    payload @ ControlPlanePayload::MirrorBootstrapComplete(_) => {
                        if bootstrap_complete.is_none() {
                            bootstrap_complete = Some(payload);
                            write_node_session_envelope(
                                &mut stream,
                                &NodeSessionEnvelope {
                                    channel: NodeSessionChannel::Authority,
                                    envelope: close_mirror_envelope(),
                                },
                            )
                            .expect("close mirror should encode");
                        }
                    }
                    _ => {}
                }
            }
            stream
                .shutdown(Shutdown::Write)
                .expect("server shutdown should succeed");
            server_tx
                .send((
                    bootstrap_chunk.expect("bootstrap chunk should exist"),
                    bootstrap_complete.expect("bootstrap complete should exist"),
                ))
                .expect("bootstrap payloads should send");
        });

        runtime
            .run_target_host(command)
            .expect("runtime should finish cleanly");

        let (bootstrap_chunk, bootstrap_complete) = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete");

        match bootstrap_chunk {
            ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                assert_eq!(
                    &payload.output_bytes,
                    b"\x1b[2J\x1b[H\x1b[1;1H\x1b[32mbash\x1b[0m\x1b[1;5H"
                );
            }
            other => panic!("unexpected bootstrap payload: {other:?}"),
        }
        assert_eq!(
            bootstrap_complete,
            ControlPlanePayload::MirrorBootstrapComplete(
                crate::infra::remote_protocol::MirrorBootstrapCompletePayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                    last_chunk_seq: 1,
                    alternate_screen_active: false,
                    application_cursor_keys: false,
                    cursor_visible: true,
                }
            )
        );
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn authority_host_runtime_replays_bootstrap_for_repeated_open_mirror() {
        let socket_name = unique_test_socket_name("wa-reopen");
        let transport_socket_path = transport_socket_path("host-reopen");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway {
            capture_bootstrap_screen: Arc::new(Mutex::new("\u{1b}[32mbash\u{1b}[0m".to_string())),
            cursor_position: Arc::new(Mutex::new((4, 0))),
            terminal_flags: Arc::new(Mutex::new(RemoteTargetTerminalFlags {
                alternate_screen_active: false,
                application_cursor_keys: false,
                cursor_visible: true,
            })),
            ..FakeGateway::default()
        };
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway.clone(),
            FakePublicationGateway::default(),
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: socket_name.clone(),
            target_session_name: "target-1".to_string(),
            transport_session_id: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
        };
        let (server_tx, server_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            let hello = read_control_plane_envelope(&mut stream).expect("hello should decode");
            match hello.payload {
                ControlPlanePayload::ClientHello(ClientHelloPayload { .. }) => {}
                other => panic!("unexpected hello payload: {other:?}"),
            }
            write_server_hello(&mut stream, "waitagent-remote-node-session")
                .expect("server hello should encode");

            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: open_mirror_envelope(),
                },
            )
            .expect("first open mirror should encode");

            let mut accepted_count = 0usize;
            let mut bootstrap_chunk_count = 0usize;
            let mut bootstrap_complete_count = 0usize;
            while accepted_count < 2 || bootstrap_complete_count < 2 || bootstrap_chunk_count < 2 {
                let envelope =
                    read_node_session_envelope(&mut stream).expect("node session should decode");
                match envelope.envelope.payload {
                    ControlPlanePayload::OpenMirrorAccepted(_) => {
                        accepted_count += 1;
                        if accepted_count == 1 {
                            write_node_session_envelope(
                                &mut stream,
                                &NodeSessionEnvelope {
                                    channel: NodeSessionChannel::Authority,
                                    envelope: open_mirror_envelope(),
                                },
                            )
                            .expect("second open mirror should encode");
                        }
                    }
                    ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                        bootstrap_chunk_count += 1;
                        assert_eq!(
                            &payload.output_bytes,
                            b"\x1b[2J\x1b[H\x1b[1;1H\x1b[32mbash\x1b[0m\x1b[1;5H"
                        );
                    }
                    ControlPlanePayload::MirrorBootstrapComplete(payload) => {
                        bootstrap_complete_count += 1;
                        assert_eq!(payload.last_chunk_seq, 1);
                        if bootstrap_complete_count == 2 {
                            write_node_session_envelope(
                                &mut stream,
                                &NodeSessionEnvelope {
                                    channel: NodeSessionChannel::Authority,
                                    envelope: close_mirror_envelope(),
                                },
                            )
                            .expect("close mirror should encode");
                        }
                    }
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
            }
            stream
                .shutdown(Shutdown::Write)
                .expect("server shutdown should succeed");
            server_tx
                .send((
                    accepted_count,
                    bootstrap_chunk_count,
                    bootstrap_complete_count,
                ))
                .expect("counts should send");
        });

        runtime
            .run_target_host(command)
            .expect("runtime should finish cleanly");

        let (accepted_count, bootstrap_chunk_count, bootstrap_complete_count) = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete");

        assert_eq!(accepted_count, 2);
        assert_eq!(bootstrap_chunk_count, 2);
        assert_eq!(bootstrap_complete_count, 2);
        assert_eq!(
            fake_gateway
                .resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .clone(),
            vec![(80, 24), (80, 24)]
        );
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn bootstrap_replay_preserves_trailing_prompt_space_and_cursor() {
        let replay = render_bootstrap_replay("kk@lenovo:~/wait-agent$ ", 24, 0);

        assert_eq!(
            replay,
            "\x1b[2J\x1b[H\x1b[1;1Hkk@lenovo:~/wait-agent$ \x1b[1;25H"
        );
    }

    #[test]
    fn authority_output_ingest_socket_path_scopes_by_transport_and_target() {
        let path = authority_output_ingest_socket_path(
            "/tmp/waitagent-remote-wa-1-workspace-1-peer-a_shell-1.sock",
            "remote-peer:peer-a:shell-1",
        );

        let rendered = path.to_string_lossy();
        assert!(rendered.contains("waitagent-authority-output-"));
        assert!(rendered.ends_with(".sock"));
    }

    fn wait_for_path(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("path did not appear at {}", path.display());
    }

    fn transport_socket_path(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-authority-transport-{name}-{}-{millis}.sock",
            process::id()
        ))
    }

    fn ingest_socket_path(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-authority-ingest-{name}-{}-{millis}.sock",
            process::id()
        ))
    }

    fn unique_test_socket_name(prefix: &str) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("{prefix}-{}-{millis}", process::id())
    }

    fn raw_pty_input_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-raw-pty-input".to_string(),
            message_type: "raw_pty_input",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some("target-1".to_string()),
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::RawPtyInput(RawPtyInputPayload {
                attachment_id: "attach-1".to_string(),
                session_id: "target-1".to_string(),
                target_id: "remote-peer:peer-a:target-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 1,
                input_bytes: b"a".to_vec(),
            }),
        }
    }

    fn open_mirror_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-open-mirror".to_string(),
            message_type: "open_mirror_request",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some("target-1".to_string()),
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: None,
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
                session_id: "target-1".to_string(),
                target_id: "remote-peer:peer-a:target-1".to_string(),
                console_id: "console-a".to_string(),
                cols: 80,
                rows: 24,
                raw_pty_passthrough: false,
                bootstrap_mode: BootstrapMode::Full,
            }),
        }
    }

    fn close_mirror_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-close-mirror".to_string(),
            message_type: "close_mirror_request",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some("target-1".to_string()),
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: None,
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::CloseMirrorRequest(
                crate::infra::remote_protocol::CloseMirrorRequestPayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                },
            ),
        }
    }

    fn apply_resize_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-apply-resize".to_string(),
            message_type: "apply_resize",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some("target-1".to_string()),
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::ApplyResize(ApplyResizePayload {
                session_id: "target-1".to_string(),
                target_id: "remote-peer:peer-a:target-1".to_string(),
                resize_epoch: 2,
                resize_authority_console_id: "console-a".to_string(),
                cols: 160,
                rows: 50,
            }),
        }
    }
}
