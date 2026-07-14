use crate::infra::remote_protocol::{
    ApplyResizePayload, BootstrapMode, ClientHelloPayload, CloseMirrorRequestPayload,
    ControlPlanePayload, CreateSessionAcceptedPayload, CreateSessionRejectedPayload,
    CreateSessionRequestPayload, ErrorPayload, MirrorBootstrapChunkPayload,
    MirrorBootstrapCompletePayload, NodeSessionChannel, NodeSessionEnvelope,
    OpenMirrorAcceptedPayload, OpenMirrorRejectedPayload, OpenMirrorRequestPayload,
    OpenTargetOkPayload, OpenTargetRejectedPayload, ProtocolEnvelope, RawPtyInputPayload,
    RawPtyOutputPayload, ResizeAuthorityChangedPayload, ServerHelloPayload, TargetExitedPayload,
    TargetOutputPayload, TargetPublicationAckPayload, TargetPublicationAckStatus,
    TargetPublishedPayload,
};
use std::fmt;
use std::io::{self, Cursor, Read, Write};

const REGISTRATION_MAGIC: &[u8; 4] = b"wr1n";
const AUTHORITY_FRAME_MAGIC: &[u8; 4] = b"waRP";
const AUTHORITY_FRAME_CONTROL_PLANE: u8 = 1;
const AUTHORITY_FRAME_RAW_PTY_INPUT: u8 = 2;
const AUTHORITY_FRAME_RAW_PTY_OUTPUT: u8 = 3;
const AUTHORITY_FRAME_PING: u8 = 4;
const AUTHORITY_FRAME_PONG: u8 = 5;
const AUTHORITY_FRAME_SYNC_REQUEST: u8 = 6;
const AUTHORITY_FRAME_SYNC_RESPONSE: u8 = 7;
const AUTHORITY_FRAME_INPUT_CONGESTION: u8 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityTransportFrame {
    ControlPlane(ProtocolEnvelope<ControlPlanePayload>),
    RawPtyInput(RawPtyInputPayload),
    RawPtyOutput(RawPtyOutputPayload),
    Ping,
    Pong,
    SyncRequest {
        expected_seq: u64,
        received_seq: u64,
    },
    SyncResponse {
        session_id: String,
        target_id: String,
        seq: u64,
        bytes: Vec<u8>,
    },
    InputCongestion(bool),
}

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
    write_control_plane_envelope_without_flush(writer, envelope)?;
    writer.flush()?;
    Ok(())
}

fn write_control_plane_envelope_without_flush(
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

pub fn write_authority_transport_frame(
    writer: &mut impl Write,
    frame: &AuthorityTransportFrame,
) -> Result<(), RemoteTransportCodecError> {
    writer.write_all(AUTHORITY_FRAME_MAGIC)?;
    match frame {
        AuthorityTransportFrame::ControlPlane(envelope) => {
            write_u8(writer, AUTHORITY_FRAME_CONTROL_PLANE)?;
            write_control_plane_envelope_without_flush(writer, envelope)?;
        }
        AuthorityTransportFrame::RawPtyInput(payload) => {
            write_u8(writer, AUTHORITY_FRAME_RAW_PTY_INPUT)?;
            write_raw_pty_input_payload(writer, payload)?;
        }
        AuthorityTransportFrame::RawPtyOutput(payload) => {
            write_u8(writer, AUTHORITY_FRAME_RAW_PTY_OUTPUT)?;
            write_raw_pty_output_payload(writer, payload)?;
        }
        AuthorityTransportFrame::Ping => {
            write_u8(writer, AUTHORITY_FRAME_PING)?;
        }
        AuthorityTransportFrame::Pong => {
            write_u8(writer, AUTHORITY_FRAME_PONG)?;
        }
        AuthorityTransportFrame::SyncRequest {
            expected_seq,
            received_seq,
        } => {
            write_u8(writer, AUTHORITY_FRAME_SYNC_REQUEST)?;
            write_u64(writer, *expected_seq)?;
            write_u64(writer, *received_seq)?;
        }
        AuthorityTransportFrame::SyncResponse {
            session_id,
            target_id,
            seq,
            bytes,
        } => {
            write_u8(writer, AUTHORITY_FRAME_SYNC_RESPONSE)?;
            write_string(writer, session_id)?;
            write_string(writer, target_id)?;
            write_u64(writer, *seq)?;
            write_bytes(writer, bytes)?;
        }
        AuthorityTransportFrame::InputCongestion(congested) => {
            write_u8(writer, AUTHORITY_FRAME_INPUT_CONGESTION)?;
            write_u8(writer, u8::from(*congested))?;
        }
    }
    writer.flush()?;
    Ok(())
}

pub fn read_authority_transport_frame(
    reader: &mut impl Read,
) -> Result<AuthorityTransportFrame, RemoteTransportCodecError> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix)?;
    if &prefix != AUTHORITY_FRAME_MAGIC {
        let mut chained = Cursor::new(prefix).chain(reader);
        return read_control_plane_envelope(&mut chained)
            .map(AuthorityTransportFrame::ControlPlane);
    }
    match read_u8(reader)? {
        AUTHORITY_FRAME_CONTROL_PLANE => {
            read_control_plane_envelope(reader).map(AuthorityTransportFrame::ControlPlane)
        }
        AUTHORITY_FRAME_RAW_PTY_INPUT => {
            read_raw_pty_input_payload(reader).map(AuthorityTransportFrame::RawPtyInput)
        }
        AUTHORITY_FRAME_RAW_PTY_OUTPUT => {
            read_raw_pty_output_payload(reader).map(AuthorityTransportFrame::RawPtyOutput)
        }
        AUTHORITY_FRAME_PING => Ok(AuthorityTransportFrame::Ping),
        AUTHORITY_FRAME_PONG => Ok(AuthorityTransportFrame::Pong),
        AUTHORITY_FRAME_SYNC_REQUEST => {
            let expected_seq = read_u64(reader)?;
            let received_seq = read_u64(reader)?;
            Ok(AuthorityTransportFrame::SyncRequest {
                expected_seq,
                received_seq,
            })
        }
        AUTHORITY_FRAME_SYNC_RESPONSE => {
            let session_id = read_string(reader)?;
            let target_id = read_string(reader)?;
            let seq = read_u64(reader)?;
            let bytes = read_bytes(reader)?;
            Ok(AuthorityTransportFrame::SyncResponse {
                session_id,
                target_id,
                seq,
                bytes,
            })
        }
        AUTHORITY_FRAME_INPUT_CONGESTION => Ok(AuthorityTransportFrame::InputCongestion(
            read_u8(reader)? != 0,
        )),
        other => Err(RemoteTransportCodecError::new(format!(
            "unknown authority transport frame tag `{other}`"
        ))),
    }
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
    io_kind: Option<io::ErrorKind>,
}

impl RemoteTransportCodecError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            io_kind: None,
        }
    }

    /// On Linux `SO_RCVTIMEO` produces `EAGAIN` (WouldBlock), not `TimedOut`.
    /// This check covers both so the reader thread correctly enters the
    /// Ping / Pong keepalive path instead of exiting on a socket timeout.
    pub fn is_read_timeout(&self) -> bool {
        matches!(
            self.io_kind,
            Some(io::ErrorKind::TimedOut) | Some(io::ErrorKind::WouldBlock)
        )
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
        Self {
            io_kind: Some(value.kind()),
            ..Self::new(value.to_string())
        }
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
        ControlPlanePayload::OpenMirrorRequest(payload) => {
            write_u8(writer, 12)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_string(writer, &payload.console_id)?;
            write_usize(writer, payload.cols)?;
            write_usize(writer, payload.rows)?;
            write_bool(writer, payload.raw_pty_passthrough)?;
            write_bool(
                writer,
                matches!(payload.bootstrap_mode, BootstrapMode::VisibleOnly),
            )?;
        }
        ControlPlanePayload::OpenMirrorAccepted(payload) => {
            write_u8(writer, 13)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_static_string(writer, payload.availability)?;
        }
        ControlPlanePayload::OpenMirrorRejected(payload) => {
            write_u8(writer, 14)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_static_string(writer, payload.code)?;
            write_string(writer, &payload.message)?;
        }
        ControlPlanePayload::CloseMirrorRequest(payload) => {
            write_u8(writer, 15)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
        }
        ControlPlanePayload::MirrorBootstrapChunk(payload) => {
            write_u8(writer, 16)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_u64(writer, payload.chunk_seq)?;
            write_static_string(writer, payload.stream)?;
            write_bytes(writer, &payload.output_bytes)?;
        }
        ControlPlanePayload::MirrorBootstrapComplete(payload) => {
            write_u8(writer, 17)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_u64(writer, payload.last_chunk_seq)?;
            write_bool(writer, payload.alternate_screen_active)?;
            write_bool(writer, payload.application_cursor_keys)?;
            write_bool(writer, payload.cursor_visible)?;
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
        ControlPlanePayload::RawPtyInput(payload) => {
            write_u8(writer, 18)?;
            write_raw_pty_input_payload(writer, payload)?;
        }
        ControlPlanePayload::TargetOutput(payload) => {
            write_u8(writer, 7)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
            write_u64(writer, payload.output_seq)?;
            write_static_string(writer, payload.stream)?;
            write_bytes(writer, &payload.output_bytes)?;
        }
        ControlPlanePayload::RawPtyOutput(payload) => {
            write_u8(writer, 19)?;
            write_raw_pty_output_payload(writer, payload)?;
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
        ControlPlanePayload::CreateSessionRequest(payload) => {
            write_u8(writer, 70)?;
            write_string(writer, &payload.request_id)?;
            write_string(writer, &payload.authority_node_id)?;
            write_optional_string(writer, payload.cwd_hint.as_deref())?;
            write_usize(writer, payload.cols)?;
            write_usize(writer, payload.rows)?;
        }
        ControlPlanePayload::CreateSessionAccepted(payload) => {
            write_u8(writer, 71)?;
            write_string(writer, &payload.request_id)?;
            write_string(writer, &payload.session_id)?;
            write_string(writer, &payload.target_id)?;
        }
        ControlPlanePayload::CreateSessionRejected(payload) => {
            write_u8(writer, 72)?;
            write_string(writer, &payload.request_id)?;
            write_static_string(writer, payload.code)?;
            write_string(writer, &payload.message)?;
        }
        ControlPlanePayload::TargetPublished(payload) => {
            write_u8(writer, 9)?;
            write_string(writer, &payload.transport_session_id)?;
            write_string(writer, &payload.node_instance_id)?;
            write_u64(writer, payload.revision)?;
            write_optional_string(writer, payload.source_session_name.as_deref())?;
            write_optional_string(writer, payload.selector.as_deref())?;
            write_static_string(writer, payload.availability)?;
            write_optional_static_string(writer, payload.session_role)?;
            write_optional_string(writer, payload.workspace_key.as_deref())?;
            write_optional_string(writer, payload.command_name.as_deref())?;
            write_optional_string(writer, payload.display_command_name.as_deref())?;
            write_optional_string(writer, payload.current_path.as_deref())?;
            write_usize(writer, payload.attached_clients)?;
            write_usize(writer, payload.window_count)?;
            write_string(writer, payload.task_state)?;
        }
        ControlPlanePayload::TargetExited(payload) => {
            write_u8(writer, 10)?;
            write_string(writer, &payload.transport_session_id)?;
            write_string(writer, &payload.node_instance_id)?;
            write_u64(writer, payload.revision)?;
            write_optional_string(writer, payload.source_session_name.as_deref())?;
        }
        ControlPlanePayload::TargetPublicationAck(payload) => {
            write_u8(writer, 73)?;
            write_string(writer, &payload.node_id)?;
            write_string(writer, &payload.node_instance_id)?;
            write_string(writer, &payload.target_id)?;
            write_u64(writer, payload.revision)?;
            write_target_publication_ack_status(writer, payload.status)?;
            write_optional_string(writer, payload.message.as_deref())?;
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
        12 => ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            console_id: read_string(reader)?,
            cols: read_usize(reader)?,
            rows: read_usize(reader)?,
            raw_pty_passthrough: read_bool(reader)?,
            bootstrap_mode: if read_bool(reader)? {
                BootstrapMode::VisibleOnly
            } else {
                BootstrapMode::Full
            },
        }),
        13 => ControlPlanePayload::OpenMirrorAccepted(OpenMirrorAcceptedPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            availability: read_known_static_string(reader)?,
        }),
        14 => ControlPlanePayload::OpenMirrorRejected(OpenMirrorRejectedPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            code: read_known_static_string(reader)?,
            message: read_string(reader)?,
        }),
        15 => ControlPlanePayload::CloseMirrorRequest(CloseMirrorRequestPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
        }),
        16 => ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            chunk_seq: read_u64(reader)?,
            stream: read_known_static_string(reader)?,
            output_bytes: read_bytes(reader)?,
        }),
        17 => ControlPlanePayload::MirrorBootstrapComplete(MirrorBootstrapCompletePayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            last_chunk_seq: read_u64(reader)?,
            alternate_screen_active: read_bool(reader)?,
            application_cursor_keys: read_bool(reader)?,
            cursor_visible: read_bool(reader)?,
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
        18 => ControlPlanePayload::RawPtyInput(read_raw_pty_input_payload(reader)?),
        7 => ControlPlanePayload::TargetOutput(TargetOutputPayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            output_seq: read_u64(reader)?,
            stream: read_known_static_string(reader)?,
            output_bytes: read_bytes(reader)?,
        }),
        19 => ControlPlanePayload::RawPtyOutput(read_raw_pty_output_payload(reader)?),
        8 => ControlPlanePayload::ApplyResize(ApplyResizePayload {
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
            resize_epoch: read_u64(reader)?,
            resize_authority_console_id: read_string(reader)?,
            cols: read_usize(reader)?,
            rows: read_usize(reader)?,
        }),
        70 => ControlPlanePayload::CreateSessionRequest(CreateSessionRequestPayload {
            request_id: read_string(reader)?,
            authority_node_id: read_string(reader)?,
            cwd_hint: read_optional_string(reader)?,
            cols: read_usize(reader)?,
            rows: read_usize(reader)?,
        }),
        71 => ControlPlanePayload::CreateSessionAccepted(CreateSessionAcceptedPayload {
            request_id: read_string(reader)?,
            session_id: read_string(reader)?,
            target_id: read_string(reader)?,
        }),
        72 => ControlPlanePayload::CreateSessionRejected(CreateSessionRejectedPayload {
            request_id: read_string(reader)?,
            code: read_known_static_string(reader)?,
            message: read_string(reader)?,
        }),
        9 => ControlPlanePayload::TargetPublished(TargetPublishedPayload {
            transport_session_id: read_string(reader)?,
            node_instance_id: read_string(reader)?,
            revision: read_u64(reader)?,
            source_session_name: read_optional_string(reader)?,
            selector: read_optional_string(reader)?,
            availability: read_known_static_string(reader)?,
            session_role: read_optional_static_string(reader)?,
            workspace_key: read_optional_string(reader)?,
            command_name: read_optional_string(reader)?,
            display_command_name: read_optional_string(reader)?,
            current_path: read_optional_string(reader)?,
            attached_clients: read_usize(reader)?,
            window_count: read_usize(reader)?,
            task_state: read_task_state(reader)?,
        }),
        10 => ControlPlanePayload::TargetExited(TargetExitedPayload {
            transport_session_id: read_string(reader)?,
            node_instance_id: read_string(reader)?,
            revision: read_u64(reader)?,
            source_session_name: read_optional_string(reader)?,
        }),
        73 => ControlPlanePayload::TargetPublicationAck(TargetPublicationAckPayload {
            node_id: read_string(reader)?,
            node_instance_id: read_string(reader)?,
            target_id: read_string(reader)?,
            revision: read_u64(reader)?,
            status: read_target_publication_ack_status(reader)?,
            message: read_optional_string(reader)?,
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

fn write_bytes(writer: &mut impl Write, value: &[u8]) -> Result<(), RemoteTransportCodecError> {
    let len = u32::try_from(value.len())
        .map_err(|_| RemoteTransportCodecError::new("bytes too long for transport frame"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(value)?;
    Ok(())
}

fn write_raw_pty_input_payload(
    writer: &mut impl Write,
    payload: &RawPtyInputPayload,
) -> Result<(), RemoteTransportCodecError> {
    write_string(writer, &payload.attachment_id)?;
    write_string(writer, &payload.session_id)?;
    write_string(writer, &payload.target_id)?;
    write_string(writer, &payload.console_id)?;
    write_string(writer, &payload.console_host_id)?;
    write_u64(writer, payload.input_seq)?;
    write_bytes(writer, &payload.input_bytes)?;
    Ok(())
}

fn read_raw_pty_input_payload(
    reader: &mut impl Read,
) -> Result<RawPtyInputPayload, RemoteTransportCodecError> {
    Ok(RawPtyInputPayload {
        attachment_id: read_string(reader)?,
        session_id: read_string(reader)?,
        target_id: read_string(reader)?,
        console_id: read_string(reader)?,
        console_host_id: read_string(reader)?,
        input_seq: read_u64(reader)?,
        input_bytes: read_bytes(reader)?,
    })
}

fn write_raw_pty_output_payload(
    writer: &mut impl Write,
    payload: &RawPtyOutputPayload,
) -> Result<(), RemoteTransportCodecError> {
    write_string(writer, &payload.session_id)?;
    write_string(writer, &payload.target_id)?;
    write_u64(writer, payload.output_seq)?;
    write_bytes(writer, &payload.output_bytes)?;
    Ok(())
}

fn read_raw_pty_output_payload(
    reader: &mut impl Read,
) -> Result<RawPtyOutputPayload, RemoteTransportCodecError> {
    Ok(RawPtyOutputPayload {
        session_id: read_string(reader)?,
        target_id: read_string(reader)?,
        output_seq: read_u64(reader)?,
        output_bytes: read_bytes(reader)?,
    })
}

fn read_bytes(reader: &mut impl Read) -> Result<Vec<u8>, RemoteTransportCodecError> {
    let len = read_u32(reader)? as usize;
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn write_string(writer: &mut impl Write, value: &str) -> Result<(), RemoteTransportCodecError> {
    write_bytes(writer, value.as_bytes())
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
        "mirror_not_available" => Ok("mirror_not_available"),
        "unauthorized" => Ok("unauthorized"),
        "pty" => Ok("pty"),
        "stdout" => Ok("stdout"),
        "stderr" => Ok("stderr"),
        "test" => Ok("test"),
        "republish_live_targets" => Ok("republish_live_targets"),
        "resize_denied" => Ok("resize_denied"),
        "target_not_opened" => Ok("target_not_opened"),
        "attachment_not_open" => Ok("attachment_not_open"),
        "attachment_closed" => Ok("attachment_closed"),
        "create_session_failed" => Ok("create_session_failed"),
        "workspace-chrome" => Ok("workspace-chrome"),
        "target-host" => Ok("target-host"),
        other => Err(RemoteTransportCodecError::new(format!(
            "unknown static transport string `{other}`"
        ))),
    }
}

fn read_task_state(reader: &mut impl Read) -> Result<&'static str, RemoteTransportCodecError> {
    let value = read_string(reader)?;
    crate::domain::session_catalog::ManagedSessionTaskState::parse(&value)
        .map(|state| state.as_str())
        .ok_or_else(|| {
            RemoteTransportCodecError::new(format!("unknown task state string `{value}`"))
        })
}

fn write_target_publication_ack_status(
    writer: &mut impl Write,
    status: TargetPublicationAckStatus,
) -> Result<(), RemoteTransportCodecError> {
    write_u8(
        writer,
        match status {
            TargetPublicationAckStatus::Applied => 1,
            TargetPublicationAckStatus::StaleRevision => 2,
            TargetPublicationAckStatus::Failed => 3,
        },
    )
}

fn read_target_publication_ack_status(
    reader: &mut impl Read,
) -> Result<TargetPublicationAckStatus, RemoteTransportCodecError> {
    match read_u8(reader)? {
        1 => Ok(TargetPublicationAckStatus::Applied),
        2 => Ok(TargetPublicationAckStatus::StaleRevision),
        3 => Ok(TargetPublicationAckStatus::Failed),
        other => Err(RemoteTransportCodecError::new(format!(
            "unknown target publication ack status `{other}`"
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

#[allow(dead_code)]
fn write_u32(writer: &mut impl Write, value: u32) -> Result<(), RemoteTransportCodecError> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u32(reader: &mut impl Read) -> Result<u32, RemoteTransportCodecError> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_bool(writer: &mut impl Write, value: bool) -> Result<(), RemoteTransportCodecError> {
    write_u8(writer, if value { 1 } else { 0 })
}

fn read_bool(reader: &mut impl Read) -> Result<bool, RemoteTransportCodecError> {
    match read_u8(reader)? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(RemoteTransportCodecError::new(format!(
            "invalid bool tag `{other}`"
        ))),
    }
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
        read_authority_transport_frame, read_control_plane_envelope, read_node_session_envelope,
        read_registration_frame, write_authority_transport_frame, write_control_plane_envelope,
        write_node_session_envelope, write_registration_frame, AuthorityTransportFrame,
    };
    use crate::infra::remote_protocol::{
        BootstrapMode, CloseMirrorRequestPayload, ControlPlanePayload,
        CreateSessionAcceptedPayload, CreateSessionRejectedPayload, CreateSessionRequestPayload,
        NodeSessionChannel, NodeSessionEnvelope, OpenMirrorAcceptedPayload,
        OpenMirrorRequestPayload, ProtocolEnvelope, RawPtyInputPayload, RawPtyOutputPayload,
        TargetOutputPayload, TargetPublicationAckPayload, TargetPublicationAckStatus,
        TargetPublishedPayload,
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
                output_bytes: b"a".to_vec(),
            }),
        };
        let mut bytes = Vec::new();

        write_control_plane_envelope(&mut bytes, &envelope).expect("envelope should encode");
        let decoded =
            read_control_plane_envelope(&mut bytes.as_slice()).expect("envelope should decode");

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn authority_transport_frame_round_trips_raw_pty_input_without_envelope() {
        let frame = AuthorityTransportFrame::RawPtyInput(RawPtyInputPayload {
            attachment_id: "attach-1".to_string(),
            session_id: "shell-1".to_string(),
            target_id: "remote-peer:peer-a:shell-1".to_string(),
            console_id: "console-a".to_string(),
            console_host_id: "observer-a".to_string(),
            input_seq: 9,
            input_bytes: b"abc\r".to_vec(),
        });
        let mut bytes = Vec::new();

        write_authority_transport_frame(&mut bytes, &frame).expect("frame should encode");
        let decoded =
            read_authority_transport_frame(&mut bytes.as_slice()).expect("frame should decode");

        assert_eq!(decoded, frame);
    }

    #[test]
    fn authority_transport_frame_round_trips_raw_pty_output_without_envelope() {
        let frame = AuthorityTransportFrame::RawPtyOutput(RawPtyOutputPayload {
            session_id: "shell-1".to_string(),
            target_id: "remote-peer:peer-a:shell-1".to_string(),
            output_seq: 7,
            output_bytes: b"\x1b[32mok\r\n".to_vec(),
        });
        let mut bytes = Vec::new();

        write_authority_transport_frame(&mut bytes, &frame).expect("frame should encode");
        let decoded =
            read_authority_transport_frame(&mut bytes.as_slice()).expect("frame should decode");

        assert_eq!(decoded, frame);
    }

    #[test]
    fn sync_request_and_response_frames_round_trip() {
        let frames: Vec<AuthorityTransportFrame> = vec![
            AuthorityTransportFrame::SyncRequest {
                expected_seq: 42,
                received_seq: 47,
            },
            AuthorityTransportFrame::SyncResponse {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                seq: 43,
                bytes: b"\x1b[32mreplay data\r\n".to_vec(),
            },
            AuthorityTransportFrame::InputCongestion(true),
        ];
        for frame in frames {
            let mut bytes = Vec::new();
            write_authority_transport_frame(&mut bytes, &frame).expect("should encode");
            let decoded =
                read_authority_transport_frame(&mut bytes.as_slice()).expect("should decode");
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn ping_pong_frames_round_trip_without_payload() {
        for frame in [AuthorityTransportFrame::Ping, AuthorityTransportFrame::Pong] {
            let mut bytes = Vec::new();
            write_authority_transport_frame(&mut bytes, &frame).expect("ping/pong should encode");
            let decoded = read_authority_transport_frame(&mut bytes.as_slice())
                .expect("ping/pong should decode");
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn control_plane_envelope_round_trips_create_session_messages() {
        let request = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-create-session".to_string(),
            message_type: "create_session_request",
            timestamp: "2026-06-16T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: Some("req-1".to_string()),
            session_id: None,
            target_id: None,
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::CreateSessionRequest(CreateSessionRequestPayload {
                request_id: "req-1".to_string(),
                authority_node_id: "node-130".to_string(),
                cwd_hint: Some("/opt/data/workspace/app-insight".to_string()),
                cols: 120,
                rows: 40,
            }),
        };
        let accepted = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-create-session-ok".to_string(),
            message_type: "create_session_accepted",
            timestamp: "2026-06-16T00:00:00Z".to_string(),
            sender_id: "node-130".to_string(),
            correlation_id: Some("req-1".to_string()),
            session_id: Some("session-1".to_string()),
            target_id: Some("remote-peer:node-130:session-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::CreateSessionAccepted(CreateSessionAcceptedPayload {
                request_id: "req-1".to_string(),
                session_id: "session-1".to_string(),
                target_id: "remote-peer:node-130:session-1".to_string(),
            }),
        };
        let rejected = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-create-session-rejected".to_string(),
            message_type: "create_session_rejected",
            timestamp: "2026-06-16T00:00:00Z".to_string(),
            sender_id: "node-130".to_string(),
            correlation_id: Some("req-1".to_string()),
            session_id: None,
            target_id: None,
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::CreateSessionRejected(CreateSessionRejectedPayload {
                request_id: "req-1".to_string(),
                code: "create_session_failed",
                message: "failed to create target-host session".to_string(),
            }),
        };

        for envelope in [request, accepted, rejected] {
            let mut bytes = Vec::new();
            write_control_plane_envelope(&mut bytes, &envelope).expect("envelope should encode");
            let decoded =
                read_control_plane_envelope(&mut bytes.as_slice()).expect("envelope should decode");
            assert_eq!(decoded, envelope);
        }
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
                node_instance_id: "node-inst-1".to_string(),
                revision: 7,
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wa-local:shell-1".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                display_command_name: None,
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "input",
            }),
        };
        let mut bytes = Vec::new();

        write_control_plane_envelope(&mut bytes, &envelope).expect("envelope should encode");
        let decoded =
            read_control_plane_envelope(&mut bytes.as_slice()).expect("envelope should decode");

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn control_plane_envelope_round_trips_target_publication_ack() {
        let envelope = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-ack".to_string(),
            message_type: "target_publication_ack",
            timestamp: "2026-06-23T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: Some("corr-ack".to_string()),
            session_id: None,
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetPublicationAck(TargetPublicationAckPayload {
                node_id: "peer-a".to_string(),
                node_instance_id: "node-inst-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                revision: 7,
                status: TargetPublicationAckStatus::Applied,
                message: Some("applied".to_string()),
            }),
        };
        let mut bytes = Vec::new();

        write_control_plane_envelope(&mut bytes, &envelope).expect("envelope should encode");
        let decoded =
            read_control_plane_envelope(&mut bytes.as_slice()).expect("envelope should decode");

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn control_plane_envelope_round_trips_open_mirror_request() {
        let envelope = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-open-mirror".to_string(),
            message_type: "open_mirror_request",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: Some("corr-open-mirror".to_string()),
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
        };
        let mut bytes = Vec::new();

        write_control_plane_envelope(&mut bytes, &envelope).expect("envelope should encode");
        let decoded =
            read_control_plane_envelope(&mut bytes.as_slice()).expect("envelope should decode");

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn control_plane_envelope_round_trips_mirror_lifecycle_replies() {
        let accepted = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-open-mirror-ok".to_string(),
            message_type: "open_mirror_accepted",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: Some("corr-open-mirror".to_string()),
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::OpenMirrorAccepted(OpenMirrorAcceptedPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                availability: "online",
            }),
        };
        let close = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-close-mirror".to_string(),
            message_type: "close_mirror_request",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: Some("corr-close-mirror".to_string()),
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::CloseMirrorRequest(CloseMirrorRequestPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
            }),
        };

        for envelope in [accepted, close] {
            let mut bytes = Vec::new();
            write_control_plane_envelope(&mut bytes, &envelope).expect("envelope should encode");
            let decoded =
                read_control_plane_envelope(&mut bytes.as_slice()).expect("envelope should decode");
            assert_eq!(decoded, envelope);
        }
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
                    node_instance_id: "node-inst-1".to_string(),
                    revision: 7,
                    source_session_name: Some("target-host-1".to_string()),
                    selector: Some("wk:shell".to_string()),
                    availability: "online",
                    session_role: Some("target-host"),
                    workspace_key: Some("wk-1".to_string()),
                    command_name: Some("codex".to_string()),
                    display_command_name: None,
                    current_path: Some("/tmp/demo".to_string()),
                    attached_clients: 2,
                    window_count: 1,
                    task_state: "running",
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
