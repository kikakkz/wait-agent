use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, TargetExitedPayload, TargetPublishedPayload,
    REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    write_control_plane_envelope, write_registration_frame, RemoteTransportCodecError,
};
use std::fmt;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RemoteTargetPublicationTransportRuntime {
    node_id: String,
    writer: Mutex<UnixStream>,
    next_message_id: AtomicU64,
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
        let mut stream = UnixStream::connect(socket_path)?;
        write_registration_frame(&mut stream, &node_id)?;
        Ok(Self {
            node_id,
            writer: Mutex::new(stream),
            next_message_id: AtomicU64::new(0),
        })
    }

    pub fn send_target_published(
        &self,
        target: &ManagedSessionRecord,
        source_session_name: Option<&str>,
    ) -> Result<(), RemoteTargetPublicationTransportError> {
        validate_publication_target(target, &self.node_id)?;
        let current_path = target
            .current_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let payload = ControlPlanePayload::TargetPublished(TargetPublishedPayload {
            transport_session_id: target.address.session_id().to_string(),
            source_session_name: source_session_name.map(str::to_string),
            selector: target.selector.clone(),
            availability: target.availability.as_str(),
            session_role: target
                .session_role
                .as_ref()
                .map(WorkspaceSessionRole::as_str),
            workspace_key: target.workspace_key.clone(),
            command_name: target.command_name.clone(),
            current_path,
            attached_clients: target.attached_clients,
            window_count: target.window_count,
        });
        self.send_envelope(target.address.id().as_str(), payload)
    }

    pub fn send_target_exited(
        &self,
        transport_session_id: &str,
        source_session_name: Option<&str>,
    ) -> Result<(), RemoteTargetPublicationTransportError> {
        let target_id = format!("remote-peer:{}:{transport_session_id}", self.node_id);
        self.send_envelope(
            &target_id,
            ControlPlanePayload::TargetExited(TargetExitedPayload {
                transport_session_id: transport_session_id.to_string(),
                source_session_name: source_session_name.map(str::to_string),
            }),
        )
    }

    fn send_envelope(
        &self,
        target_id: &str,
        payload: ControlPlanePayload,
    ) -> Result<(), RemoteTargetPublicationTransportError> {
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!(
                "{}-publication-msg-{}",
                self.node_id,
                self.next_message_id.fetch_add(1, Ordering::Relaxed) + 1
            ),
            message_type: payload.message_type(),
            timestamp: now_rfc3339_like(),
            sender_id: self.node_id.clone(),
            correlation_id: None,
            target_id: Some(target_id.to_string()),
            attachment_id: None,
            console_id: None,
            payload,
        };
        let mut writer = self
            .writer
            .lock()
            .expect("publication transport writer mutex should not be poisoned");
        write_control_plane_envelope(&mut *writer, &envelope)?;
        Ok(())
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

fn now_rfc3339_like() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
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

impl From<io::Error> for RemoteTargetPublicationTransportError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<RemoteTransportCodecError> for RemoteTargetPublicationTransportError {
    fn from(value: RemoteTransportCodecError) -> Self {
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
        ControlPlanePayload, TargetExitedPayload, TargetPublishedPayload,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, read_registration_frame,
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
            let node_id = read_registration_frame(&mut stream).expect("registration should decode");
            let envelope =
                read_control_plane_envelope(&mut stream).expect("publication should decode");
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
        match envelope.payload {
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
            let _ = read_registration_frame(&mut stream).expect("registration should decode");
            read_control_plane_envelope(&mut stream).expect("exit should decode")
        });

        let transport = RemoteTargetPublicationTransportRuntime::connect(&socket_path, "peer-a")
            .expect("publication transport should connect");
        transport
            .send_target_exited("shell-1", Some("target-host-1"))
            .expect("exit publication should encode");

        let envelope = accept_thread
            .join()
            .expect("accept thread should join cleanly");
        assert_eq!(
            envelope.target_id.as_deref(),
            Some("remote-peer:peer-a:shell-1")
        );
        assert_eq!(
            envelope.payload,
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
