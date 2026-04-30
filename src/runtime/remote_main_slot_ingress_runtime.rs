use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_main_slot_pane_runtime::RemoteMainSlotPaneRuntime;
use crate::runtime::remote_node_ingress_runtime::{
    LocalSocketRemoteNodeIngressSource, RemoteNodeIngressGuard, RemoteNodeIngressRuntime,
    RemoteNodeIngressStarter,
};
use crate::runtime::remote_node_session_runtime::{
    RemoteNodePublicationSink, RemoteNodeSessionError,
};
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

// Remote main-slot authority ingress belongs to the pane process lifecycle.
// It accepts authority-side transport connections on the scoped socket path
// and hands the accepted streams into the in-process pane runtime.
pub struct RemoteMainSlotIngressRuntime {
    pane_runtime: RemoteMainSlotPaneRuntime,
    publication_runtime: RemoteTargetPublicationRuntime,
    node_ingress: Box<dyn RemoteNodeIngressStarter>,
}

impl RemoteMainSlotIngressRuntime {
    pub(crate) fn new(
        pane_runtime: RemoteMainSlotPaneRuntime,
        publication_runtime: RemoteTargetPublicationRuntime,
        node_ingress: Box<dyn RemoteNodeIngressStarter>,
    ) -> Self {
        Self {
            pane_runtime,
            publication_runtime,
            node_ingress,
        }
    }

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
            Box::new(RemoteNodeIngressRuntime::new(
                LocalSocketRemoteNodeIngressSource,
            )),
        ))
    }

    pub fn submit_external_authority_stream(
        &self,
        stream: UnixStream,
    ) -> Result<(), LifecycleError> {
        self.pane_runtime.submit_external_authority_stream(stream)
    }

    pub(crate) fn start_ingress_for_command(
        &self,
        command: &RemoteMainSlotCommand,
    ) -> Result<Box<dyn RemoteNodeIngressGuard>, LifecycleError> {
        let socket_path = authority_transport_socket_path(
            &command.socket_name,
            &command.session_name,
            &command.target,
        );
        let submitter = self.pane_runtime.external_authority_stream_submitter()?;
        let publication_sink: Arc<dyn RemoteNodePublicationSink> =
            Arc::new(LiveRemotePublicationSink {
                runtime: self.publication_runtime.clone(),
                socket_name: command.socket_name.clone(),
            });
        let _authority_ingress = self
            .node_ingress
            .start_ingress(socket_path, submitter, publication_sink)
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to start remote main-slot authority ingress".to_string(),
                    error,
                )
            })?;
        Ok(_authority_ingress)
    }

    pub fn run(&self, command: RemoteMainSlotCommand) -> Result<(), LifecycleError> {
        let _authority_ingress = self.start_ingress_for_command(&command)?;
        self.pane_runtime.run(command)
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

#[cfg(test)]
mod tests {
    use super::RemoteMainSlotIngressRuntime;
    use crate::application::target_registry_service::{
        DefaultTargetCatalogGateway, TargetRegistryService,
    };
    use crate::cli::RemoteMainSlotCommand;
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
    use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
    use crate::infra::remote_grpc_proto::v1::{
        ClientHello, NodeSessionEnvelope, ProtocolVersion, RouteContext, TargetOutput,
    };
    use crate::infra::remote_protocol::RemoteConsoleDescriptor;
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
    use std::time::Duration;
    use tokio::runtime::Builder;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;

    #[test]
    fn ingress_runtime_owns_external_authority_stream_submission() {
        let runtime = RemoteMainSlotIngressRuntime::from_build_env()
            .expect("ingress runtime should build from build env");
        let (_client, server) = UnixStream::pair().expect("stream pair should open");

        runtime
            .submit_external_authority_stream(server)
            .expect("ingress runtime should accept submitted authority stream");
    }

    #[test]
    fn ingress_runtime_uses_scoped_authority_transport_socket_path() {
        let socket_path = authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1");

        assert!(socket_path
            .to_string_lossy()
            .contains("waitagent-remote-wa-1-workspace-1-peer-a_shell-1.sock"));
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
            PathBuf::from("/tmp/waitagent"),
        );
        let runtime = RemoteMainSlotIngressRuntime::new(
            pane_runtime,
            RemoteTargetPublicationRuntime::from_build_env()
                .expect("publication runtime should build from env"),
            Box::new(RemoteNodeIngressRuntime::with_grpc_source(bind_addr)),
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
        let _ingress_guard = runtime
            .start_ingress_for_command(&RemoteMainSlotCommand {
                socket_name: "wa-1".to_string(),
                session_name: "workspace-1".to_string(),
                target: "peer-a:shell-1".to_string(),
            })
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
                rx.recv_timeout(Duration::from_secs(1))
                    .expect("connected event should arrive"),
                AuthorityTransportEvent::Connected
            );

            tx.send(target_output_envelope())
                .await
                .expect("target output should send");
        });

        let authority_event = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority target output should arrive");
        let AuthorityTransportEvent::Envelope(envelope) = authority_event else {
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
