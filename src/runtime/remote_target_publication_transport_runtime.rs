use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::runtime::remote_node_session_runtime::{
    RemoteNodeSessionError, RemoteNodeSessionRuntime,
};
use std::fmt;
use std::path::{Path, PathBuf};

pub struct RemoteTargetPublicationTransportRuntime {
    node_id: String,
    session: RemoteNodeSessionRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTargetPublicationTransportError {
    message: String,
}

impl RemoteTargetPublicationTransportRuntime {
    pub fn connect(
        socket_path: impl AsRef<Path>,
        node_id: impl Into<String>,
    ) -> Result<Self, RemoteTargetPublicationTransportError> {
        let node_id = node_id.into();
        let session = RemoteNodeSessionRuntime::connect(socket_path, node_id.clone(), None)?;
        Ok(Self { node_id, session })
    }

    pub fn send_target_published(
        &self,
        target: &ManagedSessionRecord,
        source_session_name: Option<&str>,
    ) -> Result<(), RemoteTargetPublicationTransportError> {
        validate_publication_target(target, &self.node_id)?;
        self.session
            .send_target_published(target, source_session_name)
            .map_err(RemoteTargetPublicationTransportError::from)
    }

    pub fn send_target_exited(
        &self,
        transport_session_id: &str,
        source_session_name: Option<&str>,
    ) -> Result<(), RemoteTargetPublicationTransportError> {
        self.session
            .send_target_exited(transport_session_id, source_session_name)
            .map_err(RemoteTargetPublicationTransportError::from)
    }
}

pub fn remote_target_publication_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-publication-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

fn validate_publication_target(
    target: &ManagedSessionRecord,
    authority_id: &str,
) -> Result<(), RemoteTargetPublicationTransportError> {
    if target.address.transport() != &SessionTransport::RemotePeer {
        return Err(RemoteTargetPublicationTransportError::new(format!(
            "published target `{}` is not a remote-peer target",
            target.address.id().as_str()
        )));
    }
    if target.address.authority_id() != authority_id {
        return Err(RemoteTargetPublicationTransportError::new(format!(
            "published target authority `{}` does not match transport node `{authority_id}`",
            target.address.authority_id()
        )));
    }
    Ok(())
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

impl RemoteTargetPublicationTransportError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteTargetPublicationTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteTargetPublicationTransportError {}

impl From<RemoteNodeSessionError> for RemoteTargetPublicationTransportError {
    fn from(value: RemoteNodeSessionError) -> Self {
        Self::new(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{remote_target_publication_socket_path, RemoteTargetPublicationTransportRuntime};
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::remote_protocol::{
        ClientHelloPayload, ControlPlanePayload, NodeSessionChannel, TargetExitedPayload,
        TargetPublishedPayload,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, read_node_session_envelope,
    };
    use crate::runtime::remote_node_transport_runtime::{
        write_server_hello, NODE_TRANSPORT_CLIENT_VERSION,
    };
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    #[test]
    fn publication_socket_path_is_scoped_to_socket_name() {
        let path = remote_target_publication_socket_path("wa/local");

        assert!(path
            .to_string_lossy()
            .contains("waitagent-remote-publication-wa_local.sock"));
    }

    #[test]
    fn connect_and_send_target_published_round_trip() {
        let socket_path = test_socket_path("published");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let accept_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            let node_id = match read_control_plane_envelope(&mut stream)
                .expect("client hello should decode")
                .payload
            {
                ControlPlanePayload::ClientHello(ClientHelloPayload {
                    node_id,
                    client_version,
                }) => {
                    assert_eq!(client_version, NODE_TRANSPORT_CLIENT_VERSION);
                    node_id
                }
                other => panic!("unexpected hello payload: {other:?}"),
            };
            write_server_hello(&mut stream, "waitagent-publication")
                .expect("server hello should encode");
            let envelope =
                read_node_session_envelope(&mut stream).expect("publication should decode");
            (node_id, envelope)
        });

        let transport = RemoteTargetPublicationTransportRuntime::connect(&socket_path, "peer-a")
            .expect("publication transport should connect");
        transport
            .send_target_published(&remote_target("peer-a", "shell-1"), Some("target-host-1"))
            .expect("published target should encode");

        let (node_id, envelope) = accept_thread
            .join()
            .expect("accept thread should join cleanly");
        assert_eq!(node_id, "peer-a");
        assert_eq!(envelope.channel, NodeSessionChannel::Publication);
        match envelope.envelope.payload {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
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
                assert_eq!(transport_session_id, "shell-1");
                assert_eq!(source_session_name.as_deref(), Some("target-host-1"));
                assert_eq!(selector.as_deref(), Some("wa-local:shell-1"));
                assert_eq!(availability, "online");
                assert_eq!(session_role, Some("target-host"));
                assert_eq!(workspace_key.as_deref(), Some("wk-1"));
                assert_eq!(command_name.as_deref(), Some("codex"));
                assert_eq!(current_path.as_deref(), Some("/tmp/demo"));
                assert_eq!(attached_clients, 2);
                assert_eq!(window_count, 3);
                assert_eq!(task_state, "unknown");
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn connect_and_send_target_exited_round_trip() {
        let socket_path = test_socket_path("exited");
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
            read_node_session_envelope(&mut stream).expect("exit should decode")
        });

        let transport = RemoteTargetPublicationTransportRuntime::connect(&socket_path, "peer-a")
            .expect("publication transport should connect");
        transport
            .send_target_exited("shell-1", Some("target-host-1"))
            .expect("exit publication should encode");

        let envelope = accept_thread
            .join()
            .expect("accept thread should join cleanly");
        assert_eq!(envelope.channel, NodeSessionChannel::Publication);
        assert_eq!(
            envelope.envelope.target_id.as_deref(),
            Some("remote-peer:peer-a:shell-1")
        );
        assert_eq!(
            envelope.envelope.payload,
            ControlPlanePayload::TargetExited(TargetExitedPayload {
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
            })
        );

        let _ = fs::remove_file(&socket_path);
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some("wa-local:shell-1".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 2,
            window_count: 3,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Unknown,
        }
    }

    fn test_socket_path(label: &str) -> PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "waitagent-publication-{}-{}-{}.sock",
            std::process::id(),
            label,
            now
        ))
    }
}
