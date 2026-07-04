use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, OutboundNodeSessionRequest, RemoteNodeTransport,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_connection_runtime::QueuedAuthorityStreamSink;
use crate::runtime::remote_authority_transport_runtime::{
    authority_transport_socket_path, spawn_authority_transport_listener,
    AuthorityTransportListenerGuard,
};
use crate::runtime::remote_main_slot_pane_runtime::RemoteMainSlotPaneRuntime;
use crate::runtime::remote_node::remote_node_ingress_server_runtime::notify_authority_socket_ready;
use crate::runtime::remote_node_ingress_runtime::run_grpc_node_ingress_worker;
use crate::runtime::remote_node_session_runtime::{
    RemoteNodePublicationSink, RemoteNodeSessionError,
};
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
#[cfg(test)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// Remote main-slot authority ingress belongs to the pane process lifecycle.
// It accepts authority-side transport connections on the scoped socket path
// and hands the accepted streams into the in-process pane runtime.
pub struct RemoteMainSlotIngressRuntime {
    pane_runtime: RemoteMainSlotPaneRuntime,
    publication_runtime: RemoteTargetPublicationRuntime,
    network: RemoteNetworkConfig,
}

impl RemoteMainSlotIngressRuntime {
    pub(crate) fn new(
        pane_runtime: RemoteMainSlotPaneRuntime,
        publication_runtime: RemoteTargetPublicationRuntime,
        network: RemoteNetworkConfig,
    ) -> Self {
        Self {
            pane_runtime,
            publication_runtime,
            network,
        }
    }

    #[cfg(test)]
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Self::from_build_env_with_network(RemoteNetworkConfig::default())
    }

    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self::new(
            RemoteMainSlotPaneRuntime::from_build_env_with_external_authority_streams_and_network(
                network.clone(),
            )?,
            RemoteTargetPublicationRuntime::from_build_env_with_network(network.clone())?,
            network,
        ))
    }

    #[cfg(test)]
    pub fn submit_external_authority_stream(
        &self,
        stream: UnixStream,
    ) -> Result<(), LifecycleError> {
        self.pane_runtime.submit_external_authority_stream(stream)
    }

    pub fn run(&self, command: RemoteMainSlotCommand) -> Result<(), LifecycleError> {
        let t_run = std::time::Instant::now();
        ERROR_LOG.log(format!(
            "[diag-timing] RemoteMainSlotIngressRuntime::run start target={}",
            command.target
        ));
        let authority_sink = self.pane_runtime.external_authority_stream_submitter()?;
        // In client mode (`--connect`), this pane owns a direct outbound gRPC
        // bridge. In server/listener mode, it exposes the same scoped authority
        // transport socket that the global node-ingress owner discovers via
        // inotify and bridges into the already-open remote node session. Both
        // paths feed accepted streams into the pane-owned queue.
        let _authority_ingress = if self.network.connect_endpoint_uri().is_some() {
            AuthorityIngressGuard::grpc(self.spawn_background_grpc_bridge(
                extract_authority_id_from_target(&command.target),
                authority_sink,
                command.socket_name.clone(),
            )?)
        } else {
            AuthorityIngressGuard::local(
                self.start_local_authority_transport_listener(&command, authority_sink)?,
            )
        };
        ERROR_LOG.log(format!(
            "[diag-timing] grpc bridge spawned, entering pane_runtime.run ({:?})",
            t_run.elapsed()
        ));
        self.pane_runtime.run(command)
    }

    /// Spawns a background thread that connects to the remote authority via
    /// gRPC and bridges all session events (mirror bootstrap, target output,
    /// etc.) into the authority stream queue. Returns immediately so the
    /// caller can start the pane UI without delay.
    ///
    /// The thread reconnects automatically when the gRPC stream closes or the
    /// initial connection fails, using exponential backoff (1s → 30s max).
    /// Each successful connection submits a fresh authority stream to the
    /// sink, which triggers a new `Connected` event in the pane event loop
    /// so the reconnecting phase can recover without user intervention.
    fn start_local_authority_transport_listener(
        &self,
        command: &RemoteMainSlotCommand,
        authority_sink: QueuedAuthorityStreamSink,
    ) -> Result<AuthorityTransportListenerGuard, LifecycleError> {
        let socket_path = authority_transport_socket_path(
            &command.socket_name,
            &command.session_name,
            &command.target,
        );
        let guard = spawn_authority_transport_listener(socket_path.clone(), authority_sink)
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to start remote main-slot authority transport listener".to_string(),
                    error,
                )
            })?;
        let authority_id = extract_authority_id_from_target(&command.target);
        notify_authority_socket_ready(&self.network, &authority_id, &socket_path).map_err(
            |error| {
                LifecycleError::Io(
                    "failed to register remote main-slot authority socket".to_string(),
                    error,
                )
            },
        )?;
        Ok(guard)
    }

    fn spawn_background_grpc_bridge(
        &self,
        authority_id: String,
        authority_sink: QueuedAuthorityStreamSink,
        socket_name: String,
    ) -> Result<Option<GrpcAuthorityBridgeGuard>, LifecycleError> {
        let Some(endpoint_uri) = self.network.connect_endpoint_uri() else {
            return Ok(None);
        };
        if authority_id.is_empty() {
            return Ok(None);
        }

        let publication_sink = Arc::new(LiveRemotePublicationSink {
            runtime: self.publication_runtime.clone(),
            socket_name,
        });

        thread::Builder::new()
            .name("grpc-bridge".into())
            .spawn(move || {
                let transport = GrpcRemoteNodeTransport::new();
                let mut reconnect_delay = Duration::from_secs(1);
                const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
                loop {
                    let (event_tx, event_rx) = std::sync::mpsc::channel();
                    let t_bridge = std::time::Instant::now();
                    ERROR_LOG.log(format!(
                        "[diag-timing] grpc-bridge thread: starting connect_outbound to {}",
                        endpoint_uri
                    ));

                    match transport.connect_outbound(
                        OutboundNodeSessionRequest {
                            node_id: authority_id.clone(),
                            endpoint_uri: endpoint_uri.clone(),
                        },
                        event_tx,
                    ) {
                        Ok(guard) => {
                            ERROR_LOG.log(format!(
                                "[diag-timing] grpc-bridge: connect_outbound OK ({:?}), starting ingress worker",
                                t_bridge.elapsed()
                            ));
                            reconnect_delay = Duration::from_secs(1);
                            let t_worker = std::time::Instant::now();
                            run_grpc_node_ingress_worker(
                                event_rx,
                                authority_sink.clone(),
                                publication_sink.clone(),
                            );
                            ERROR_LOG.log(format!(
                                "[diag-timing] grpc-bridge: ingress worker exited (worker={:?}, total={:?}), reconnecting in {:?}",
                                t_worker.elapsed(),
                                t_bridge.elapsed(),
                                reconnect_delay
                            ));
                            drop(guard);
                        }
                        Err(error) => {
                            ERROR_LOG.log(format!(
                                "[diag-timing] grpc-bridge: connect_outbound FAILED after {:?}: {}, reconnecting in {:?}",
                                t_bridge.elapsed(),
                                error,
                                reconnect_delay
                            ));
                        }
                    }
                    std::thread::sleep(reconnect_delay);
                    reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                }
            })
            .map_err(|error| {
                LifecycleError::Io("failed to spawn gRPC bridge thread".to_string(), error)
            })?;

        Ok(Some(GrpcAuthorityBridgeGuard))
    }
}

struct LiveRemotePublicationSink {
    runtime: RemoteTargetPublicationRuntime,
    socket_name: String,
}

impl RemoteNodePublicationSink for LiveRemotePublicationSink {
    fn publish(
        &self,
        envelope: crate::infra::remote_protocol::ProtocolEnvelope<
            crate::infra::remote_protocol::ControlPlanePayload,
        >,
    ) -> Result<(), RemoteNodeSessionError> {
        self.runtime
            .apply_live_publication_envelope(&self.socket_name, envelope)
            .map_err(|error| RemoteNodeSessionError::new(error.to_string()))
    }
}

struct AuthorityIngressGuard {
    _grpc: Option<GrpcAuthorityBridgeGuard>,
    _local: Option<AuthorityTransportListenerGuard>,
}

impl AuthorityIngressGuard {
    fn grpc(guard: Option<GrpcAuthorityBridgeGuard>) -> Self {
        Self {
            _grpc: guard,
            _local: None,
        }
    }

    fn local(guard: AuthorityTransportListenerGuard) -> Self {
        Self {
            _grpc: None,
            _local: Some(guard),
        }
    }
}

pub(crate) struct GrpcAuthorityBridgeGuard;

fn extract_authority_id_from_target(target: &str) -> String {
    let target = target.strip_prefix("remote-peer:").unwrap_or(target);
    target
        .rsplit_once(':')
        .map(|(authority_id, _)| authority_id.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{LiveRemotePublicationSink, RemoteMainSlotIngressRuntime};
    use crate::application::target_registry_service::{
        DefaultTargetCatalogGateway, TargetRegistryService,
    };
    use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
    use crate::infra::remote_grpc_proto::v1::{
        ClientHello, MirrorBootstrapChunk, MirrorBootstrapComplete, NodeSessionEnvelope,
        ProtocolVersion, RouteContext, TargetOutput,
    };
    use crate::infra::remote_protocol::RemoteConsoleDescriptor;
    use crate::infra::tmux::EmbeddedTmuxBackend;
    use crate::runtime::remote_authority_connection_runtime::{
        AuthorityConnectionRequest, AuthorityTransportEvent,
    };
    use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
    use crate::runtime::remote_main_slot_pane_runtime::apply_authority_envelope;
    use crate::runtime::remote_main_slot_pane_runtime::RemoteMainSlotPaneRuntime;
    use crate::runtime::remote_main_slot_runtime::RemoteMainSlotRuntime;
    use crate::runtime::remote_node_ingress_runtime::RemoteNodeIngressRuntime;
    use crate::runtime::remote_observer_runtime::RemoteObserverRuntime;
    use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
    use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
    use std::net::{SocketAddr, TcpListener};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::runtime::Builder;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;

    #[test]
    fn connected_client_ingress_runtime_owns_external_authority_stream_submission() {
        let runtime =
            RemoteMainSlotIngressRuntime::from_build_env_with_network(RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            })
            .expect("ingress runtime should build from build env");
        let (_client, server) = UnixStream::pair().expect("stream pair should open");

        runtime
            .submit_external_authority_stream(server)
            .expect("ingress runtime should accept submitted authority stream");
    }

    #[test]
    fn ingress_runtime_uses_scoped_authority_transport_socket_path() {
        let socket_path = authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1");

        let rendered = socket_path.to_string_lossy();
        assert!(rendered.contains("waitagent-remote-"));
        assert!(rendered.ends_with(".sock"));
        assert!(rendered.len() < 108);
    }

    #[test]
    fn server_listener_mode_keeps_queued_stream_for_local_authority_transport_listener() {
        let runtime = RemoteMainSlotIngressRuntime::from_build_env()
            .expect("ingress runtime should build from build env");

        assert!(runtime
            .pane_runtime
            .external_authority_stream_submitter()
            .is_ok());
    }

    #[test]
    fn connected_client_mode_uses_queued_authority_stream_source() {
        let runtime =
            RemoteMainSlotIngressRuntime::from_build_env_with_network(RemoteNetworkConfig {
                port: 7474,
                connect: Some("127.0.0.1:7474".to_string()),
                node_id: None,
                public_endpoint: None,
            })
            .expect("ingress runtime should build from build env");

        assert!(runtime
            .pane_runtime
            .external_authority_stream_submitter()
            .is_ok());
    }

    #[test]
    fn server_listener_authority_transport_listener_requires_owner_registration() {
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let pane_runtime = RemoteMainSlotPaneRuntime::new_with_external_authority_streams(
            target_registry,
            EmbeddedTmuxBackend::from_build_env().expect("tmux backend should build"),
            PathBuf::from("/tmp/waitagent"),
        );
        let runtime = RemoteMainSlotIngressRuntime::new(
            pane_runtime,
            RemoteTargetPublicationRuntime::from_build_env()
                .expect("publication runtime should build from env"),
            RemoteNetworkConfig::default(),
        );
        let command = RemoteMainSlotCommand {
            socket_name: unique_socket_name("server-main-slot-authority"),
            session_name: "workspace-1".to_string(),
            target: "remote-peer:peer-a:shell-1".to_string(),
        };
        let sink = runtime
            .pane_runtime
            .external_authority_stream_submitter()
            .expect("pane queue should exist");
        let (tx, rx) = mpsc::channel();
        let _connection_guard = runtime
            .pane_runtime
            .start_authority_connection(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-server-main-slot.sock"),
                    authority_id: "peer-a".to_string(),
                },
                RemoteConnectionRegistry::new(),
                tx,
            )
            .expect("pane runtime should start queued authority connection");
        let error = match runtime.start_local_authority_transport_listener(&command, sink) {
            Ok(_) => panic!("server listener should fail without owner registration ACK"),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains("failed to register remote main-slot authority socket"));
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn grpc_ingress_bridges_authority_output_into_visible_observer_render_path() {
        let bind_addr = unused_local_addr();
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let pane_runtime = RemoteMainSlotPaneRuntime::new_with_external_authority_streams(
            target_registry,
            EmbeddedTmuxBackend::from_build_env().expect("tmux backend should build"),
            PathBuf::from("/tmp/waitagent"),
        );
        let runtime = RemoteMainSlotIngressRuntime::new(
            pane_runtime,
            RemoteTargetPublicationRuntime::from_build_env()
                .expect("publication runtime should build from env"),
            RemoteNetworkConfig::default(),
        );
        let target = remote_target();
        let remote_runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = remote_runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        remote_runtime.ensure_local_connection("peer-a");
        remote_runtime
            .activate_target(
                &target,
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");

        let (tx, rx) = mpsc::channel();
        let _connection_guard = runtime
            .pane_runtime
            .start_authority_connection(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-main-slot-ingress.sock"),
                    authority_id: "peer-a".to_string(),
                },
                RemoteConnectionRegistry::new(),
                tx,
            )
            .expect("pane runtime should start queued authority connection");
        let _ingress_guard = RemoteNodeIngressRuntime::with_grpc_source(bind_addr)
            .start_with_source(
                authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1"),
                runtime
                    .pane_runtime
                    .external_authority_stream_submitter()
                    .expect("authority submitter should exist"),
                Arc::new(LiveRemotePublicationSink {
                    runtime: runtime.publication_runtime.clone(),
                    socket_name: "wa-1".to_string(),
                }),
            )
            .expect("grpc ingress should start");

        let async_runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");
        async_runtime.block_on(async {
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
                match rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("connected event should arrive")
                {
                    AuthorityTransportEvent::Connected { authority_id, .. } => authority_id,
                    other => panic!("unexpected authority event: {other:?}"),
                },
                "peer-a"
            );

            tx.send(target_output_envelope())
                .await
                .expect("target output should send");
        });

        let authority_event = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority target output should arrive");
        let AuthorityTransportEvent::Envelope { envelope, .. } = authority_event else {
            panic!("unexpected authority event: {authority_event:?}");
        };
        apply_authority_envelope(&remote_runtime, &target, &envelope)
            .expect("authority envelope should apply through main-slot runtime");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(7));
        assert_eq!(snapshot.active_screen().lines[0], "a           ");
    }

    #[test]
    fn grpc_ingress_bridges_authority_bootstrap_into_visible_observer_render_path() {
        let bind_addr = unused_local_addr();
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let pane_runtime = RemoteMainSlotPaneRuntime::new_with_external_authority_streams(
            target_registry,
            EmbeddedTmuxBackend::from_build_env().expect("tmux backend should build"),
            PathBuf::from("/tmp/waitagent"),
        );
        let runtime = RemoteMainSlotIngressRuntime::new(
            pane_runtime,
            RemoteTargetPublicationRuntime::from_build_env()
                .expect("publication runtime should build from env"),
            RemoteNetworkConfig::default(),
        );
        let target = remote_target();
        let remote_runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = remote_runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        remote_runtime.ensure_local_connection("peer-a");
        remote_runtime
            .activate_target(
                &target,
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");

        let (tx, rx) = mpsc::channel();
        let _connection_guard = runtime
            .pane_runtime
            .start_authority_connection(
                AuthorityConnectionRequest {
                    socket_path: std::env::temp_dir().join("unused-main-slot-ingress.sock"),
                    authority_id: "peer-a".to_string(),
                },
                RemoteConnectionRegistry::new(),
                tx,
            )
            .expect("pane runtime should start queued authority connection");
        let _ingress_guard = RemoteNodeIngressRuntime::with_grpc_source(bind_addr)
            .start_with_source(
                authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1"),
                runtime
                    .pane_runtime
                    .external_authority_stream_submitter()
                    .expect("authority submitter should exist"),
                Arc::new(LiveRemotePublicationSink {
                    runtime: runtime.publication_runtime.clone(),
                    socket_name: "wa-1".to_string(),
                }),
            )
            .expect("grpc ingress should start");

        let async_runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");
        async_runtime.block_on(async {
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
                match rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("connected event should arrive")
                {
                    AuthorityTransportEvent::Connected { authority_id, .. } => authority_id,
                    other => panic!("unexpected authority event: {other:?}"),
                },
                "peer-a"
            );

            tx.send(mirror_bootstrap_chunk_envelope())
                .await
                .expect("mirror bootstrap chunk should send");
            tx.send(mirror_bootstrap_complete_envelope())
                .await
                .expect("mirror bootstrap complete should send");
        });

        let authority_event = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority bootstrap chunk should arrive");
        let AuthorityTransportEvent::Envelope { envelope, .. } = authority_event else {
            panic!("unexpected authority event: {authority_event:?}");
        };
        apply_authority_envelope(&remote_runtime, &target, &envelope)
            .expect("authority bootstrap chunk should apply through main-slot runtime");

        let authority_event = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority bootstrap complete should arrive");
        let AuthorityTransportEvent::Envelope { envelope, .. } = authority_event else {
            panic!("unexpected authority event: {authority_event:?}");
        };
        apply_authority_envelope(&remote_runtime, &target, &envelope)
            .expect("authority bootstrap complete should apply through main-slot runtime");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert!(snapshot.has_visible_output);
        assert!(snapshot.bootstrap_complete);
        assert_eq!(snapshot.active_screen().lines[0], "bootstrap   ");
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
                output_bytes: b"bootstrap".to_vec(),
                chunk_seq: 1,
                stream: "pty".to_string(),
                session_id: "shell-1".to_string(),
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
                last_chunk_seq: 1,
                session_id: "shell-1".to_string(),
                alternate_screen_active: false,
                application_cursor_keys: false,
                cursor_visible: true,
            })),
        }
    }

    fn unique_socket_name(prefix: &str) -> String {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("waitagent-test-{prefix}-{}-{millis}", std::process::id())
    }

    fn unused_local_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral listener should bind");
        let addr = listener
            .local_addr()
            .expect("ephemeral listener should report local addr");
        drop(listener);
        addr
    }

    fn remote_target() -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("peer-a", "shell-1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
