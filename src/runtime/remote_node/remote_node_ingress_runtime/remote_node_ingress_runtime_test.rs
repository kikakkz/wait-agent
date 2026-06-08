mod tests {
    use super::super::{
        AuthoritySocketRemoteNodeIngressSource, GrpcRemoteNodeIngressSource,
        RemoteNodeIngressSource, RouteContext,
    };
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
    use crate::infra::remote_grpc_proto::v1::{
        ClientHello, MirrorBootstrapChunk, MirrorBootstrapComplete, NodeSessionEnvelope,
        ProtocolVersion, TargetOutput,
    };
    use crate::infra::remote_protocol::{
        BootstrapMode, ControlPlanePayload, MirrorBootstrapChunkPayload,
        MirrorBootstrapCompletePayload, OpenMirrorRequestPayload, ProtocolEnvelope,
        RawPtyInputPayload, REMOTE_PROTOCOL_VERSION,
    };
    use crate::runtime::remote_authority_connection_runtime::{
        AuthorityConnectionRequest, AuthorityTransportEvent, QueuedAuthorityStreamSource,
        RemoteAuthorityConnectionRuntime,
    };
    use crate::runtime::remote_authority_transport_runtime::{
        RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
    };
    use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneSink;
    use crate::runtime::remote_node_session_runtime::RemoteNodePublicationSink;
    use crate::runtime::remote_transport_runtime::{
        RegistryRemoteControlPlaneSink, RemoteConnectionRegistry,
    };
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;
    use tokio::runtime::Builder;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;

    #[test]
    fn grpc_source_handles_host_port_authority_in_target_ids() {
        let event = super::super::map_target_exited_envelope(
            "10.1.26.84:7474",
            &NodeSessionEnvelope {
                message_id: "msg-1".to_string(),
                sent_at: None,
                session_instance_id: "client-session-1".to_string(),
                correlation_id: None,
                route: Some(RouteContext {
                    authority_node_id: Some("10.1.26.84:7474".to_string()),
                    target_id: Some("remote-peer:10.1.26.84:7474:6a1b816eb1111435".to_string()),
                    attachment_id: None,
                    console_id: None,
                    console_host_id: None,
                    session_id: None,
                }),
                body: Some(Body::TargetExited(
                    crate::infra::remote_grpc_proto::v1::TargetExited {
                        target_id: "remote-peer:10.1.26.84:7474:6a1b816eb1111435".to_string(),
                        transport_session_id: "6a1b816eb1111435".to_string(),
                    },
                )),
            },
            &crate::infra::remote_grpc_proto::v1::TargetExited {
                target_id: "remote-peer:10.1.26.84:7474:6a1b816eb1111435".to_string(),
                transport_session_id: "6a1b816eb1111435".to_string(),
            },
        );

        assert_eq!(event.session_id.as_deref(), Some("6a1b816eb1111435"));
        assert_eq!(
            event.target_id.as_deref(),
            Some("remote-peer:10.1.26.84:7474:6a1b816eb1111435")
        );
    }

    #[test]
    fn grpc_source_bridges_authority_output_and_publication_into_existing_runtime_boundaries() {
        let bind_addr = unused_local_addr();
        let source = GrpcRemoteNodeIngressSource::new(bind_addr);
        let (authority_source, authority_sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(authority_source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _connection_guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-grpc-node-ingress.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued authority runtime should start");
        let publication_sink = Arc::new(RecordingPublicationSink::default());
        let _guard = source
            .start(
                std::env::temp_dir().join("unused-grpc-node-session.sock"),
                authority_sink,
                publication_sink.clone(),
            )
            .expect("grpc ingress source should start");

        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");
        runtime.block_on(async {
            let mut client = NodeSessionServiceClient::connect(format!("http://{bind_addr}"))
                .await
                .expect("grpc client should connect");
            let (tx, rx_stream) = tokio_mpsc::channel(8);
            tx.send(client_hello_envelope("peer-a"))
                .await
                .expect("client hello should send");
            let response = client
                .open_node_session(Request::new(ReceiverStream::new(rx_stream)))
                .await
                .expect("node session should open");
            let mut inbound = response.into_inner();
            let _server_hello = inbound
                .message()
                .await
                .expect("server hello should decode")
                .expect("server hello should exist");

            assert_eq!(
                rx.recv_timeout(Duration::from_secs(1))
                    .expect("connected event should arrive"),
                AuthorityTransportEvent::Connected
            );
            assert!(registry.has_connection("peer-a"));

            tx.send(target_output_envelope())
                .await
                .expect("target output should send");
            let inbound_event = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("authority envelope event should arrive");
            match inbound_event {
                AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                    ControlPlanePayload::TargetOutput(payload) => {
                        assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                        assert_eq!(payload.output_seq, 7);
                        assert_eq!(payload.output_bytes, b"a");
                    }
                    other => panic!("unexpected authority envelope payload: {other:?}"),
                },
                other => panic!("unexpected authority transport event: {other:?}"),
            }

            tx.send(mirror_bootstrap_chunk_envelope())
                .await
                .expect("mirror bootstrap chunk should send");
            let inbound_event = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("mirror bootstrap chunk should arrive");
            match inbound_event {
                AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                    ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
                        target_id,
                        chunk_seq,
                        ref output_bytes,
                        ..
                    }) => {
                        assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                        assert_eq!(chunk_seq, 1);
                        assert_eq!(output_bytes, b"bootstrap");
                    }
                    other => panic!("unexpected authority envelope payload: {other:?}"),
                },
                other => panic!("unexpected authority transport event: {other:?}"),
            }

            tx.send(mirror_bootstrap_complete_envelope())
                .await
                .expect("mirror bootstrap complete should send");
            let inbound_event = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("mirror bootstrap complete should arrive");
            match inbound_event {
                AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                    ControlPlanePayload::MirrorBootstrapComplete(
                        MirrorBootstrapCompletePayload {
                            target_id,
                            last_chunk_seq,
                            ..
                        },
                    ) => {
                        assert_eq!(target_id, "remote-peer:peer-a:shell-1");
                        assert_eq!(last_chunk_seq, 1);
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
                            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                            message_id: "raw-pty-input-1".to_string(),
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
                .expect("raw PTY input should route through bridged grpc authority session");
            let outbound = inbound
                .message()
                .await
                .expect("outbound authority envelope should decode")
                .expect("outbound authority envelope should exist");
            match outbound.body.expect("body should exist") {
                Body::RawPtyInput(payload) => {
                    assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                    assert_eq!(payload.input_seq, 3);
                    assert_eq!(payload.input_bytes, b"b".to_vec());
                }
                other => panic!("unexpected grpc outbound body: {other:?}"),
            }
        });

        publication_sink.assert_empty();
    }

    #[test]
    fn authority_socket_source_accepts_authority_transport_clients() {
        let socket_path = std::env::temp_dir().join(format!(
            "waitagent-authority-source-test-{}-{}.sock",
            std::process::id(),
            unused_local_addr().port()
        ));
        let source = AuthoritySocketRemoteNodeIngressSource;
        let (authority_source, authority_sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(authority_source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _connection_guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-authority-source.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued authority runtime should start");
        let publication_sink = Arc::new(RecordingPublicationSink::default());
        let _guard = source
            .start(socket_path.clone(), authority_sink, publication_sink)
            .expect("authority socket ingress source should start");

        let transport = RemoteAuthorityTransportRuntime::connect(&socket_path, "peer-a")
            .expect("authority transport should connect");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        RegistryRemoteControlPlaneSink::new(registry.clone())
            .send(&[
                crate::infra::remote_protocol::NodeBoundControlPlaneMessage {
                    node_id: "peer-a".to_string(),
                    envelope: ProtocolEnvelope {
                        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                        message_id: "open-mirror-1".to_string(),
                        message_type: "open_mirror_request",
                        timestamp: "1Z".to_string(),
                        sender_id: "server".to_string(),
                        correlation_id: None,
                        session_id: Some("shell-1".to_string()),
                        target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                        attachment_id: None,
                        console_id: Some("console-1".to_string()),
                        payload: ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
                            session_id: "shell-1".to_string(),
                            target_id: "remote-peer:peer-a:shell-1".to_string(),
                            console_id: "console-1".to_string(),
                            cols: 120,
                            rows: 40,
                            raw_pty_passthrough: false,
                            bootstrap_mode: BootstrapMode::Full,
                        }),
                    },
                },
            ])
            .expect("open mirror request should route through authority transport");

        match transport
            .recv_command()
            .expect("authority command should arrive")
        {
            RemoteAuthorityCommand::OpenMirror(payload) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.console_id, "console-1");
                assert_eq!(payload.cols, 120);
                assert_eq!(payload.rows, 40);
            }
            other => panic!("unexpected authority command: {other:?}"),
        }

        std::fs::remove_file(socket_path).ok();
    }

    #[derive(Default)]
    struct RecordingPublicationSink {
        envelopes: Mutex<Vec<ProtocolEnvelope<ControlPlanePayload>>>,
    }

    impl RecordingPublicationSink {
        fn assert_empty(&self) {
            assert!(self
                .envelopes
                .lock()
                .expect("publication sink mutex should not be poisoned")
                .is_empty());
        }
    }

    impl RemoteNodePublicationSink for RecordingPublicationSink {
        fn publish(
            &self,
            envelope: ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<(), crate::runtime::remote_node_session_runtime::RemoteNodeSessionError>
        {
            self.envelopes
                .lock()
                .expect("publication sink mutex should not be poisoned")
                .push(envelope);
            Ok(())
        }
    }

    fn client_hello_envelope(node_id: &str) -> NodeSessionEnvelope {
        NodeSessionEnvelope {
            message_id: "client-hello-1".to_string(),
            sent_at: None,
            session_instance_id: "client-session-1".to_string(),
            correlation_id: None,
            route: None,
            body: Some(Body::ClientHello(ClientHello {
                node_id: node_id.to_string(),
                node_instance_id: "instance-a".to_string(),
                min_supported_version: Some(ProtocolVersion { major: 1, minor: 0 }),
                max_supported_version: Some(ProtocolVersion { major: 1, minor: 0 }),
                capabilities: None,
                resume: None,
            })),
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

    fn unused_local_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral listener should bind");
        let addr = listener
            .local_addr()
            .expect("ephemeral listener should report local addr");
        drop(listener);
        addr
    }
}
