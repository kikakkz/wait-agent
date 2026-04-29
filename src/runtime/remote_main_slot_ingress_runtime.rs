use crate::cli::RemoteMainSlotCommand;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_main_slot_pane_runtime::RemoteMainSlotPaneRuntime;
use crate::runtime::remote_node_session_runtime::{
    spawn_remote_node_session_listener, RemoteNodePublicationSink, RemoteNodeSessionError,
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
}

impl RemoteMainSlotIngressRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Ok(Self {
            pane_runtime:
                RemoteMainSlotPaneRuntime::from_build_env_with_external_authority_streams()?,
            publication_runtime: RemoteTargetPublicationRuntime::from_build_env()?,
        })
    }

    pub fn submit_external_authority_stream(
        &self,
        stream: UnixStream,
    ) -> Result<(), LifecycleError> {
        self.pane_runtime.submit_external_authority_stream(stream)
    }

    pub fn run(&self, command: RemoteMainSlotCommand) -> Result<(), LifecycleError> {
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
        let _authority_ingress =
            spawn_remote_node_session_listener(socket_path, submitter, publication_sink).map_err(
                |error| {
                    LifecycleError::Io(
                        "failed to start remote main-slot authority ingress".to_string(),
                        error,
                    )
                },
            )?;
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
    use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
    use std::os::unix::net::UnixStream;

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
}
