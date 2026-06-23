mod tests {
    use super::super::{
        register_authority_stream, register_authority_stream_with_timeouts,
        spawn_authority_listener, AuthorityConnectionRequest, AuthorityConnectionStarter,
        AuthorityTransportEvent, LocalAuthoritySocketBridgeStarter, QueuedAuthorityStreamSource,
        QueuedAuthorityStreamStarter, RemoteAuthorityConnectionRuntime,
    };
    use crate::infra::remote_protocol::{
        ControlPlanePayload, ProtocolEnvelope, RawPtyInputPayload, RawPtyOutputPayload,
        TargetOutputPayload,
    };
    use crate::infra::remote_transport_codec::{
        read_authority_transport_frame, read_control_plane_envelope,
        write_authority_transport_frame, write_control_plane_envelope, write_registration_frame,
        AuthorityTransportFrame,
    };
    use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
    use std::fs;
    use std::os::unix::net::UnixStream;
    use std::process;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn register_authority_stream_tracks_connection_and_forwards_inbound_envelopes() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");
        register_authority_stream(server, registry.clone(), "peer-a".to_string(), tx)
            .expect("authority stream should register");

        assert!(registry.has_connection("peer-a"));
        assert_eq!(
            rx.recv().expect("transport event should be emitted"),
            AuthorityTransportEvent::Connected
        );

        write_control_plane_envelope(&mut client, &authority_target_output_envelope(1))
            .expect("target output should encode");
        match rx.recv().expect("authority envelope should arrive") {
            AuthorityTransportEvent::Envelope(envelope) => {
                assert_eq!(envelope.sender_id, "peer-a");
                match envelope.payload {
                    ControlPlanePayload::TargetOutput(payload) => {
                        assert_eq!(payload.output_seq, 1);
                        assert_eq!(payload.output_bytes, b"a".to_vec());
                    }
                    other => panic!("unexpected payload: {other:?}"),
                }
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn register_authority_stream_forwards_raw_pty_output_without_envelope() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");
        register_authority_stream(server, registry.clone(), "peer-a".to_string(), tx)
            .expect("authority stream should register");

        assert_eq!(
            rx.recv().expect("transport event should be emitted"),
            AuthorityTransportEvent::Connected
        );

        write_authority_transport_frame(
            &mut client,
            &AuthorityTransportFrame::RawPtyOutput(RawPtyOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq: 7,
                output_bytes: b"raw".to_vec(),
            }),
        )
        .expect("raw output frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("raw output event should arrive"),
            AuthorityTransportEvent::RawPtyOutput {
                authority_id: "peer-a".to_string(),
                payload: RawPtyOutputPayload {
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    output_seq: 7,
                    output_bytes: b"raw".to_vec(),
                },
            }
        );
    }

    #[test]
    fn register_authority_stream_stays_registered_after_idle_keepalive_pong() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");
        register_authority_stream_with_timeouts(
            server,
            registry.clone(),
            "peer-a".to_string(),
            tx,
            Duration::from_millis(20),
            Duration::from_millis(60),
        )
        .expect("authority stream should register");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        let frame = read_authority_transport_frame(&mut client)
            .expect("idle reader should send keepalive ping");
        assert_eq!(frame, AuthorityTransportFrame::Ping);
        write_authority_transport_frame(&mut client, &AuthorityTransportFrame::Pong)
            .expect("pong should encode");
        std::thread::sleep(Duration::from_millis(30));
        assert!(registry.has_connection("peer-a"));

        write_control_plane_envelope(&mut client, &authority_target_output_envelope(21))
            .expect("target output should encode after keepalive");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive after keepalive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 21);
                    assert_eq!(payload.output_bytes, b"a".to_vec());
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn register_authority_stream_requests_sync_on_raw_output_gap() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");
        register_authority_stream(server, registry, "peer-a".to_string(), tx)
            .expect("authority stream should register");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );

        write_authority_transport_frame(
            &mut client,
            &AuthorityTransportFrame::RawPtyOutput(RawPtyOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq: 1,
                output_bytes: b"one".to_vec(),
            }),
        )
        .expect("first raw output should encode");
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Ok(AuthorityTransportEvent::RawPtyOutput { .. })
        ));

        write_authority_transport_frame(
            &mut client,
            &AuthorityTransportFrame::RawPtyOutput(RawPtyOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq: 3,
                output_bytes: b"three".to_vec(),
            }),
        )
        .expect("gapped raw output should encode");
        let sync =
            read_authority_transport_frame(&mut client).expect("gap should trigger sync request");
        assert_eq!(
            sync,
            AuthorityTransportFrame::SyncRequest {
                expected_seq: 2,
                received_seq: 3,
            }
        );
    }

    #[test]
    fn authority_connection_marks_closed_after_raw_input_write_failure() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");
        register_authority_stream(server, registry.clone(), "peer-a".to_string(), tx)
            .expect("authority stream should register");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );

        drop(client);
        let connection = registry
            .connection_for("peer-a")
            .expect("authority connection should be registered");

        let mut first_error = None;
        for _ in 0..8 {
            match connection.send_raw_pty_input(&raw_pty_input_payload()) {
                Err(error) => {
                    first_error = Some(error);
                    break;
                }
                Ok(()) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let first_error = first_error.expect("closed peer should eventually fail raw input write");
        assert!(!first_error.to_string().is_empty());

        let second_error = connection
            .send_raw_pty_input(&raw_pty_input_payload())
            .expect_err("failed write should mark connection closed");
        assert_eq!(
            second_error.to_string(),
            "authority transport connection is closed"
        );
    }

    #[test]
    fn register_authority_stream_rejects_unexpected_node_id() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, _rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-b").expect("registration frame should encode");
        let error = register_authority_stream(server, registry.clone(), "peer-a".to_string(), tx)
            .expect_err("unexpected authority node should fail");

        assert_eq!(
            error.to_string(),
            "unexpected authority node `peer-b`; expected `peer-a`"
        );
        assert!(!registry.has_connection("peer-a"));
        assert!(!registry.has_connection("peer-b"));
    }

    #[test]
    fn spawned_listener_reports_failed_registrations_without_registering_connections() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let socket_path = test_socket_path("failed-registration");
        let _guard = spawn_authority_listener(
            AuthorityConnectionRequest {
                socket_path: socket_path.clone(),
                authority_id: "peer-a".to_string(),
            },
            registry.clone(),
            tx,
        )
        .expect("authority listener should bind");

        let mut stream = UnixStream::connect(&socket_path).expect("listener should accept");
        write_registration_frame(&mut stream, "peer-b").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("failure event should arrive"),
            AuthorityTransportEvent::Failed(
                "unexpected authority node `peer-b`; expected `peer-a`".to_string()
            )
        );
        assert!(!registry.has_connection("peer-a"));
        assert!(!registry.has_connection("peer-b"));
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn spawned_listener_accepts_authority_transport_connections() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let socket_path = test_socket_path("accept");
        let _guard = spawn_authority_listener(
            AuthorityConnectionRequest {
                socket_path: socket_path.clone(),
                authority_id: "peer-a".to_string(),
            },
            registry.clone(),
            tx,
        )
        .expect("authority listener should bind");

        let mut stream = UnixStream::connect(&socket_path).expect("listener should accept");
        write_registration_frame(&mut stream, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn runtime_with_local_socket_source_starts_listener_through_source_boundary() {
        let runtime = RemoteAuthorityConnectionRuntime::with_local_socket_source();
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let socket_path = test_socket_path("runtime-local-source");
        let _guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: socket_path.clone(),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("runtime should start local socket source");

        let mut stream = UnixStream::connect(&socket_path).expect("listener should accept");
        write_registration_frame(&mut stream, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn queued_stream_source_accepts_injected_authority_streams() {
        let (source, sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("queued-unused"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued source should start");

        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        sink.submit(server)
            .expect("queued source should accept injected stream");
        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        write_control_plane_envelope(&mut client, &authority_target_output_envelope(7))
            .expect("target output should encode");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 7);
                    assert_eq!(payload.output_bytes, b"a".to_vec());
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn queued_stream_source_reports_failed_registration_from_injected_stream() {
        let (source, sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("queued-failed-unused"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued source should start");

        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        sink.submit(server)
            .expect("queued source should accept injected stream");
        write_registration_frame(&mut client, "peer-b").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("failed event should arrive"),
            AuthorityTransportEvent::Failed(
                "unexpected authority node `peer-b`; expected `peer-a`".to_string()
            )
        );
        assert!(!registry.has_connection("peer-a"));
        assert!(!registry.has_connection("peer-b"));
    }

    #[test]
    fn local_socket_bridge_starter_feeds_listener_streams_through_queued_source() {
        let starter = LocalAuthoritySocketBridgeStarter;
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let socket_path = test_socket_path("bridge-starter");
        let _guard = starter
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: socket_path.clone(),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("bridge starter should start");

        let mut stream = UnixStream::connect(&socket_path).expect("bridge listener should accept");
        write_registration_frame(&mut stream, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        write_control_plane_envelope(&mut stream, &authority_target_output_envelope(9))
            .expect("target output should encode");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 9);
                    assert_eq!(payload.output_bytes, b"a".to_vec());
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn queued_stream_starter_exposes_external_producer_boundary() {
        let (starter, sink) = QueuedAuthorityStreamStarter::channel();
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = starter
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("queued-starter-unused"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued stream starter should start");

        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        sink.submit(server)
            .expect("queued stream starter should accept injected stream");
        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        write_control_plane_envelope(&mut client, &authority_target_output_envelope(13))
            .expect("target output should encode");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 13);
                    assert_eq!(payload.output_bytes, b"a".to_vec());
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
    }

    fn authority_target_output_envelope(output_seq: u64) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("msg-{output_seq}"),
            message_type: "target_output",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq,
                stream: "pty",
                output_bytes: b"a".to_vec(),
            }),
        }
    }

    fn raw_pty_input_payload() -> RawPtyInputPayload {
        RawPtyInputPayload {
            attachment_id: "attach-1".to_string(),
            session_id: "shell-1".to_string(),
            target_id: "remote-peer:peer-a:shell-1".to_string(),
            console_id: "console-a".to_string(),
            console_host_id: "observer-a".to_string(),
            input_seq: 1,
            input_bytes: b"a".to_vec(),
        }
    }

    fn test_socket_path(name: &str) -> std::path::PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-authority-connection-{name}-{}-{millis}.sock",
            process::id()
        ))
    }

    #[test]
    #[ignore]
    fn benchmark_authority_transport_rtt() {
        // Run with: cargo test benchmark_authority_transport_rtt -- --nocapture --ignored
        const ITERATIONS: usize = 10_000;

        // --- Codec overhead (encode + decode) ---
        let envelope = authority_target_output_envelope(1);
        let mut buf = Vec::with_capacity(4096);
        let mut decode_cursor = std::io::Cursor::new(Vec::new());

        let codec_start = std::time::Instant::now();
        for i in 0..ITERATIONS {
            let mut env = envelope.clone();
            if let ControlPlanePayload::TargetOutput(ref mut p) = env.payload {
                p.output_seq = i as u64;
            }
            buf.clear();
            write_control_plane_envelope(&mut buf, &env).expect("encode");
            decode_cursor.get_mut().clear();
            decode_cursor.get_mut().extend_from_slice(&buf);
            decode_cursor.set_position(0);
            let _decoded = read_control_plane_envelope(&mut decode_cursor).expect("decode");
        }
        let codec_ns = codec_start.elapsed().as_nanos() as f64 / ITERATIONS as f64;

        // --- Authority transport RTT (Unix socket) ---
        let registry = RemoteConnectionRegistry::new();
        let (event_tx, event_rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        write_registration_frame(&mut client, "peer-a").expect("ok");
        register_authority_stream(server, registry.clone(), "peer-a".to_string(), event_tx)
            .expect("ok");
        event_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("connected");

        let rtt_start = std::time::Instant::now();
        for i in 0..ITERATIONS {
            let env = authority_target_output_envelope(i as u64);
            write_control_plane_envelope(&mut client, &env).expect("encode");
            match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(AuthorityTransportEvent::Envelope(_)) => {}
                other => panic!("{other:?}"),
            }
        }
        let rtt_ns = rtt_start.elapsed().as_nanos() as f64 / ITERATIONS as f64;

        // --- Throughput (send batched then recv batched) ---
        let env = authority_target_output_envelope(1);
        let send_start = std::time::Instant::now();
        for i in 0..ITERATIONS {
            let mut e = env.clone();
            if let ControlPlanePayload::TargetOutput(ref mut p) = e.payload {
                p.output_seq = i as u64;
            }
            write_control_plane_envelope(&mut client, &e).expect("encode");
        }
        let send_ns = send_start.elapsed().as_nanos() as f64 / ITERATIONS as f64;

        let recv_start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(AuthorityTransportEvent::Envelope(_)) => {}
                other => panic!("{other:?}"),
            }
        }
        let recv_ns = recv_start.elapsed().as_nanos() as f64 / ITERATIONS as f64;

        println!("=== Authority Transport Benchmark ({ITERATIONS} iters) ===");
        println!(
            "Codec encode+decode:       {:.0} ns  ({:.3} µs)",
            codec_ns,
            codec_ns / 1000.0
        );
        println!(
            "Send-only (encode+write):  {:.0} ns  ({:.3} µs)",
            send_ns,
            send_ns / 1000.0
        );
        println!(
            "Receive-only (read+decode): {:.0} ns  ({:.3} µs)",
            recv_ns,
            recv_ns / 1000.0
        );
        println!(
            "Authority transport RTT:    {:.0} ns  ({:.3} µs)",
            rtt_ns,
            rtt_ns / 1000.0
        );
        println!("========================================");
        println!("For reference:");
        println!("  Tmux send-keys subprocess:  ~3 ms   (3000 µs)");
        println!("  Direct pty write:           ~1.5 µs");
        println!("  Network RTT (this link):    avg ~90 ms");
    }
}
