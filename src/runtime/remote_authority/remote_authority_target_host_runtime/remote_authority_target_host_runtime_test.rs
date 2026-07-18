mod tests {
    use super::super::{
        authority_event_socket_path, authority_input_socket_path,
        authority_output_ingest_socket_path, pump_reader_to_ingest_socket, read_output_chunk_frame,
        read_stream_id_frame, remote_authority_error, remote_authority_output_pump_shell_command,
        remote_authority_target_host_args, render_bootstrap_replay, write_output_chunk_frame,
        write_stream_id_frame, LifecycleError, RemoteAuthorityPublicationGateway,
        RemoteAuthorityTargetHostRuntime, RemoteTargetPtyGateway, RemoteTargetTerminalFlags,
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
    use std::io::{Cursor, Read, Write};
    use std::net::Shutdown;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone)]
    struct FakeGateway {
        current_pane: Arc<Mutex<String>>,
        resize_calls: Arc<Mutex<Vec<(String, usize, usize)>>>,
        pipe_calls: Arc<Mutex<Vec<(String, String)>>>,
        pipe_live: Arc<Mutex<bool>>,
        clear_calls: Arc<Mutex<usize>>,
        hook_calls: Arc<Mutex<Vec<(String, String)>>>,
        clear_hook_calls: Arc<Mutex<Vec<String>>>,
        clear_runtime_override_calls: Arc<Mutex<Vec<String>>>,
        chrome_refresh_calls: Arc<Mutex<Vec<String>>>,
        runtime_signal_order: Arc<Mutex<Vec<String>>>,
        capture_bootstrap_screen: Arc<Mutex<String>>,
        cursor_position: Arc<Mutex<(usize, usize)>>,
        terminal_flags: Arc<Mutex<RemoteTargetTerminalFlags>>,
    }

    impl Default for FakeGateway {
        fn default() -> Self {
            Self {
                current_pane: Arc::new(Mutex::new("%7".to_string())),
                resize_calls: Arc::new(Mutex::new(Vec::new())),
                pipe_calls: Arc::new(Mutex::new(Vec::new())),
                pipe_live: Arc::new(Mutex::new(false)),
                clear_calls: Arc::new(Mutex::new(0)),
                hook_calls: Arc::new(Mutex::new(Vec::new())),
                clear_hook_calls: Arc::new(Mutex::new(Vec::new())),
                clear_runtime_override_calls: Arc::new(Mutex::new(Vec::new())),
                chrome_refresh_calls: Arc::new(Mutex::new(Vec::new())),
                runtime_signal_order: Arc::new(Mutex::new(Vec::new())),
                capture_bootstrap_screen: Arc::new(Mutex::new(String::new())),
                cursor_position: Arc::new(Mutex::new((0, 0))),
                terminal_flags: Arc::new(Mutex::new(RemoteTargetTerminalFlags::default())),
            }
        }
    }

    impl FakeGateway {
        fn set_current_pane(&self, pane: &str) {
            *self
                .current_pane
                .lock()
                .expect("current pane mutex should not be poisoned") = pane.to_string();
        }
    }

    impl RemoteTargetPtyGateway for FakeGateway {
        type Error = &'static str;

        fn target_pty_pane(
            &self,
            _socket_name: &str,
            _target_session_name: &str,
        ) -> Result<TmuxPaneId, Self::Error> {
            Ok(TmuxPaneId::new(
                self.current_pane
                    .lock()
                    .expect("current pane mutex should not be poisoned")
                    .clone(),
            ))
        }

        fn resize_pty(
            &self,
            _socket_name: &str,
            pane: &TmuxPaneId,
            cols: usize,
            rows: usize,
        ) -> Result<(), Self::Error> {
            self.resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .push((pane.as_str().to_string(), cols, rows));
            Ok(())
        }

        fn clear_output_pipe_if_owner(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            _owner: &str,
        ) -> Result<bool, Self::Error> {
            let mut clear_calls = self
                .clear_calls
                .lock()
                .expect("clear calls mutex should not be poisoned");
            *clear_calls += 1;
            Ok(true)
        }

        fn output_pipe_is_live(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            _owner: &str,
        ) -> Result<bool, Self::Error> {
            Ok(*self
                .pipe_live
                .lock()
                .expect("pipe live mutex should not be poisoned"))
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

        fn set_output_pipe_owned(
            &self,
            _socket_name: &str,
            pane: &TmuxPaneId,
            _owner: &str,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.pipe_calls
                .lock()
                .expect("pipe calls mutex should not be poisoned")
                .push((pane.as_str().to_string(), command.to_string()));
            *self
                .pipe_live
                .lock()
                .expect("pipe live mutex should not be poisoned") = true;
            Ok(())
        }

        fn set_pane_died_hook(
            &self,
            _socket_name: &str,
            pane: &TmuxPaneId,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.hook_calls
                .lock()
                .expect("hook calls mutex should not be poisoned")
                .push((pane.as_str().to_string(), command.to_string()));
            Ok(())
        }

        fn clear_pane_died_hook(
            &self,
            _socket_name: &str,
            pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            self.clear_hook_calls
                .lock()
                .expect("clear hook calls mutex should not be poisoned")
                .push(pane.as_str().to_string());
            Ok(())
        }

        fn clear_runtime_command_override(
            &self,
            _socket_name: &str,
            pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            self.clear_runtime_override_calls
                .lock()
                .expect("clear runtime override calls mutex should not be poisoned")
                .push(pane.as_str().to_string());
            Ok(())
        }

        fn signal_chrome_refresh_targets(&self, socket_name: &str) -> Result<(), Self::Error> {
            self.chrome_refresh_calls
                .lock()
                .expect("chrome refresh calls mutex should not be poisoned")
                .push(socket_name.to_string());
            self.runtime_signal_order
                .lock()
                .expect("runtime signal order mutex should not be poisoned")
                .push("chrome-refresh".to_string());
            Ok(())
        }
    }

    struct FakeLiveSession {
        session: Arc<RemoteNodeSessionRuntime>,
        socket_path: PathBuf,
        running: Arc<AtomicBool>,
        worker: thread::JoinHandle<()>,
    }

    #[derive(Clone, Default)]
    struct FakePublicationGateway {
        live_session: Arc<Mutex<Option<FakeLiveSession>>>,
        closed_sessions: Arc<Mutex<Vec<String>>>,
        runtime_changed_sockets: Arc<Mutex<Vec<String>>>,
        runtime_signal_order: Arc<Mutex<Vec<String>>>,
        runtime_change_error: Arc<Mutex<Option<String>>>,
    }

    impl RemoteAuthorityPublicationGateway for FakePublicationGateway {
        fn ensure_live_session_registered(
            &self,
            _socket_name: &str,
            _target_session_name: &str,
            authority_id: &str,
            _target_id: &str,
            transport_socket_path: &str,
            authority_socket_path: &std::path::Path,
        ) -> Result<(), LifecycleError> {
            let session = Arc::new(
                RemoteNodeSessionRuntime::connect(transport_socket_path, authority_id, None)
                    .map_err(remote_authority_error)?,
            );
            let running = Arc::new(AtomicBool::new(true));
            let socket_path = authority_socket_path.to_path_buf();
            let worker = spawn_live_authority_session_bridge(
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
                worker,
            });
            Ok(())
        }

        fn ensure_live_session_unregistered(
            &self,
            _socket_name: &str,
            target_session_name: &str,
        ) -> Result<(), LifecycleError> {
            assert!(target_session_name.starts_with("target-"));
            if let Some(live_session) = self
                .live_session
                .lock()
                .expect("live session mutex should not be poisoned")
                .take()
            {
                live_session.running.store(false, Ordering::Relaxed);
                let _ = UnixStream::connect(&live_session.socket_path);
                let _ = live_session.worker.join();
                live_session.session.shutdown();
                let _ = fs::remove_file(live_session.socket_path);
            }
            Ok(())
        }

        fn signal_source_session_closed(
            &self,
            socket_name: &str,
            target_session_name: &str,
        ) -> Result<(), LifecycleError> {
            let _ = socket_name;
            self.closed_sessions
                .lock()
                .expect("closed sessions mutex should not be poisoned")
                .push(target_session_name.to_string());
            Ok(())
        }

        fn signal_local_runtime_changed(&self, socket_name: &str) -> Result<(), LifecycleError> {
            self.runtime_changed_sockets
                .lock()
                .expect("runtime changed sockets mutex should not be poisoned")
                .push(socket_name.to_string());
            self.runtime_signal_order
                .lock()
                .expect("runtime signal order mutex should not be poisoned")
                .push("local-runtime-changed".to_string());
            if let Some(message) = self
                .runtime_change_error
                .lock()
                .expect("runtime change error mutex should not be poisoned")
                .clone()
            {
                return Err(LifecycleError::Protocol(message));
            }
            Ok(())
        }
    }

    fn test_authority_socket_path(socket_name: &str, target_session_name: &str) -> String {
        live_authority_session_socket_path(
            socket_name,
            target_session_name,
            "test-session-instance",
        )
        .to_string_lossy()
        .into_owned()
    }

    #[test]
    fn authority_output_pump_shell_command_quotes_ingest_socket_path() {
        let command = remote_authority_output_pump_shell_command(
            "/tmp/wait agent",
            Path::new("/tmp/output path.sock"),
            Path::new("/tmp/input path.sock"),
            "wa-test-socket",
            42,
        );

        assert_eq!(
            command,
            "'/tmp/wait agent' '__remote-authority-output-pump' '--ingest-socket-path' '/tmp/output path.sock' '--input-socket-path' '/tmp/input path.sock' '--socket-name' 'wa-test-socket' '--stream-id' '42'"
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
            "/tmp/authority.sock",
            &RemoteNetworkConfig {
                port: 9001,
                connect: Some("10.0.0.8:7474".to_string()),
                node_id: None,
                public_endpoint: None,
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
                "--authority-socket-path",
                "/tmp/authority.sock",
            ]
        );
    }

    #[test]
    fn output_pump_reader_forwards_framed_chunks() {
        let socket_path = ingest_socket_path("pump");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            let stream_id = read_stream_id_frame(&mut stream).expect("stream id should decode");
            let bytes = read_output_chunk_frame(&mut stream).expect("frame should decode");
            (stream_id, bytes)
        });

        let mut reader = Cursor::new(b"hello".to_vec());
        pump_reader_to_ingest_socket(&mut reader, socket_path.to_string_lossy().as_ref(), 42)
            .expect("pump should forward bytes");

        let (stream_id, bytes) = server.join().expect("server should join cleanly");
        assert_eq!(stream_id, 42);
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
        let fake_publication_gateway = FakePublicationGateway {
            runtime_signal_order: fake_gateway.runtime_signal_order.clone(),
            ..FakePublicationGateway::default()
        };
        let fake_publication_gateway_for_assert = fake_publication_gateway.clone();
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
            authority_socket_path: test_authority_socket_path(&socket_name, "target-1"),
        };
        let ingest_socket_path = authority_output_ingest_socket_path(
            command.transport_socket_path.as_str(),
            &command.target_id,
        );
        let input_socket_path =
            authority_input_socket_path(command.transport_socket_path.as_str(), &command.target_id);
        let _ = fs::remove_file(&input_socket_path);
        let input_listener =
            UnixListener::bind(&input_socket_path).expect("input socket should bind");
        let server_ingest_socket_path = ingest_socket_path.clone();
        let (server_tx, server_rx) = std::sync::mpsc::channel();
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
            let (mut input_stream, _) =
                input_listener.accept().expect("input socket should accept");
            input_stream
                .set_read_timeout(Some(std::time::Duration::from_millis(100)))
                .expect("input stream should accept read timeout");
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
                            write_stream_id_frame(&mut output_stream, 1)
                                .expect("stream id should encode");
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
                    ControlPlanePayload::ResizeApplied(_) => {
                        // Resize ack is expected after each resize_pty; the test
                        // only verifies that the gateway received the resize calls.
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
                .shutdown(Shutdown::Both)
                .expect("server shutdown should succeed");
            let mut input_bytes = Vec::new();
            input_stream.read_to_end(&mut input_bytes).ok();
            let _ = server_tx.send((
                accepted_payload.expect("accepted payload should be collected"),
                bootstrap_chunk_payload,
                bootstrap_complete_payload.expect("bootstrap complete payload should be collected"),
                output_payload.expect("output payload should be collected"),
                input_bytes,
            ));
        });

        let (runtime_tx, runtime_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = runtime_tx.send(runtime.run_target_host(command));
        });

        let (accepted, bootstrap_chunk, bootstrap_complete, output, input_bytes) = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete within timeout");
        runtime_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("runtime should complete within timeout")
            .expect("runtime should finish cleanly");

        assert_eq!(input_bytes, b"a");
        assert_eq!(
            fake_gateway
                .resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .clone(),
            vec![("%7".to_string(), 80, 24), ("%7".to_string(), 160, 50)]
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
            .1
            .contains("__remote-authority-output-pump"));
        assert_eq!(
            fake_gateway
                .clear_runtime_override_calls
                .lock()
                .expect("clear runtime override calls mutex should not be poisoned")
                .clone(),
            vec!["%7".to_string()]
        );
        assert_eq!(
            fake_gateway
                .chrome_refresh_calls
                .lock()
                .expect("chrome refresh calls mutex should not be poisoned")
                .clone(),
            vec![socket_name.clone()]
        );
        assert_eq!(
            fake_publication_gateway_for_assert
                .runtime_changed_sockets
                .lock()
                .expect("runtime changed sockets mutex should not be poisoned")
                .clone(),
            vec![socket_name.clone()]
        );
        assert_eq!(
            fake_gateway
                .runtime_signal_order
                .lock()
                .expect("runtime signal order mutex should not be poisoned")
                .as_slice(),
            &[
                "local-runtime-changed".to_string(),
                "chrome-refresh".to_string()
            ]
        );
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn authority_host_runtime_exits_from_pane_died_event_without_active_mirror() {
        let socket_name = unique_test_socket_name("wa-pane-died");
        let transport_socket_path = transport_socket_path("host-pane-died");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway::default();
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
            authority_socket_path: test_authority_socket_path(&socket_name, "target-1"),
        };
        let event_socket_path =
            authority_event_socket_path(command.transport_socket_path.as_str(), &command.target_id);

        let (server_tx, server_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("transport stream should accept read timeout");
            let hello = read_control_plane_envelope(&mut stream).expect("hello should decode");
            match hello.payload {
                ControlPlanePayload::ClientHello(ClientHelloPayload { .. }) => {}
                other => panic!("unexpected hello payload: {other:?}"),
            }
            write_server_hello(&mut stream, "waitagent-remote-node-session")
                .expect("server hello should encode");
            ready_tx.send(()).expect("server readiness should send");

            let payload = loop {
                let envelope = read_node_session_envelope(&mut stream)
                    .expect("pane-died should produce TargetExited");
                match envelope.envelope.payload {
                    ControlPlanePayload::TargetExited(payload) => break payload,
                    ControlPlanePayload::ResizeApplied(_) => {}
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
            };
            server_tx.send(payload).expect("payload should send");
        });

        let (runtime_tx, runtime_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = runtime_tx.send(runtime.run_target_host(command));
        });

        wait_for_path(&event_socket_path);
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server hello should complete before pane event");
        let mut event_stream =
            UnixStream::connect(&event_socket_path).expect("event socket should accept");
        event_stream
            .write_all(b"%7\n")
            .expect("pane event should write");
        drop(event_stream);

        let payload = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("TargetExited should arrive");
        runtime_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("runtime should complete within timeout")
            .expect("runtime should finish cleanly");

        assert_eq!(payload.transport_session_id, "target-1");
        assert_eq!(payload.source_session_name.as_deref(), Some("target-1"));
        assert!(fake_gateway
            .hook_calls
            .lock()
            .expect("hook calls mutex should not be poisoned")[0]
            .1
            .contains("__remote-authority-pane-died"));
        assert_eq!(
            fake_gateway
                .clear_hook_calls
                .lock()
                .expect("clear hook calls mutex should not be poisoned")
                .clone(),
            vec!["%7".to_string()]
        );
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
            authority_socket_path: test_authority_socket_path(&socket_name, "target-1"),
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
                .shutdown(Shutdown::Both)
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
    fn raw_pty_passthrough_output_uses_raw_channel_and_signals_runtime_refresh() {
        let socket_name = unique_test_socket_name("wa-raw-output");
        let transport_socket_path = transport_socket_path("host-raw-output");
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
        let fake_gateway_for_assert = fake_gateway.clone();
        let fake_publication = FakePublicationGateway {
            runtime_signal_order: fake_gateway.runtime_signal_order.clone(),
            ..FakePublicationGateway::default()
        };
        let fake_publication_for_assert = fake_publication.clone();
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway,
            fake_publication,
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: socket_name.clone(),
            target_session_name: "target-1".to_string(),
            transport_session_id: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
            authority_socket_path: test_authority_socket_path(&socket_name, "target-1"),
        };
        let ingest_socket_path = authority_output_ingest_socket_path(
            command.transport_socket_path.as_str(),
            &command.target_id,
        );
        let server_ingest_socket_path = ingest_socket_path.clone();
        let (server_tx, server_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .expect("transport stream should accept read timeout");
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
                    envelope: open_mirror_envelope_with_raw_passthrough(true),
                },
            )
            .expect("open mirror should encode");

            let mut raw_output = None;
            while raw_output.is_none() {
                let envelope =
                    read_node_session_envelope(&mut stream).expect("node session should decode");
                match envelope.envelope.payload {
                    ControlPlanePayload::OpenMirrorAccepted(_) => {
                        wait_for_path(&server_ingest_socket_path);
                        let mut output_stream = UnixStream::connect(&server_ingest_socket_path)
                            .expect("ingest socket should accept");
                        write_stream_id_frame(&mut output_stream, 1)
                            .expect("stream id should encode");
                        write_output_chunk_frame(&mut output_stream, b"echo")
                            .expect("output chunk should encode");
                        drop(output_stream);
                    }
                    ControlPlanePayload::RawPtyOutput(payload) => {
                        raw_output = Some(payload);
                        write_node_session_envelope(
                            &mut stream,
                            &NodeSessionEnvelope {
                                channel: NodeSessionChannel::Authority,
                                envelope: close_mirror_envelope(),
                            },
                        )
                        .expect("close mirror should encode");
                    }
                    ControlPlanePayload::MirrorBootstrapChunk(_)
                    | ControlPlanePayload::MirrorBootstrapComplete(_) => {}
                    ControlPlanePayload::ResizeApplied(_) => {}
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
            }
            stream
                .shutdown(Shutdown::Both)
                .expect("server shutdown should succeed");
            server_tx
                .send(raw_output.expect("raw output should be collected"))
                .expect("raw output should send");
        });

        runtime
            .run_target_host(command)
            .expect("runtime should finish cleanly");

        let raw_output = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete");
        assert_eq!(raw_output.output_seq, 1);
        assert_eq!(raw_output.output_bytes, b"echo");
        assert_eq!(
            fake_gateway_for_assert
                .clear_runtime_override_calls
                .lock()
                .expect("clear runtime override calls mutex should not be poisoned")
                .as_slice(),
            &["%7".to_string()]
        );
        assert_eq!(
            fake_gateway_for_assert
                .chrome_refresh_calls
                .lock()
                .expect("chrome refresh calls mutex should not be poisoned")
                .as_slice(),
            &[socket_name.clone()]
        );
        assert_eq!(
            fake_publication_for_assert
                .runtime_changed_sockets
                .lock()
                .expect("runtime changed sockets mutex should not be poisoned")
                .as_slice(),
            &[socket_name]
        );
        assert_eq!(
            fake_gateway_for_assert
                .runtime_signal_order
                .lock()
                .expect("runtime signal order mutex should not be poisoned")
                .as_slice(),
            &[
                "local-runtime-changed".to_string(),
                "chrome-refresh".to_string()
            ]
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
            authority_socket_path: test_authority_socket_path(&socket_name, "target-1"),
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
                    ControlPlanePayload::ResizeApplied(_) => {}
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
            }
            stream
                .shutdown(Shutdown::Both)
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
            vec![("%7".to_string(), 80, 24), ("%7".to_string(), 80, 24)]
        );
        assert_eq!(
            fake_gateway
                .pipe_calls
                .lock()
                .expect("pipe calls mutex should not be poisoned")
                .len(),
            1
        );
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn authority_host_runtime_reactivates_repeated_open_when_output_pipe_is_missing() {
        let socket_name = unique_test_socket_name("wa-reopen-missing-pipe");
        let transport_socket_path = transport_socket_path("host-reopen-missing-pipe");
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
        let fake_gateway_for_server = fake_gateway.clone();
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
            authority_socket_path: test_authority_socket_path(&socket_name, "target-1"),
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
            let mut bootstrap_complete_count = 0usize;
            while accepted_count < 2 || bootstrap_complete_count < 2 {
                let envelope =
                    read_node_session_envelope(&mut stream).expect("node session should decode");
                match envelope.envelope.payload {
                    ControlPlanePayload::OpenMirrorAccepted(_) => {
                        accepted_count += 1;
                        if accepted_count == 1 {
                            *fake_gateway_for_server
                                .pipe_live
                                .lock()
                                .expect("pipe live mutex should not be poisoned") = false;
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
                    ControlPlanePayload::MirrorBootstrapComplete(_) => {
                        bootstrap_complete_count += 1;
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
                    ControlPlanePayload::MirrorBootstrapChunk(_) => {}
                    ControlPlanePayload::ResizeApplied(_) => {}
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
            }
            stream
                .shutdown(Shutdown::Both)
                .expect("server shutdown should succeed");
            server_tx
                .send((accepted_count, bootstrap_complete_count))
                .expect("counts should send");
        });

        runtime
            .run_target_host(command)
            .expect("runtime should finish cleanly");

        let (accepted_count, bootstrap_complete_count) = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete");

        assert_eq!(accepted_count, 2);
        assert_eq!(bootstrap_complete_count, 2);
        assert_eq!(
            fake_gateway
                .pipe_calls
                .lock()
                .expect("pipe calls mutex should not be poisoned")
                .len(),
            2
        );
        assert_eq!(
            fake_gateway
                .resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .clone(),
            vec![("%7".to_string(), 80, 24), ("%7".to_string(), 80, 24)]
        );
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn authority_host_runtime_rebinds_active_mirror_on_binding_refresh() {
        let socket_name = unique_test_socket_name("wa-refresh-active-mirror");
        let transport_socket_path = transport_socket_path("host-refresh-active-mirror");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway {
            capture_bootstrap_screen: Arc::new(Mutex::new("bash".to_string())),
            cursor_position: Arc::new(Mutex::new((4, 0))),
            ..FakeGateway::default()
        };
        let fake_gateway_for_server = fake_gateway.clone();
        let target_id = "remote-peer:peer-a:target-1".to_string();
        let event_socket_path = authority_event_socket_path(
            transport_socket_path.to_string_lossy().as_ref(),
            &target_id,
        );
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
            target_id,
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
            authority_socket_path: test_authority_socket_path(&socket_name, "target-1"),
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
            wait_for_pipe_count(&fake_gateway_for_server, 1);
            fake_gateway_for_server.set_current_pane("%8");
            wait_for_path(&event_socket_path);
            let mut event_stream =
                UnixStream::connect(&event_socket_path).expect("event socket should accept");
            event_stream
                .write_all(b"__refresh_binding\n")
                .expect("refresh event should write");
            event_stream.flush().expect("refresh event should flush");
            event_stream
                .shutdown(Shutdown::Write)
                .expect("refresh event should close write side");
            wait_for_pipe_count(&fake_gateway_for_server, 2);
            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: close_mirror_envelope(),
                },
            )
            .expect("close mirror should encode");
            stream
                .shutdown(Shutdown::Both)
                .expect("server shutdown should succeed");
            server_tx.send(()).expect("server completion should send");
        });

        runtime
            .run_target_host(command)
            .expect("runtime should finish cleanly");
        server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete");

        assert_eq!(
            fake_gateway
                .pipe_calls
                .lock()
                .expect("pipe calls mutex should not be poisoned")
                .iter()
                .map(|(pane, _)| pane.clone())
                .collect::<Vec<_>>(),
            vec!["%7".to_string(), "%8".to_string()]
        );
        assert!(fake_gateway
            .clear_hook_calls
            .lock()
            .expect("clear hook calls mutex should not be poisoned")
            .contains(&"%7".to_string()));
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

    #[test]
    fn authority_output_ingest_socket_path_preserves_host_port_authority_session_boundary() {
        let path = authority_output_ingest_socket_path(
            "/tmp/waitagent-remote-wa-1-workspace-1-10.1.26.84_7474_target.sock",
            "remote-peer:10.1.26.84:7474:6a1b816eb1111435",
        );

        let rendered = path.to_string_lossy();
        assert!(rendered.contains("waitagent-authority-output-"));
        assert!(rendered.ends_with(".sock"));
        assert!(!rendered.contains("7474:6a1b816eb1111435"));
    }

    #[test]
    fn output_runtime_change_signal_failure_does_not_skip_chrome_refresh() {
        let socket_name = unique_test_socket_name("wa-runtime-signal-failure");
        let fake_gateway = FakeGateway::default();
        let fake_publication = FakePublicationGateway {
            runtime_signal_order: fake_gateway.runtime_signal_order.clone(),
            runtime_change_error: Arc::new(Mutex::new(Some("owner busy".to_string()))),
            ..FakePublicationGateway::default()
        };
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway.clone(),
            fake_publication.clone(),
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: socket_name.clone(),
            target_session_name: "target-1".to_string(),
            transport_session_id: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: "/tmp/transport.sock".to_string(),
            authority_socket_path: test_authority_socket_path(&socket_name, "target-1"),
        };
        let (event_tx, _event_rx) = std::sync::mpsc::channel();
        let signal = super::super::RuntimeChangeSignal::new(event_tx, std::time::Duration::ZERO);

        let pane = TmuxPaneId::new("%7");
        super::super::emit_runtime_change_signal(&runtime, &command, &signal, &pane)
            .expect("chrome refresh should still be signaled");

        assert_eq!(
            fake_publication
                .runtime_changed_sockets
                .lock()
                .expect("runtime changed sockets mutex should not be poisoned")
                .as_slice(),
            &[socket_name.clone()]
        );
        assert_eq!(
            fake_gateway
                .chrome_refresh_calls
                .lock()
                .expect("chrome refresh calls mutex should not be poisoned")
                .as_slice(),
            &[socket_name]
        );
        assert_eq!(
            fake_gateway
                .runtime_signal_order
                .lock()
                .expect("runtime signal order mutex should not be poisoned")
                .as_slice(),
            &[
                "local-runtime-changed".to_string(),
                "chrome-refresh".to_string()
            ]
        );
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

    fn wait_for_pipe_count(fake_gateway: &FakeGateway, expected: usize) {
        for _ in 0..100 {
            if fake_gateway
                .pipe_calls
                .lock()
                .expect("pipe calls mutex should not be poisoned")
                .len()
                >= expected
            {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("pipe count did not reach {expected}");
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
        raw_input_envelope(b"a")
    }

    fn raw_input_envelope(bytes: &[u8]) -> ProtocolEnvelope<ControlPlanePayload> {
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
                input_bytes: bytes.to_vec(),
            }),
        }
    }

    fn open_mirror_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        open_mirror_envelope_with_raw_passthrough(false)
    }

    fn open_mirror_envelope_with_raw_passthrough(
        raw_pty_passthrough: bool,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
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
                raw_pty_passthrough,
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
