use crate::infra::remote_protocol::{
    ApplyResizePayload, ControlPlanePayload, ProtocolEnvelope, TargetInputPayload,
    TargetOutputPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, write_control_plane_envelope, write_registration_frame,
    RemoteTransportCodecError,
};
use std::fmt;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RemoteAuthorityTransportRuntime {
    node_id: String,
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
    next_message_id: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAuthorityCommand {
    TargetInput(TargetInputPayload),
    ApplyResize(ApplyResizePayload),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAuthorityTransportError {
    message: String,
}

impl RemoteAuthorityTransportRuntime {
    pub fn connect(
        socket_path: impl AsRef<Path>,
        node_id: impl Into<String>,
    ) -> Result<Self, RemoteAuthorityTransportError> {
        let node_id = node_id.into();
        let mut stream = UnixStream::connect(socket_path)?;
        write_registration_frame(&mut stream, &node_id)?;
        let writer = stream.try_clone()?;
        Ok(Self {
            node_id,
            reader: Mutex::new(stream),
            writer: Mutex::new(writer),
            next_message_id: AtomicU64::new(0),
        })
    }

    pub fn recv_command(&self) -> Result<RemoteAuthorityCommand, RemoteAuthorityTransportError> {
        let mut reader = self
            .reader
            .lock()
            .expect("authority transport reader mutex should not be poisoned");
        let envelope = read_control_plane_envelope(&mut *reader)?;
        match envelope.payload {
            ControlPlanePayload::TargetInput(payload) => {
                Ok(RemoteAuthorityCommand::TargetInput(payload))
            }
            ControlPlanePayload::ApplyResize(payload) => {
                Ok(RemoteAuthorityCommand::ApplyResize(payload))
            }
            other => Err(RemoteAuthorityTransportError::new(format!(
                "unexpected authority command `{}`",
                other.message_type()
            ))),
        }
    }

    pub fn send_target_output(
        &self,
        target_id: &str,
        output_seq: u64,
        stream: &'static str,
        bytes_base64: impl Into<String>,
    ) -> Result<(), RemoteAuthorityTransportError> {
        let payload = ControlPlanePayload::TargetOutput(TargetOutputPayload {
            target_id: target_id.to_string(),
            output_seq,
            stream,
            bytes_base64: bytes_base64.into(),
        });
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!(
                "{}-authority-msg-{}",
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
            .expect("authority transport writer mutex should not be poisoned");
        write_control_plane_envelope(&mut *writer, &envelope)?;
        Ok(())
    }
}

impl RemoteAuthorityTransportError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteAuthorityTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteAuthorityTransportError {}

impl From<io::Error> for RemoteAuthorityTransportError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<RemoteTransportCodecError> for RemoteAuthorityTransportError {
    fn from(value: RemoteTransportCodecError) -> Self {
        Self::new(value.to_string())
    }
}

pub fn authority_transport_socket_path(
    socket_name: &str,
    session_name: &str,
    target: &str,
) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-{}-{}-{}.sock",
        remote_transport_path_component(socket_name),
        remote_transport_path_component(session_name),
        remote_transport_path_component(target)
    ))
}

fn remote_transport_path_component(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::{
        authority_transport_socket_path, RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
    };
    use crate::infra::remote_protocol::{
        ApplyResizePayload, ControlPlanePayload, ProtocolEnvelope, TargetInputPayload,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, read_registration_frame, write_control_plane_envelope,
    };
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::process;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn authority_transport_socket_path_is_workspace_and_target_scoped() {
        let path = authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1");
        let rendered = path.to_string_lossy();

        assert!(rendered.contains("waitagent-remote-wa-1-workspace-1-peer-a_shell-1.sock"));
    }

    #[test]
    fn connect_writes_registration_frame() {
        let socket_path = test_socket_path("registration");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let accept_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            read_registration_frame(&mut stream).expect("registration should decode")
        });

        let _runtime = RemoteAuthorityTransportRuntime::connect(&socket_path, "peer-a")
            .expect("authority runtime should connect");
        let registered = accept_thread
            .join()
            .expect("accept thread should join cleanly");

        assert_eq!(registered, "peer-a");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn recv_command_decodes_target_input_and_apply_resize() {
        let socket_path = test_socket_path("recv");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let server_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            let registered =
                read_registration_frame(&mut stream).expect("registration should decode");
            assert_eq!(registered, "peer-a");
            write_control_plane_envelope(&mut stream, &target_input_envelope())
                .expect("target input should encode");
            write_control_plane_envelope(&mut stream, &apply_resize_envelope())
                .expect("apply resize should encode");
        });

        let runtime = RemoteAuthorityTransportRuntime::connect(&socket_path, "peer-a")
            .expect("authority runtime should connect");

        assert_eq!(
            runtime.recv_command().expect("target input should decode"),
            RemoteAuthorityCommand::TargetInput(TargetInputPayload {
                attachment_id: "attach-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 7,
                bytes_base64: "YQ==".to_string(),
            })
        );
        assert_eq!(
            runtime.recv_command().expect("apply resize should decode"),
            RemoteAuthorityCommand::ApplyResize(ApplyResizePayload {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                resize_epoch: 3,
                resize_authority_console_id: "console-a".to_string(),
                cols: 160,
                rows: 50,
            })
        );

        server_thread
            .join()
            .expect("server thread should join cleanly");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn send_target_output_writes_authority_envelope() {
        let socket_path = test_socket_path("send");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let server_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            let registered =
                read_registration_frame(&mut stream).expect("registration should decode");
            assert_eq!(registered, "peer-a");
            read_control_plane_envelope(&mut stream).expect("target output should decode")
        });

        let runtime = RemoteAuthorityTransportRuntime::connect(&socket_path, "peer-a")
            .expect("authority runtime should connect");
        runtime
            .send_target_output("remote-peer:peer-a:shell-1", 11, "pty", "Yg==")
            .expect("target output should send");

        let envelope = server_thread
            .join()
            .expect("server thread should join cleanly");
        assert_eq!(envelope.sender_id, "peer-a");
        assert_eq!(envelope.message_type, "target_output");
        match envelope.payload {
            ControlPlanePayload::TargetOutput(payload) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.output_seq, 11);
                assert_eq!(payload.stream, "pty");
                assert_eq!(payload.bytes_base64, "Yg==");
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        let _ = fs::remove_file(&socket_path);
    }

    fn test_socket_path(name: &str) -> std::path::PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-remote-authority-{name}-{}-{millis}.sock",
            process::id()
        ))
    }

    fn target_input_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-target-input".to_string(),
            message_type: "target_input",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::TargetInput(TargetInputPayload {
                attachment_id: "attach-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 7,
                bytes_base64: "YQ==".to_string(),
            }),
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
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::ApplyResize(ApplyResizePayload {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                resize_epoch: 3,
                resize_authority_console_id: "console-a".to_string(),
                cols: 160,
                rows: 50,
            }),
        }
    }
}
