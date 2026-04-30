use crate::infra::remote_protocol::{
    ClientHelloPayload, ControlPlanePayload, ProtocolEnvelope, ServerHelloPayload,
    REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, write_control_plane_envelope,
};
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

pub const NODE_TRANSPORT_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NODE_TRANSPORT_SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NODE_TRANSPORT_HEARTBEAT_INTERVAL_MS: u64 = 5_000;
pub const NODE_TRANSPORT_SESSION_RECOVERY_POLICY: &str = "republish_live_targets";

pub fn write_client_hello(writer: &mut impl io::Write, node_id: &str) -> io::Result<()> {
    let payload = ControlPlanePayload::ClientHello(ClientHelloPayload {
        node_id: node_id.to_string(),
        client_version: NODE_TRANSPORT_CLIENT_VERSION.to_string(),
    });
    write_control_plane_envelope(
        writer,
        &ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("{node_id}-client-hello-1"),
            message_type: payload.message_type(),
            timestamp: now_rfc3339_like(),
            sender_id: node_id.to_string(),
            correlation_id: None,
            session_id: None,
            target_id: None,
            attachment_id: None,
            console_id: None,
            payload,
        },
    )
    .map_err(to_io_error)
}

pub fn read_client_hello(reader: &mut impl io::Read) -> io::Result<String> {
    let envelope = read_control_plane_envelope(reader).map_err(to_io_error)?;
    match envelope.payload {
        ControlPlanePayload::ClientHello(payload) => {
            if payload.node_id != envelope.sender_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "node transport hello sender `{}` does not match node `{}`",
                        envelope.sender_id, payload.node_id
                    ),
                ));
            }
            Ok(payload.node_id)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected node transport handshake `{}`",
                other.message_type()
            ),
        )),
    }
}

pub fn write_server_hello(writer: &mut impl io::Write, server_id: &str) -> io::Result<()> {
    let payload = ControlPlanePayload::ServerHello(ServerHelloPayload {
        server_id: server_id.to_string(),
        server_version: NODE_TRANSPORT_SERVER_VERSION.to_string(),
        accepted_protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        heartbeat_interval_ms: NODE_TRANSPORT_HEARTBEAT_INTERVAL_MS,
        session_recovery_policy: NODE_TRANSPORT_SESSION_RECOVERY_POLICY,
    });
    write_control_plane_envelope(
        writer,
        &ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("{server_id}-server-hello-1"),
            message_type: payload.message_type(),
            timestamp: now_rfc3339_like(),
            sender_id: server_id.to_string(),
            correlation_id: None,
            session_id: None,
            target_id: None,
            attachment_id: None,
            console_id: None,
            payload,
        },
    )
    .map_err(to_io_error)
}

pub fn read_server_hello(reader: &mut impl io::Read) -> io::Result<ServerHelloPayload> {
    let envelope = read_control_plane_envelope(reader).map_err(to_io_error)?;
    match envelope.payload {
        ControlPlanePayload::ServerHello(payload) => {
            if payload.server_id != envelope.sender_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "node transport hello sender `{}` does not match server `{}`",
                        envelope.sender_id, payload.server_id
                    ),
                ));
            }
            if payload.accepted_protocol_version != REMOTE_PROTOCOL_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "node transport protocol mismatch: server accepted `{}`",
                        payload.accepted_protocol_version
                    ),
                ));
            }
            Ok(payload)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected node transport handshake `{}`",
                other.message_type()
            ),
        )),
    }
}

fn now_rfc3339_like() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
}

fn to_io_error(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error.to_string())
}
