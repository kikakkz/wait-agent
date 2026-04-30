use crate::infra::remote_protocol::{
    ApplyResizePayload, ClientHelloPayload, ControlPlanePayload, ErrorPayload, NodeSessionChannel,
    NodeSessionEnvelope, OpenTargetOkPayload, OpenTargetRejectedPayload, ProtocolEnvelope,
    ResizeAuthorityChangedPayload, ServerHelloPayload, TargetExitedPayload, TargetInputPayload,
    TargetOutputPayload, TargetPublishedPayload,
};
use std::fmt;
use std::io::{self, Read, Write};

const REGISTRATION_MAGIC: &[u8; 4] = b"wr1n";

pub fn write_registration_frame(
    writer: &mut impl Write,
    node_id: &str,
) -> Result<(), RemoteTransportCodecError> {
    writer.write_all(REGISTRATION_MAGIC)?;
    write_string(writer, node_id)?;
    writer.flush()?;
    Ok(())
}

pub fn read_registration_frame(
    reader: &mut impl Read,
) -> Result<String, RemoteTransportCodecError> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != REGISTRATION_MAGIC {
        return Err(RemoteTransportCodecError::new(
            "invalid remote transport registration magic",
        ));
    }
    read_string(reader)
}

pub fn write_control_plane_envelope(
    writer: &mut impl Write,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<(), RemoteTransportCodecError> {
    write_string(writer, &envelope.protocol_version)?;
    write_string(writer, &envelope.message_id)?;
    write_string(writer, &envelope.timestamp)?;
    write_string(writer, &envelope.sender_id)?;
    write_optional_string(writer, envelope.correlation_id.as_deref())?;
    write_optional_string(writer, envelope.session_id.as_deref())?;
    write_optional_string(writer, envelope.target_id.as_deref())?;
    write_optional_string(writer, envelope.attachment_id.as_deref())?;
    write_optional_string(writer, envelope.console_id.as_deref())?;
    write_payload(writer, &envelope.payload)?;
    writer.flush()?;
    Ok(())
}

pub fn write_node_session_envelope(
    writer: &mut impl Write,
    envelope: &NodeSessionEnvelope,
) -> Result<(), RemoteTransportCodecError> {
    write_node_session_channel(writer, envelope.channel)?;
    write_control_plane_envelope(writer, &envelope.envelope)
}

pub fn read_control_plane_envelope(
    reader: &mut impl Read,
) -> Result<ProtocolEnvelope<ControlPlanePayload>, RemoteTransportCodecError> {
    let protocol_version = read_string(reader)?;
    let message_id = read_string(reader)?;
    let timestamp = read_string(reader)?;
    let sender_id = read_string(reader)?;
    let correlation_id = read_optional_string(reader)?;
    let session_id = read_optional_string(reader)?;
    let target_id = read_optional_string(reader)?;
    let attachment_id = read_optional_string(reader)?;
    let console_id = read_optional_string(reader)?;
    let payload = read_payload(reader)?;
    let message_type = payload.message_type();

    Ok(ProtocolEnvelope {
        protocol_version,
        message_id,
        message_type,
        timestamp,
        sender_id,
        correlation_id,
        session_id,
        target_id,
        attachment_id,
        console_id,
        payload,
    })
}

pub fn read_node_session_envelope(
    reader: &mut impl Read,
) -> Result<NodeSessionEnvelope, RemoteTransportCodecError> {
    let channel = read_node_session_channel(reader)?;
    let envelope = read_control_plane_envelope(reader)?;
    Ok(NodeSessionEnvelope { channel, envelope })
}

#[derive(Debug)]
pub struct RemoteTransportCodecError {
    message: String,
}

impl RemoteTransportCodecError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteTransportCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteTransportCodecError {}

impl From<io::Error> for RemoteTransportCodecError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

fn write_payload(
    writer: &mut impl Write,
    payload: &ControlPlanePayload,
) -> Result<(), RemoteTransportCodecError> {
    match payload {
        ControlPlanePayload::ClientHello(payload) => {
            write_u8(writer, 1)?;
            write_string(writer, &payload.node_id)?;
            write_string(writer, &payload.client_version)?;
        }
        ControlPlanePayload::ServerHello(payload) => {
            write_u8(writer, 2)?;
            write_string(writer, &payload.server_id)?;
            write_string(writer, &payload.server_version)?;
            write_string(writer, &payload.accepted_protocol_version)?;
            write_u64(writer, payload.heartbeat_interval_ms)?;
            write_static_string(writer, payload.session_recovery_policy)?;
        }
        ControlPlanePayload::OpenTargetOk(payload) => {
            write_u8(writer, 3)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_string(writer, &payload.attachment_id)?;
            write_string(writer, &payload.console_id)?;
            write_u64(writer, payload.resize_epoch)?;
            write_string(writer, &payload.resize_authority_console_id)?;
            write_string(writer, &payload.resize_authority_host_id)?;
            write_static_string(writer, payload.availability)?;
            write_optional_string(writer, payload.initial_snapshot.as_deref())?;
        }
        ControlPlanePayload::OpenTargetRejected(payload) => {
            write_u8(writer, 4)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_string(writer, &payload.console_id)?;
            write_static_string(writer, payload.code)?;
            write_string(writer, &payload.message)?;
        }
        ControlPlanePayload::ResizeAuthorityChanged(payload) => {
            write_u8(writer, 5)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_u64(writer, payload.resize_epoch)?;
            write_string(writer, &payload.resize_authority_console_id)?;
            write_string(writer, &payload.resize_authority_host_id)?;
            write_optional_usize(writer, payload.cols)?;
            write_optional_usize(writer, payload.rows)?;
        }
        ControlPlanePayload::TargetInput(payload) => {
            write_u8(writer, 6)?;
            write_string(writer, &payload.attachment_id)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_string(writer, &payload.console_id)?;
            write_string(writer, &payload.console_host_id)?;
            write_u64(writer, payload.input_seq)?;
            write_string(writer, &payload.bytes_base64)?;
        }
        ControlPlanePayload::TargetOutput(payload) => {
            write_u8(writer, 7)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_u64(writer, payload.output_seq)?;
            write_static_string(writer, payload.stream)?;
            write_string(writer, &payload.bytes_base64)?;
        }
        ControlPlanePayload::ApplyResize(payload) => {
            write_u8(writer, 8)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_u64(writer, payload.resize_epoch)?;
            write_string(writer, &payload.resize_authority_console_id)?;
            write_usize(writer, payload.cols)?;
            write_usize(writer, payload.rows)?;
        }
        ControlPlanePayload::TargetPublished(payload) => {
            write_u8(writer, 9)?;
            write_string(writer, &payload.transport_session_id)?;
            write_optional_string(writer, payload.source_session_name.as_deref())?;
            write_optional_string(writer, payload.selector.as_deref())?;
            write_static_string(writer, payload.availability)?;
            write_optional_static_string(writer, payload.session_role)?;
            write_optional_string(writer, payload.workspace_key.as_deref())?;
            write_optional_string(writer, payload.command_name.as_deref())?;
            write_optional_string(writer, payload.current_path.as_deref())?;
            write_usize(writer, payload.attached_clients)?;
            write_usize(writer, payload.window_count)?;
        }
        ControlPlanePayload::TargetExited(payload) => {
            write_u8(writer, 10)?;
            write_string(writer, &payload.transport_session_id)?;
            write_optional_string(writer, payload.source_session_name.as_deref())?;
        }
        ControlPlanePayload::Error(payload) => {
            write_u8(writer, 11)?;
            write_static_string(writer, payload.code)?;
            write_string(writer, &payload.message)?;
            write_optional_string(writer, payload.details.as_deref())?;
        }
    }
    Ok(())
}

fn read_payload(reader: &mut impl Read) -> Result<ControlPlanePayload, RemoteTransportCodecError> {
    Ok(match read_u8(reader)? {
        1 => ControlPlanePayload::ClientHello(ClientHelloPayload {
            node_id: read_string(reader)?,
            client_version: read_string(reader)?,
        }),
        2 => ControlPlanePayload::ServerHello(ServerHelloPayload {
            server_id: read_string(reader)?,
            server_version: read_string(reader)?,
            accepted_protocol_version: read_string(reader)?,
            heartbeat_interval_ms: read_u64(reader)?,
            session_recovery_policy: read_known_static_string(reader)?,
        }),
        3 => ControlPlanePayload::OpenTargetOk(OpenTargetOkPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            attachment_id: read_string(reader)?,
            console_id: read_string(reader)?,
            resize_epoch: read_u64(reader)?,
            resize_authority_console_id: read_string(reader)?,
            resize_authority_host_id: read_string(reader)?,
            availability: read_known_static_string(reader)?,
            initial_snapshot: read_optional_string(reader)?,
        }),
        4 => ControlPlanePayload::OpenTargetRejected(OpenTargetRejectedPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            console_id: read_string(reader)?,
            code: read_known_static_string(reader)?,
            message: read_string(reader)?,
        }),
        5 => ControlPlanePayload::ResizeAuthorityChanged(ResizeAuthorityChangedPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            resize_epoch: read_u64(reader)?,
            resize_authority_console_id: read_string(reader)?,
            resize_authority_host_id: read_string(reader)?,
            cols: read_optional_usize(reader)?,
            rows: read_optional_usize(reader)?,
        }),
        6 => ControlPlanePayload::TargetInput(TargetInputPayload {
            attachment_id: read_string(reader)?,
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            console_id: read_string(reader)?,
            console_host_id: read_string(reader)?,
            input_seq: read_u64(reader)?,
            bytes_base64: read_string(reader)?,
        }),
        7 => ControlPlanePayload::TargetOutput(TargetOutputPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            output_seq: read_u64(reader)?,
            stream: read_known_static_string(reader)?,
            bytes_base64: read_string(reader)?,
        }),
        8 => ControlPlanePayload::ApplyResize(ApplyResizePayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            resize_epoch: read_u64(reader)?,
            resize_authority_console_id: read_string(reader)?,
            cols: read_usize(reader)?,
            rows: read_usize(reader)?,
        }),
        9 => ControlPlanePayload::TargetPublished(TargetPublishedPayload {
            transport_session_id: read_string(reader)?,
            source_session_name: read_optional_string(reader)?,
            selector: read_optional_string(reader)?,
            availability: read_known_static_string(reader)?,
            session_role: read_optional_static_string(reader)?,
            workspace_key: read_optional_string(reader)?,
            command_name: read_optional_string(reader)?,
            current_path: read_optional_string(reader)?,
            attached_clients: read_usize(reader)?,
            window_count: read_usize(reader)?,
        }),
        10 => ControlPlanePayload::TargetExited(TargetExitedPayload {
            transport_session_id: read_string(reader)?,
            source_session_name: read_optional_string(reader)?,
        }),
        11 => ControlPlanePayload::Error(ErrorPayload {
            code: read_known_static_string(reader)?,
            message: read_string(reader)?,
            details: read_optional_string(reader)?,
        }),
        other => {
            return Err(RemoteTransportCodecError::new(format!(
                "unknown control-plane payload tag `{other}`"
            )));
        }
    })
}

fn write_node_session_channel(
    writer: &mut impl Write,
    channel: NodeSessionChannel,
) -> Result<(), RemoteTransportCodecError> {
    write_u8(
        writer,
        match channel {
            NodeSessionChannel::Authority => 1,
            NodeSessionChannel::Publication => 2,
        },
    )
}

fn read_node_session_channel(
    reader: &mut impl Read,
) -> Result<NodeSessionChannel, RemoteTransportCodecError> {
    match read_u8(reader)? {
        1 => Ok(NodeSessionChannel::Authority),
        2 => Ok(NodeSessionChannel::Publication),
        other => Err(RemoteTransportCodecError::new(format!(
            "unknown node session channel tag `{other}`"
        ))),
    }
}

fn write_string(writer: &mut impl Write, value: &str) -> Result<(), RemoteTransportCodecError> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| RemoteTransportCodecError::new("string too long for transport frame"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(bytes)?;
    Ok(())
}

fn read_string(reader: &mut impl Read) -> Result<String, RemoteTransportCodecError> {
    let len = read_u32(reader)? as usize;
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|_| RemoteTransportCodecError::new("transport frame contained invalid utf-8"))
}

fn write_static_string(
    writer: &mut impl Write,
    value: &'static str,
) -> Result<(), RemoteTransportCodecError> {
    write_string(writer, value)
}

fn read_known_static_string(
    reader: &mut impl Read,
) -> Result<&'static str, RemoteTransportCodecError> {
    let value = read_string(reader)?;
    match value.as_str() {
        "online" => Ok("online"),
        "offline" => Ok("offline"),
        "exited" => Ok("exited"),
        "unknown_target" => Ok("unknown_target"),
        "target_offline" => Ok("target_offline"),
        "unauthorized" => Ok("unauthorized"),
        "pty" => Ok("pty"),
        "test" => Ok("test"),
        "republish_live_targets" => Ok("republish_live_targets"),
        "resize_denied" => Ok("resize_denied"),
        "target_not_opened" => Ok("target_not_opened"),
        "attachment_not_open" => Ok("attachment_not_open"),
        "attachment_closed" => Ok("attachment_closed"),
        "workspace-chrome" => Ok("workspace-chrome"),
        "target-host" => Ok("target-host"),
        other => Err(RemoteTransportCodecError::new(format!(
            "unknown static transport string `{other}`"
        ))),
    }
}

fn write_optional_static_string(
    writer: &mut impl Write,
    value: Option<&'static str>,
) -> Result<(), RemoteTransportCodecError> {
    write_optional_string(writer, value)
}

fn read_optional_static_string(
    reader: &mut impl Read,
) -> Result<Option<&'static str>, RemoteTransportCodecError> {
    Ok(match read_u8(reader)? {
        0 => None,
        1 => Some(read_known_static_string(reader)?),
        other => {
            return Err(RemoteTransportCodecError::new(format!(
                "invalid optional-static-string tag `{other}`"
            )));
        }
    })
}

fn write_optional_string(
    writer: &mut impl Write,
    value: Option<&str>,
) -> Result<(), RemoteTransportCodecError> {
    match value {
        Some(value) => {
            write_u8(writer, 1)?;
            write_string(writer, value)?;
        }
        None => write_u8(writer, 0)?,
    }
    Ok(())
}

fn read_optional_string(
    reader: &mut impl Read,
) -> Result<Option<String>, RemoteTransportCodecError> {
    Ok(match read_u8(reader)? {
        0 => None,
        1 => Some(read_string(reader)?),
        other => {
            return Err(RemoteTransportCodecError::new(format!(
                "invalid optional-string tag `{other}`"
            )));
        }
    })
}

fn write_optional_usize(
    writer: &mut impl Write,
    value: Option<usize>,
) -> Result<(), RemoteTransportCodecError> {
    match value {
        Some(value) => {
            write_u8(writer, 1)?;
            write_usize(writer, value)?;
        }
        None => write_u8(writer, 0)?,
    }
    Ok(())
}

fn read_optional_usize(reader: &mut impl Read) -> Result<Option<usize>, RemoteTransportCodecError> {
    Ok(match read_u8(reader)? {
        0 => None,
        1 => Some(read_usize(reader)?),
        other => {
            return Err(RemoteTransportCodecError::new(format!(
                "invalid optional-usize tag `{other}`"
            )));
        }
    })
}

fn write_usize(writer: &mut impl Write, value: usize) -> Result<(), RemoteTransportCodecError> {
    let value = u64::try_from(value)
        .map_err(|_| RemoteTransportCodecError::new("usize too large for transport frame"))?;
    write_u64(writer, value)
}

fn read_usize(reader: &mut impl Read) -> Result<usize, RemoteTransportCodecError> {
    let value = read_u64(reader)?;
    usize::try_from(value)
        .map_err(|_| RemoteTransportCodecError::new("u64 too large for usize on this platform"))
}

fn write_u64(writer: &mut impl Write, value: u64) -> Result<(), RemoteTransportCodecError> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u64(reader: &mut impl Read) -> Result<u64, RemoteTransportCodecError> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn write_u32(writer: &mut impl Write, value: u32) -> Result<(), RemoteTransportCodecError> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u32(reader: &mut impl Read) -> Result<u32, RemoteTransportCodecError> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_u8(writer: &mut impl Write, value: u8) -> Result<(), RemoteTransportCodecError> {
    writer.write_all(&[value])?;
    Ok(())
}

fn read_u8(reader: &mut impl Read) -> Result<u8, RemoteTransportCodecError> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

#[cfg(test)]
mod tests {
    use super::{
        read_control_plane_envelope, read_node_session_envelope, read_registration_frame,
        write_control_plane_envelope, write_node_session_envelope, write_registration_frame,
    };
    use crate::infra::remote_protocol::{
        ControlPlanePayload, NodeSessionChannel, NodeSessionEnvelope, ProtocolEnvelope,
        TargetOutputPayload, TargetPublishedPayload,
    };

    #[test]
    fn registration_frame_round_trips_node_id() {
        let mut bytes = Vec::new();
        write_registration_frame(&mut bytes, "peer-a").expect("registration should encode");

        let decoded =
            read_registration_frame(&mut bytes.as_slice()).expect("registration should decode");

        assert_eq!(decoded, "peer-a");
    }

    #[test]
    fn control_plane_envelope_round_trips_target_output() {
        let envelope = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-1".to_string(),
            message_type: "target_output",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: Some("corr-1".to_string()),
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq: 7,
                stream: "pty",
                bytes_base64: "YQ==".to_string(),
            }),
        };
        let mut bytes = Vec::new();

        write_control_plane_envelope(&mut bytes, &envelope).expect("envelope should encode");
        let decoded =
            read_control_plane_envelope(&mut bytes.as_slice()).expect("envelope should decode");

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn control_plane_envelope_round_trips_target_published() {
        let envelope = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-2".to_string(),
            message_type: "target_published",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wa-local:shell-1".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
            }),
        };
        let mut bytes = Vec::new();

        write_control_plane_envelope(&mut bytes, &envelope).expect("envelope should encode");
        let decoded =
            read_control_plane_envelope(&mut bytes.as_slice()).expect("envelope should decode");

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn node_session_envelope_round_trips_channel_and_payload() {
        let envelope = NodeSessionEnvelope {
            channel: NodeSessionChannel::Publication,
            envelope: ProtocolEnvelope {
                protocol_version: "1.1".to_string(),
                message_id: "msg-3".to_string(),
                message_type: "target_published",
                timestamp: "2026-04-28T00:00:00Z".to_string(),
                sender_id: "peer-a".to_string(),
                correlation_id: None,
                session_id: Some("shell-1".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                payload: ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                    transport_session_id: "shell-1".to_string(),
                    source_session_name: Some("target-host-1".to_string()),
                    selector: Some("wk:shell".to_string()),
                    availability: "online",
                    session_role: Some("target-host"),
                    workspace_key: Some("wk-1".to_string()),
                    command_name: Some("codex".to_string()),
                    current_path: Some("/tmp/demo".to_string()),
                    attached_clients: 2,
                    window_count: 1,
                }),
            },
        };
        let mut bytes = Vec::new();

        write_node_session_envelope(&mut bytes, &envelope)
            .expect("node session envelope should encode");
        let decoded =
            read_node_session_envelope(&mut bytes.as_slice()).expect("node session should decode");

        assert_eq!(decoded, envelope);
    }
}
