#![allow(dead_code)]

use crate::session::SessionAddress;
use std::fmt;
use std::io::{self, Read, Write};

pub const CURRENT_PROTOCOL_VERSION: u16 = 1;
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(u16);

impl ProtocolVersion {
    pub fn new(value: u16) -> Self {
        Self(value)
    }

    pub fn current() -> Self {
        Self(CURRENT_PROTOCOL_VERSION)
    }

    pub fn value(self) -> u16 {
        self.0
    }

    pub fn is_supported(self) -> bool {
        (MIN_SUPPORTED_PROTOCOL_VERSION..=CURRENT_PROTOCOL_VERSION).contains(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConnectionId(String);

impl ConnectionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(u64);

impl MessageId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderIdentity {
    ClientNode { node_id: String },
    Server,
    ConsoleHost { console_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMessageKind {
    ClientHello,
    ServerHello,
    Heartbeat,
    SessionStarted,
    SessionUpdated,
    SessionExited,
    StdoutChunk,
    StdinChunk,
    ResizeApplied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportEnvelope {
    pub protocol_version: ProtocolVersion,
    pub message_id: MessageId,
    pub timestamp_unix_ms: u128,
    pub sender: SenderIdentity,
    pub correlation_id: Option<MessageId>,
    pub session_address: Option<SessionAddress>,
    pub console_id: Option<String>,
    pub payload: TransportPayload,
}

impl TransportEnvelope {
    pub fn kind(&self) -> TransportMessageKind {
        self.payload.kind()
    }

    pub fn validate(&self) -> Result<(), TransportError> {
        if !self.protocol_version.is_supported() {
            return Err(TransportError::UnsupportedProtocolVersion(
                self.protocol_version,
            ));
        }

        if let Some(address) = self.payload.session_address() {
            if self.session_address.as_ref() != Some(address) {
                return Err(TransportError::SessionAddressMismatch);
            }
        }

        if let Some(console_id) = self.payload.console_id() {
            if self.console_id.as_deref() != Some(console_id) {
                return Err(TransportError::ConsoleIdMismatch);
            }
        }

        Ok(())
    }
}

pub fn write_transport_envelope(
    writer: &mut impl Write,
    envelope: &TransportEnvelope,
) -> Result<(), TransportError> {
    envelope.validate()?;

    write_u16(writer, envelope.protocol_version.value())?;
    write_u8(writer, message_kind_code(envelope.kind()))?;
    write_u64(writer, envelope.message_id.value())?;
    write_u128(writer, envelope.timestamp_unix_ms)?;
    write_sender_identity(writer, &envelope.sender)?;
    write_optional_message_id(writer, envelope.correlation_id)?;
    write_optional_session_address(writer, envelope.session_address.as_ref())?;
    write_optional_string(writer, envelope.console_id.as_deref())?;
    write_payload(writer, &envelope.payload)?;
    writer.flush().map_err(|error| {
        TransportError::Io("failed to flush transport envelope".to_string(), error)
    })
}

pub fn read_transport_envelope(
    reader: &mut impl Read,
) -> Result<TransportEnvelope, TransportError> {
    let protocol_version = ProtocolVersion::new(read_u16(reader)?);
    let kind = decode_message_kind(read_u8(reader)?)?;
    let message_id = MessageId::new(read_u64(reader)?);
    let timestamp_unix_ms = read_u128(reader)?;
    let sender = read_sender_identity(reader)?;
    let correlation_id = read_optional_message_id(reader)?;
    let session_address = read_optional_session_address(reader)?;
    let console_id = read_optional_string(reader)?;
    let payload = read_payload(reader, kind)?;
    let envelope = TransportEnvelope {
        protocol_version,
        message_id,
        timestamp_unix_ms,
        sender,
        correlation_id,
        session_address,
        console_id,
        payload,
    };
    envelope.validate()?;
    Ok(envelope)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportPayload {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    Heartbeat(Heartbeat),
    SessionStarted(SessionStarted),
    SessionUpdated(SessionUpdated),
    SessionExited(SessionExited),
    StdoutChunk(StdoutChunk),
    StdinChunk(StdinChunk),
    ResizeApplied(ResizeApplied),
}

impl TransportPayload {
    pub fn kind(&self) -> TransportMessageKind {
        match self {
            Self::ClientHello(_) => TransportMessageKind::ClientHello,
            Self::ServerHello(_) => TransportMessageKind::ServerHello,
            Self::Heartbeat(_) => TransportMessageKind::Heartbeat,
            Self::SessionStarted(_) => TransportMessageKind::SessionStarted,
            Self::SessionUpdated(_) => TransportMessageKind::SessionUpdated,
            Self::SessionExited(_) => TransportMessageKind::SessionExited,
            Self::StdoutChunk(_) => TransportMessageKind::StdoutChunk,
            Self::StdinChunk(_) => TransportMessageKind::StdinChunk,
            Self::ResizeApplied(_) => TransportMessageKind::ResizeApplied,
        }
    }

    pub fn session_address(&self) -> Option<&SessionAddress> {
        match self {
            Self::SessionStarted(message) => Some(&message.address),
            Self::SessionUpdated(message) => Some(&message.address),
            Self::SessionExited(message) => Some(&message.address),
            Self::StdoutChunk(message) => Some(&message.address),
            Self::StdinChunk(message) => Some(&message.address),
            Self::ResizeApplied(message) => Some(&message.address),
            _ => None,
        }
    }

    pub fn console_id(&self) -> Option<&str> {
        match self {
            Self::StdinChunk(message) => Some(message.console_id.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub node_id: String,
    pub client_version: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    pub server_version: String,
    pub accepted_protocol_version: ProtocolVersion,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub node_id: String,
    pub session_count: usize,
    pub last_local_event_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStarted {
    pub address: SessionAddress,
    pub title: String,
    pub created_at_unix_ms: u128,
    pub process_id: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUpdated {
    pub address: SessionAddress,
    pub status: String,
    pub last_output_at_unix_ms: Option<u128>,
    pub last_input_at_unix_ms: Option<u128>,
    pub screen_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExited {
    pub address: SessionAddress,
    pub exit_code: Option<i32>,
    pub exited_at_unix_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdoutChunk {
    pub address: SessionAddress,
    pub bytes: Vec<u8>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdinChunk {
    pub address: SessionAddress,
    pub console_id: String,
    pub bytes: Vec<u8>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeApplied {
    pub address: SessionAddress,
    pub cols: u16,
    pub rows: u16,
    pub applied_at_unix_ms: u128,
}

#[derive(Debug)]
pub enum TransportError {
    UnsupportedProtocolVersion(ProtocolVersion),
    SessionAddressMismatch,
    ConsoleIdMismatch,
    UnsupportedPayloadKind(TransportMessageKind),
    InvalidMessageKind(u8),
    InvalidSenderKind(u8),
    InvalidValue(String),
    Io(String, io::Error),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProtocolVersion(version) => {
                write!(f, "unsupported protocol version {}", version.value())
            }
            Self::SessionAddressMismatch => write!(f, "session address does not match payload"),
            Self::ConsoleIdMismatch => write!(f, "console id does not match payload"),
            Self::UnsupportedPayloadKind(kind) => {
                write!(f, "transport codec does not support payload kind {kind:?}")
            }
            Self::InvalidMessageKind(kind) => {
                write!(f, "invalid transport message kind byte {kind}")
            }
            Self::InvalidSenderKind(kind) => write!(f, "invalid transport sender kind byte {kind}"),
            Self::InvalidValue(message) => write!(f, "{message}"),
            Self::Io(context, error) => write!(f, "{context}: {error}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl PartialEq for TransportError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::UnsupportedProtocolVersion(left), Self::UnsupportedProtocolVersion(right)) => {
                left == right
            }
            (Self::SessionAddressMismatch, Self::SessionAddressMismatch) => true,
            (Self::ConsoleIdMismatch, Self::ConsoleIdMismatch) => true,
            (Self::UnsupportedPayloadKind(left), Self::UnsupportedPayloadKind(right)) => {
                left == right
            }
            (Self::InvalidMessageKind(left), Self::InvalidMessageKind(right)) => left == right,
            (Self::InvalidSenderKind(left), Self::InvalidSenderKind(right)) => left == right,
            (Self::InvalidValue(left), Self::InvalidValue(right)) => left == right,
            (Self::Io(left_context, left_error), Self::Io(right_context, right_error)) => {
                left_context == right_context
                    && left_error.kind() == right_error.kind()
                    && left_error.raw_os_error() == right_error.raw_os_error()
            }
            _ => false,
        }
    }
}

impl Eq for TransportError {}

fn message_kind_code(kind: TransportMessageKind) -> u8 {
    match kind {
        TransportMessageKind::ClientHello => 1,
        TransportMessageKind::ServerHello => 2,
        TransportMessageKind::Heartbeat => 3,
        TransportMessageKind::SessionStarted => 4,
        TransportMessageKind::SessionUpdated => 5,
        TransportMessageKind::SessionExited => 6,
        TransportMessageKind::StdoutChunk => 7,
        TransportMessageKind::StdinChunk => 8,
        TransportMessageKind::ResizeApplied => 9,
    }
}

fn decode_message_kind(value: u8) -> Result<TransportMessageKind, TransportError> {
    match value {
        1 => Ok(TransportMessageKind::ClientHello),
        2 => Ok(TransportMessageKind::ServerHello),
        3 => Ok(TransportMessageKind::Heartbeat),
        4 => Ok(TransportMessageKind::SessionStarted),
        5 => Ok(TransportMessageKind::SessionUpdated),
        6 => Ok(TransportMessageKind::SessionExited),
        7 => Ok(TransportMessageKind::StdoutChunk),
        8 => Ok(TransportMessageKind::StdinChunk),
        9 => Ok(TransportMessageKind::ResizeApplied),
        other => Err(TransportError::InvalidMessageKind(other)),
    }
}

fn write_sender_identity(
    writer: &mut impl Write,
    sender: &SenderIdentity,
) -> Result<(), TransportError> {
    match sender {
        SenderIdentity::ClientNode { node_id } => {
            write_u8(writer, 1)?;
            write_string(writer, node_id)?;
        }
        SenderIdentity::Server => {
            write_u8(writer, 2)?;
        }
        SenderIdentity::ConsoleHost { console_id } => {
            write_u8(writer, 3)?;
            write_string(writer, console_id)?;
        }
    }
    Ok(())
}

fn read_sender_identity(reader: &mut impl Read) -> Result<SenderIdentity, TransportError> {
    match read_u8(reader)? {
        1 => Ok(SenderIdentity::ClientNode {
            node_id: read_string(reader)?,
        }),
        2 => Ok(SenderIdentity::Server),
        3 => Ok(SenderIdentity::ConsoleHost {
            console_id: read_string(reader)?,
        }),
        other => Err(TransportError::InvalidSenderKind(other)),
    }
}

fn write_optional_message_id(
    writer: &mut impl Write,
    message_id: Option<MessageId>,
) -> Result<(), TransportError> {
    match message_id {
        Some(value) => {
            write_u8(writer, 1)?;
            write_u64(writer, value.value())?;
        }
        None => write_u8(writer, 0)?,
    }
    Ok(())
}

fn read_optional_message_id(reader: &mut impl Read) -> Result<Option<MessageId>, TransportError> {
    match read_u8(reader)? {
        0 => Ok(None),
        1 => Ok(Some(MessageId::new(read_u64(reader)?))),
        other => Err(TransportError::InvalidValue(format!(
            "invalid optional message id marker {other}"
        ))),
    }
}

fn write_optional_session_address(
    writer: &mut impl Write,
    address: Option<&SessionAddress>,
) -> Result<(), TransportError> {
    match address {
        Some(address) => {
            write_u8(writer, 1)?;
            write_string(writer, address.node_id())?;
            write_string(writer, address.session_id())?;
        }
        None => write_u8(writer, 0)?,
    }
    Ok(())
}

fn read_optional_session_address(
    reader: &mut impl Read,
) -> Result<Option<SessionAddress>, TransportError> {
    match read_u8(reader)? {
        0 => Ok(None),
        1 => Ok(Some(SessionAddress::new(
            read_string(reader)?,
            read_string(reader)?,
        ))),
        other => Err(TransportError::InvalidValue(format!(
            "invalid optional session marker {other}"
        ))),
    }
}

fn write_optional_string(
    writer: &mut impl Write,
    value: Option<&str>,
) -> Result<(), TransportError> {
    match value {
        Some(value) => {
            write_u8(writer, 1)?;
            write_string(writer, value)?;
        }
        None => write_u8(writer, 0)?,
    }
    Ok(())
}

fn read_optional_string(reader: &mut impl Read) -> Result<Option<String>, TransportError> {
    match read_u8(reader)? {
        0 => Ok(None),
        1 => Ok(Some(read_string(reader)?)),
        other => Err(TransportError::InvalidValue(format!(
            "invalid optional string marker {other}"
        ))),
    }
}

fn write_session_address(
    writer: &mut impl Write,
    address: &SessionAddress,
) -> Result<(), TransportError> {
    write_string(writer, address.node_id())?;
    write_string(writer, address.session_id())
}

fn read_session_address(reader: &mut impl Read) -> Result<SessionAddress, TransportError> {
    Ok(SessionAddress::new(
        read_string(reader)?,
        read_string(reader)?,
    ))
}

fn write_optional_u32(writer: &mut impl Write, value: Option<u32>) -> Result<(), TransportError> {
    match value {
        Some(value) => {
            write_u8(writer, 1)?;
            write_u32(writer, value)?;
        }
        None => write_u8(writer, 0)?,
    }
    Ok(())
}

fn read_optional_u32(reader: &mut impl Read) -> Result<Option<u32>, TransportError> {
    match read_u8(reader)? {
        0 => Ok(None),
        1 => Ok(Some(read_u32(reader)?)),
        other => Err(TransportError::InvalidValue(format!(
            "invalid optional u32 marker {other}"
        ))),
    }
}

fn write_optional_u128(writer: &mut impl Write, value: Option<u128>) -> Result<(), TransportError> {
    match value {
        Some(value) => {
            write_u8(writer, 1)?;
            write_u128(writer, value)?;
        }
        None => write_u8(writer, 0)?,
    }
    Ok(())
}

fn read_optional_u128(reader: &mut impl Read) -> Result<Option<u128>, TransportError> {
    match read_u8(reader)? {
        0 => Ok(None),
        1 => Ok(Some(read_u128(reader)?)),
        other => Err(TransportError::InvalidValue(format!(
            "invalid optional u128 marker {other}"
        ))),
    }
}

fn write_optional_i32(writer: &mut impl Write, value: Option<i32>) -> Result<(), TransportError> {
    match value {
        Some(value) => {
            write_u8(writer, 1)?;
            writer.write_all(&value.to_be_bytes()).map_err(|error| {
                TransportError::Io("failed to write transport i32".to_string(), error)
            })?;
        }
        None => write_u8(writer, 0)?,
    }
    Ok(())
}

fn read_optional_i32(reader: &mut impl Read) -> Result<Option<i32>, TransportError> {
    match read_u8(reader)? {
        0 => Ok(None),
        1 => {
            let mut buf = [0_u8; 4];
            reader.read_exact(&mut buf).map_err(|error| {
                TransportError::Io("failed to read transport i32".to_string(), error)
            })?;
            Ok(Some(i32::from_be_bytes(buf)))
        }
        other => Err(TransportError::InvalidValue(format!(
            "invalid optional i32 marker {other}"
        ))),
    }
}

fn write_payload(
    writer: &mut impl Write,
    payload: &TransportPayload,
) -> Result<(), TransportError> {
    match payload {
        TransportPayload::ClientHello(message) => {
            write_string(writer, &message.node_id)?;
            write_string(writer, &message.client_version)?;
            write_string_list(writer, &message.capabilities)?;
        }
        TransportPayload::ServerHello(message) => {
            write_string(writer, &message.server_version)?;
            write_u16(writer, message.accepted_protocol_version.value())?;
            write_string_list(writer, &message.capabilities)?;
        }
        TransportPayload::Heartbeat(message) => {
            write_string(writer, &message.node_id)?;
            write_u64(writer, message.session_count as u64)?;
            match message.last_local_event_id {
                Some(value) => {
                    write_u8(writer, 1)?;
                    write_u64(writer, value)?;
                }
                None => write_u8(writer, 0)?,
            }
        }
        TransportPayload::SessionStarted(message) => {
            write_session_address(writer, &message.address)?;
            write_string(writer, &message.title)?;
            write_u128(writer, message.created_at_unix_ms)?;
            write_optional_u32(writer, message.process_id)?;
        }
        TransportPayload::SessionUpdated(message) => {
            write_session_address(writer, &message.address)?;
            write_string(writer, &message.status)?;
            write_optional_u128(writer, message.last_output_at_unix_ms)?;
            write_optional_u128(writer, message.last_input_at_unix_ms)?;
            write_u64(writer, message.screen_version)?;
        }
        TransportPayload::SessionExited(message) => {
            write_session_address(writer, &message.address)?;
            write_optional_i32(writer, message.exit_code)?;
            write_u128(writer, message.exited_at_unix_ms)?;
        }
        _ => return Err(TransportError::UnsupportedPayloadKind(payload.kind())),
    }
    Ok(())
}

fn read_payload(
    reader: &mut impl Read,
    kind: TransportMessageKind,
) -> Result<TransportPayload, TransportError> {
    match kind {
        TransportMessageKind::ClientHello => Ok(TransportPayload::ClientHello(ClientHello {
            node_id: read_string(reader)?,
            client_version: read_string(reader)?,
            capabilities: read_string_list(reader)?,
        })),
        TransportMessageKind::ServerHello => Ok(TransportPayload::ServerHello(ServerHello {
            server_version: read_string(reader)?,
            accepted_protocol_version: ProtocolVersion::new(read_u16(reader)?),
            capabilities: read_string_list(reader)?,
        })),
        TransportMessageKind::Heartbeat => {
            let node_id = read_string(reader)?;
            let session_count_raw = read_u64(reader)?;
            let session_count = usize::try_from(session_count_raw).map_err(|_| {
                TransportError::InvalidValue(format!(
                    "session count {session_count_raw} does not fit in usize"
                ))
            })?;
            let last_local_event_id = match read_u8(reader)? {
                0 => None,
                1 => Some(read_u64(reader)?),
                other => {
                    return Err(TransportError::InvalidValue(format!(
                        "invalid heartbeat event marker {other}"
                    )))
                }
            };
            Ok(TransportPayload::Heartbeat(Heartbeat {
                node_id,
                session_count,
                last_local_event_id,
            }))
        }
        TransportMessageKind::SessionStarted => {
            Ok(TransportPayload::SessionStarted(SessionStarted {
                address: read_session_address(reader)?,
                title: read_string(reader)?,
                created_at_unix_ms: read_u128(reader)?,
                process_id: read_optional_u32(reader)?,
            }))
        }
        TransportMessageKind::SessionUpdated => {
            Ok(TransportPayload::SessionUpdated(SessionUpdated {
                address: read_session_address(reader)?,
                status: read_string(reader)?,
                last_output_at_unix_ms: read_optional_u128(reader)?,
                last_input_at_unix_ms: read_optional_u128(reader)?,
                screen_version: read_u64(reader)?,
            }))
        }
        TransportMessageKind::SessionExited => Ok(TransportPayload::SessionExited(SessionExited {
            address: read_session_address(reader)?,
            exit_code: read_optional_i32(reader)?,
            exited_at_unix_ms: read_u128(reader)?,
        })),
        other => Err(TransportError::UnsupportedPayloadKind(other)),
    }
}

fn write_string_list(writer: &mut impl Write, values: &[String]) -> Result<(), TransportError> {
    write_u32(writer, values.len() as u32)?;
    for value in values {
        write_string(writer, value)?;
    }
    Ok(())
}

fn read_string_list(reader: &mut impl Read) -> Result<Vec<String>, TransportError> {
    let len = read_u32(reader)? as usize;
    let mut values = Vec::with_capacity(len);
    for _ in 0..len {
        values.push(read_string(reader)?);
    }
    Ok(values)
}

fn read_u8(reader: &mut impl Read) -> Result<u8, TransportError> {
    let mut buf = [0_u8; 1];
    reader
        .read_exact(&mut buf)
        .map_err(|error| TransportError::Io("failed to read transport byte".to_string(), error))?;
    Ok(buf[0])
}

fn write_u8(writer: &mut impl Write, value: u8) -> Result<(), TransportError> {
    writer
        .write_all(&[value])
        .map_err(|error| TransportError::Io("failed to write transport byte".to_string(), error))
}

fn read_u16(reader: &mut impl Read) -> Result<u16, TransportError> {
    let mut buf = [0_u8; 2];
    reader
        .read_exact(&mut buf)
        .map_err(|error| TransportError::Io("failed to read transport u16".to_string(), error))?;
    Ok(u16::from_be_bytes(buf))
}

fn write_u16(writer: &mut impl Write, value: u16) -> Result<(), TransportError> {
    writer
        .write_all(&value.to_be_bytes())
        .map_err(|error| TransportError::Io("failed to write transport u16".to_string(), error))
}

fn read_u32(reader: &mut impl Read) -> Result<u32, TransportError> {
    let mut buf = [0_u8; 4];
    reader
        .read_exact(&mut buf)
        .map_err(|error| TransportError::Io("failed to read transport u32".to_string(), error))?;
    Ok(u32::from_be_bytes(buf))
}

fn write_u32(writer: &mut impl Write, value: u32) -> Result<(), TransportError> {
    writer
        .write_all(&value.to_be_bytes())
        .map_err(|error| TransportError::Io("failed to write transport u32".to_string(), error))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, TransportError> {
    let mut buf = [0_u8; 8];
    reader
        .read_exact(&mut buf)
        .map_err(|error| TransportError::Io("failed to read transport u64".to_string(), error))?;
    Ok(u64::from_be_bytes(buf))
}

fn write_u64(writer: &mut impl Write, value: u64) -> Result<(), TransportError> {
    writer
        .write_all(&value.to_be_bytes())
        .map_err(|error| TransportError::Io("failed to write transport u64".to_string(), error))
}

fn read_u128(reader: &mut impl Read) -> Result<u128, TransportError> {
    let mut buf = [0_u8; 16];
    reader
        .read_exact(&mut buf)
        .map_err(|error| TransportError::Io("failed to read transport u128".to_string(), error))?;
    Ok(u128::from_be_bytes(buf))
}

fn write_u128(writer: &mut impl Write, value: u128) -> Result<(), TransportError> {
    writer
        .write_all(&value.to_be_bytes())
        .map_err(|error| TransportError::Io("failed to write transport u128".to_string(), error))
}

fn read_string(reader: &mut impl Read) -> Result<String, TransportError> {
    let len = read_u32(reader)? as usize;
    let mut buf = vec![0_u8; len];
    reader.read_exact(&mut buf).map_err(|error| {
        TransportError::Io("failed to read transport string".to_string(), error)
    })?;
    String::from_utf8(buf)
        .map_err(|error| TransportError::InvalidValue(format!("invalid transport utf-8: {error}")))
}

fn write_string(writer: &mut impl Write, value: &str) -> Result<(), TransportError> {
    write_u32(writer, value.len() as u32)?;
    writer
        .write_all(value.as_bytes())
        .map_err(|error| TransportError::Io("failed to write transport string".to_string(), error))
}

#[cfg(test)]
mod tests {
    use super::{
        read_transport_envelope, write_transport_envelope, ClientHello, Heartbeat, MessageId,
        ProtocolVersion, SenderIdentity, ServerHello, SessionExited, SessionStarted,
        SessionUpdated, StdinChunk, StdoutChunk, TransportEnvelope, TransportError,
        TransportMessageKind, TransportPayload,
    };
    use crate::session::SessionAddress;
    use std::io::Cursor;

    #[test]
    fn validates_supported_protocol_and_message_kind() {
        let address = SessionAddress::new("node-a", "session-1");
        let envelope = TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id: MessageId::new(7),
            timestamp_unix_ms: 123,
            sender: SenderIdentity::ClientNode {
                node_id: "node-a".to_string(),
            },
            correlation_id: None,
            session_address: Some(address.clone()),
            console_id: None,
            payload: TransportPayload::SessionStarted(SessionStarted {
                address,
                title: "claude".to_string(),
                created_at_unix_ms: 123,
                process_id: Some(55),
            }),
        };

        assert_eq!(envelope.kind(), TransportMessageKind::SessionStarted);
        assert!(envelope.validate().is_ok());
    }

    #[test]
    fn rejects_unsupported_protocol_version() {
        let envelope = TransportEnvelope {
            protocol_version: ProtocolVersion::new(99),
            message_id: MessageId::new(1),
            timestamp_unix_ms: 1,
            sender: SenderIdentity::Server,
            correlation_id: None,
            session_address: None,
            console_id: None,
            payload: TransportPayload::ClientHello(ClientHello {
                node_id: "node-a".to_string(),
                client_version: "0.1.0".to_string(),
                capabilities: vec!["session-stream".to_string()],
            }),
        };

        assert_eq!(
            envelope.validate(),
            Err(TransportError::UnsupportedProtocolVersion(
                ProtocolVersion::new(99)
            ))
        );
    }

    #[test]
    fn rejects_session_or_console_mismatch() {
        let address = SessionAddress::new("node-a", "session-1");
        let stdout = TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id: MessageId::new(2),
            timestamp_unix_ms: 1,
            sender: SenderIdentity::Server,
            correlation_id: None,
            session_address: Some(SessionAddress::new("node-a", "other")),
            console_id: None,
            payload: TransportPayload::StdoutChunk(StdoutChunk {
                address: address.clone(),
                bytes: b"hello".to_vec(),
                sequence: 3,
            }),
        };
        let stdin = TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id: MessageId::new(3),
            timestamp_unix_ms: 2,
            sender: SenderIdentity::ConsoleHost {
                console_id: "console-1".to_string(),
            },
            correlation_id: None,
            session_address: Some(address.clone()),
            console_id: Some("console-2".to_string()),
            payload: TransportPayload::StdinChunk(StdinChunk {
                address,
                console_id: "console-1".to_string(),
                bytes: b"input".to_vec(),
                sequence: 4,
            }),
        };

        assert_eq!(
            stdout.validate(),
            Err(TransportError::SessionAddressMismatch)
        );
        assert_eq!(stdin.validate(), Err(TransportError::ConsoleIdMismatch));
    }

    #[test]
    fn round_trips_transport_envelopes_for_registration_subset() {
        let envelopes = vec![
            TransportEnvelope {
                protocol_version: ProtocolVersion::current(),
                message_id: MessageId::new(1),
                timestamp_unix_ms: 10,
                sender: SenderIdentity::ClientNode {
                    node_id: "node-a".to_string(),
                },
                correlation_id: None,
                session_address: None,
                console_id: None,
                payload: TransportPayload::ClientHello(ClientHello {
                    node_id: "node-a".to_string(),
                    client_version: "0.1.0".to_string(),
                    capabilities: vec!["heartbeat".to_string()],
                }),
            },
            TransportEnvelope {
                protocol_version: ProtocolVersion::current(),
                message_id: MessageId::new(2),
                timestamp_unix_ms: 11,
                sender: SenderIdentity::Server,
                correlation_id: Some(MessageId::new(1)),
                session_address: None,
                console_id: None,
                payload: TransportPayload::ServerHello(ServerHello {
                    server_version: "0.1.0".to_string(),
                    accepted_protocol_version: ProtocolVersion::current(),
                    capabilities: vec!["node-registration".to_string()],
                }),
            },
            TransportEnvelope {
                protocol_version: ProtocolVersion::current(),
                message_id: MessageId::new(3),
                timestamp_unix_ms: 12,
                sender: SenderIdentity::ClientNode {
                    node_id: "node-a".to_string(),
                },
                correlation_id: Some(MessageId::new(1)),
                session_address: None,
                console_id: None,
                payload: TransportPayload::Heartbeat(Heartbeat {
                    node_id: "node-a".to_string(),
                    session_count: 3,
                    last_local_event_id: Some(99),
                }),
            },
            TransportEnvelope {
                protocol_version: ProtocolVersion::current(),
                message_id: MessageId::new(4),
                timestamp_unix_ms: 13,
                sender: SenderIdentity::ClientNode {
                    node_id: "node-a".to_string(),
                },
                correlation_id: Some(MessageId::new(1)),
                session_address: Some(SessionAddress::new("node-a", "session-1")),
                console_id: None,
                payload: TransportPayload::SessionStarted(SessionStarted {
                    address: SessionAddress::new("node-a", "session-1"),
                    title: "claude".to_string(),
                    created_at_unix_ms: 13,
                    process_id: Some(42),
                }),
            },
            TransportEnvelope {
                protocol_version: ProtocolVersion::current(),
                message_id: MessageId::new(5),
                timestamp_unix_ms: 14,
                sender: SenderIdentity::ClientNode {
                    node_id: "node-a".to_string(),
                },
                correlation_id: Some(MessageId::new(1)),
                session_address: Some(SessionAddress::new("node-a", "session-1")),
                console_id: None,
                payload: TransportPayload::SessionUpdated(SessionUpdated {
                    address: SessionAddress::new("node-a", "session-1"),
                    status: "running".to_string(),
                    last_output_at_unix_ms: Some(14),
                    last_input_at_unix_ms: Some(12),
                    screen_version: 7,
                }),
            },
            TransportEnvelope {
                protocol_version: ProtocolVersion::current(),
                message_id: MessageId::new(6),
                timestamp_unix_ms: 15,
                sender: SenderIdentity::ClientNode {
                    node_id: "node-a".to_string(),
                },
                correlation_id: Some(MessageId::new(1)),
                session_address: Some(SessionAddress::new("node-a", "session-1")),
                console_id: None,
                payload: TransportPayload::SessionExited(SessionExited {
                    address: SessionAddress::new("node-a", "session-1"),
                    exit_code: Some(0),
                    exited_at_unix_ms: 15,
                }),
            },
        ];

        let mut bytes = Vec::new();
        for envelope in &envelopes {
            write_transport_envelope(&mut bytes, envelope).expect("envelope should encode");
        }

        let mut reader = Cursor::new(bytes);
        for expected in envelopes {
            let decoded = read_transport_envelope(&mut reader).expect("envelope should decode");
            assert_eq!(decoded, expected);
        }
    }
}
