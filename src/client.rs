#![allow(dead_code)]

use crate::event::{EventBus, EventBusMessage, EventGroup};
use crate::session::{SessionAddress, SessionRecord, SessionStatus};
use crate::transport::{
    read_transport_envelope, write_transport_envelope, ClientHello, ConnectionId, Heartbeat,
    MessageId, ProtocolVersion, SenderIdentity, ServerHello, SessionExited, SessionStarted,
    SessionUpdated, TransportEnvelope, TransportError, TransportPayload,
};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

const CONTROL_PROTOCOL_VERSION: u8 = 1;
const CONTROL_OP_SPAWN: u8 = 1;

#[derive(Debug, Clone)]
pub struct ClientRuntimeConfig {
    pub endpoint: String,
    pub node_id: String,
    pub client_version: String,
    pub capabilities: Vec<String>,
}

impl ClientRuntimeConfig {
    pub fn new(endpoint: impl Into<String>, node_id: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            node_id: node_id.into(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: vec!["delegated-spawn".to_string(), "heartbeat".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedSpawnRequest {
    pub node_id: String,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedSpawnAcceptance {
    pub session_address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRuntimeEvent {
    Connected {
        connection_id: ConnectionId,
        endpoint: String,
    },
    TransportPrepared {
        connection_id: ConnectionId,
        envelope: TransportEnvelope,
    },
    SpawnDelegated {
        connection_id: ConnectionId,
        session_address: String,
    },
    NodeRegistered {
        connection_id: ConnectionId,
        node_id: String,
        server_version: String,
    },
    HeartbeatSent {
        connection_id: ConnectionId,
        node_id: String,
        session_count: usize,
    },
    SessionPublished {
        connection_id: ConnectionId,
        address: SessionAddress,
        kind: &'static str,
    },
}

impl EventBusMessage for ClientRuntimeEvent {
    fn event_group(&self) -> EventGroup {
        EventGroup::Transport
    }
}

#[derive(Debug)]
pub struct ClientRuntime {
    config: ClientRuntimeConfig,
    connection_id: ConnectionId,
    stream: Option<TcpStream>,
    next_message_id: u64,
    last_hello_message_id: Option<MessageId>,
    events: EventBus<ClientRuntimeEvent>,
}

impl ClientRuntime {
    pub fn connect(config: ClientRuntimeConfig) -> Result<Self, ClientRuntimeError> {
        let normalized_endpoint = normalize_endpoint(&config.endpoint);
        let stream = TcpStream::connect(&normalized_endpoint).map_err(|error| {
            ClientRuntimeError::Io(
                format!("failed to connect to WaitAgent server at {normalized_endpoint}"),
                error,
            )
        })?;

        Ok(Self::build(config, normalized_endpoint, Some(stream)))
    }

    #[cfg(test)]
    fn new_for_test(config: ClientRuntimeConfig) -> Self {
        let normalized_endpoint = normalize_endpoint(&config.endpoint);
        Self::build(config, normalized_endpoint, None)
    }

    fn build(
        config: ClientRuntimeConfig,
        normalized_endpoint: String,
        stream: Option<TcpStream>,
    ) -> Self {
        let connection_id = ConnectionId::new(format!("{}-{}", config.node_id, now_unix_ms()));
        let mut events = EventBus::new();
        events.publish(ClientRuntimeEvent::Connected {
            connection_id: connection_id.clone(),
            endpoint: normalized_endpoint.clone(),
        });

        Self {
            config: ClientRuntimeConfig {
                endpoint: normalized_endpoint,
                ..config
            },
            connection_id,
            stream,
            next_message_id: 0,
            last_hello_message_id: None,
            events,
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    pub fn connection_id(&self) -> &ConnectionId {
        &self.connection_id
    }

    pub fn prepare_client_hello(&mut self) -> Result<TransportEnvelope, ClientRuntimeError> {
        let message_id = self.next_message_id();
        let envelope = TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id,
            timestamp_unix_ms: now_unix_ms(),
            sender: SenderIdentity::ClientNode {
                node_id: self.config.node_id.clone(),
            },
            correlation_id: None,
            session_address: None,
            console_id: None,
            payload: TransportPayload::ClientHello(ClientHello {
                node_id: self.config.node_id.clone(),
                client_version: self.config.client_version.clone(),
                capabilities: self.config.capabilities.clone(),
            }),
        };
        envelope.validate().map_err(ClientRuntimeError::Transport)?;
        self.last_hello_message_id = Some(message_id);
        self.events.publish(ClientRuntimeEvent::TransportPrepared {
            connection_id: self.connection_id.clone(),
            envelope: envelope.clone(),
        });
        Ok(envelope)
    }

    pub fn prepare_heartbeat(
        &mut self,
        session_count: usize,
        last_local_event_id: Option<u64>,
    ) -> Result<TransportEnvelope, ClientRuntimeError> {
        let envelope = TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id: self.next_message_id(),
            timestamp_unix_ms: now_unix_ms(),
            sender: SenderIdentity::ClientNode {
                node_id: self.config.node_id.clone(),
            },
            correlation_id: self.last_hello_message_id,
            session_address: None,
            console_id: None,
            payload: TransportPayload::Heartbeat(Heartbeat {
                node_id: self.config.node_id.clone(),
                session_count,
                last_local_event_id,
            }),
        };
        envelope.validate().map_err(ClientRuntimeError::Transport)?;
        self.events.publish(ClientRuntimeEvent::TransportPrepared {
            connection_id: self.connection_id.clone(),
            envelope: envelope.clone(),
        });
        Ok(envelope)
    }

    pub fn register_node(
        &mut self,
        session_count: usize,
        last_local_event_id: Option<u64>,
    ) -> Result<ServerHello, ClientRuntimeError> {
        let hello = self.prepare_client_hello()?;
        let server_hello = {
            let stream = self.stream.as_mut().ok_or_else(|| {
                ClientRuntimeError::Protocol(
                    "node registration requires an active tcp stream".to_string(),
                )
            })?;
            write_transport_envelope(stream, &hello).map_err(ClientRuntimeError::Transport)?;
            let envelope =
                read_transport_envelope(stream).map_err(ClientRuntimeError::Transport)?;
            match envelope.payload {
                TransportPayload::ServerHello(message) => message,
                other => {
                    return Err(ClientRuntimeError::Protocol(format!(
                        "expected server hello, received {:?}",
                        other.kind()
                    )))
                }
            }
        };

        self.events.publish(ClientRuntimeEvent::NodeRegistered {
            connection_id: self.connection_id.clone(),
            node_id: self.config.node_id.clone(),
            server_version: server_hello.server_version.clone(),
        });

        let heartbeat = self.prepare_heartbeat(session_count, last_local_event_id)?;
        let stream = self.stream.as_mut().ok_or_else(|| {
            ClientRuntimeError::Protocol("heartbeat requires an active tcp stream".to_string())
        })?;
        write_transport_envelope(stream, &heartbeat).map_err(ClientRuntimeError::Transport)?;
        self.events.publish(ClientRuntimeEvent::HeartbeatSent {
            connection_id: self.connection_id.clone(),
            node_id: self.config.node_id.clone(),
            session_count,
        });

        Ok(server_hello)
    }

    pub fn delegate_spawn(
        &mut self,
        request: &DelegatedSpawnRequest,
    ) -> Result<DelegatedSpawnAcceptance, ClientRuntimeError> {
        let stream = self.stream.as_mut().ok_or_else(|| {
            ClientRuntimeError::Protocol(
                "delegated spawn requires an active tcp stream".to_string(),
            )
        })?;
        write_spawn_request(stream, request)?;
        let acceptance = read_spawn_response(stream)?;
        self.events.publish(ClientRuntimeEvent::SpawnDelegated {
            connection_id: self.connection_id.clone(),
            session_address: acceptance.session_address.clone(),
        });
        Ok(acceptance)
    }

    pub fn prepare_session_started(
        &mut self,
        record: &SessionRecord,
    ) -> Result<TransportEnvelope, ClientRuntimeError> {
        let envelope = TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id: self.next_message_id(),
            timestamp_unix_ms: now_unix_ms(),
            sender: SenderIdentity::ClientNode {
                node_id: self.config.node_id.clone(),
            },
            correlation_id: self.last_hello_message_id,
            session_address: Some(record.address().clone()),
            console_id: None,
            payload: TransportPayload::SessionStarted(SessionStarted {
                address: record.address().clone(),
                title: record.title.clone(),
                created_at_unix_ms: record.created_at_unix_ms,
                process_id: record.process_id,
            }),
        };
        envelope.validate().map_err(ClientRuntimeError::Transport)?;
        Ok(envelope)
    }

    pub fn prepare_session_updated(
        &mut self,
        record: &SessionRecord,
    ) -> Result<TransportEnvelope, ClientRuntimeError> {
        let envelope = TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id: self.next_message_id(),
            timestamp_unix_ms: now_unix_ms(),
            sender: SenderIdentity::ClientNode {
                node_id: self.config.node_id.clone(),
            },
            correlation_id: self.last_hello_message_id,
            session_address: Some(record.address().clone()),
            console_id: None,
            payload: TransportPayload::SessionUpdated(SessionUpdated {
                address: record.address().clone(),
                status: session_status_label(&record.status).to_string(),
                last_output_at_unix_ms: record.last_output_at_unix_ms,
                last_input_at_unix_ms: record.last_input_at_unix_ms,
                screen_version: record.snapshot_version,
            }),
        };
        envelope.validate().map_err(ClientRuntimeError::Transport)?;
        Ok(envelope)
    }

    pub fn prepare_session_exited(
        &mut self,
        address: SessionAddress,
        exit_code: Option<i32>,
        exited_at_unix_ms: u128,
    ) -> Result<TransportEnvelope, ClientRuntimeError> {
        let envelope = TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id: self.next_message_id(),
            timestamp_unix_ms: now_unix_ms(),
            sender: SenderIdentity::ClientNode {
                node_id: self.config.node_id.clone(),
            },
            correlation_id: self.last_hello_message_id,
            session_address: Some(address.clone()),
            console_id: None,
            payload: TransportPayload::SessionExited(SessionExited {
                address,
                exit_code,
                exited_at_unix_ms,
            }),
        };
        envelope.validate().map_err(ClientRuntimeError::Transport)?;
        Ok(envelope)
    }

    pub fn publish_session_started(
        &mut self,
        record: &SessionRecord,
    ) -> Result<(), ClientRuntimeError> {
        let envelope = self.prepare_session_started(record)?;
        self.publish_session_envelope(envelope, "started")
    }

    pub fn publish_session_updated(
        &mut self,
        record: &SessionRecord,
    ) -> Result<(), ClientRuntimeError> {
        let envelope = self.prepare_session_updated(record)?;
        self.publish_session_envelope(envelope, "updated")
    }

    pub fn publish_session_exited(
        &mut self,
        address: SessionAddress,
        exit_code: Option<i32>,
        exited_at_unix_ms: u128,
    ) -> Result<(), ClientRuntimeError> {
        let envelope = self.prepare_session_exited(address, exit_code, exited_at_unix_ms)?;
        self.publish_session_envelope(envelope, "exited")
    }

    pub fn subscribe_events(
        &mut self,
    ) -> (
        crate::event::SubscriberId,
        std::sync::mpsc::Receiver<crate::event::EventEnvelope<ClientRuntimeEvent>>,
    ) {
        self.events.subscribe()
    }

    fn next_message_id(&mut self) -> MessageId {
        self.next_message_id += 1;
        MessageId::new(self.next_message_id)
    }

    fn publish_session_envelope(
        &mut self,
        envelope: TransportEnvelope,
        kind: &'static str,
    ) -> Result<(), ClientRuntimeError> {
        let address = envelope
            .session_address
            .clone()
            .ok_or_else(|| ClientRuntimeError::Protocol("missing session address".to_string()))?;
        let stream = self.stream.as_mut().ok_or_else(|| {
            ClientRuntimeError::Protocol(
                "session publication requires an active tcp stream".to_string(),
            )
        })?;
        write_transport_envelope(stream, &envelope).map_err(ClientRuntimeError::Transport)?;
        self.events.publish(ClientRuntimeEvent::SessionPublished {
            connection_id: self.connection_id.clone(),
            address,
            kind,
        });
        Ok(())
    }
}

fn session_status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::WaitingInput => "waiting_input",
        SessionStatus::Idle => "idle",
        SessionStatus::Exited => "exited",
    }
}

pub fn normalize_endpoint(addr: &str) -> String {
    addr.trim()
        .trim_start_matches("ws://")
        .trim_start_matches("tcp://")
        .to_string()
}

pub fn read_delegated_spawn_request(
    stream: &mut impl Read,
) -> Result<DelegatedSpawnRequest, ClientRuntimeError> {
    let version = read_u8(stream)?;
    if version != CONTROL_PROTOCOL_VERSION {
        return Err(ClientRuntimeError::Protocol(format!(
            "unsupported control protocol version: {version}"
        )));
    }

    let op = read_u8(stream)?;
    if op != CONTROL_OP_SPAWN {
        return Err(ClientRuntimeError::Protocol(format!(
            "unsupported control operation: {op}"
        )));
    }

    let node_id = read_string(stream)?;
    let program = read_string(stream)?;
    let args_len = read_u32(stream)? as usize;
    let mut args = Vec::with_capacity(args_len);
    for _ in 0..args_len {
        args.push(read_string(stream)?);
    }

    Ok(DelegatedSpawnRequest {
        node_id,
        program,
        args,
    })
}

pub fn write_delegated_spawn_response(
    stream: &mut impl Write,
    result: Result<String, String>,
) -> Result<(), ClientRuntimeError> {
    match result {
        Ok(address) => {
            write_u8(stream, 0)?;
            write_string(stream, &address)?;
        }
        Err(message) => {
            write_u8(stream, 1)?;
            write_string(stream, &message)?;
        }
    }
    stream.flush().map_err(|error| {
        ClientRuntimeError::Io("failed to flush spawn response".to_string(), error)
    })?;
    Ok(())
}

fn write_spawn_request(
    stream: &mut impl Write,
    request: &DelegatedSpawnRequest,
) -> Result<(), ClientRuntimeError> {
    write_u8(stream, CONTROL_PROTOCOL_VERSION)?;
    write_u8(stream, CONTROL_OP_SPAWN)?;
    write_string(stream, &request.node_id)?;
    write_string(stream, &request.program)?;
    write_u32(stream, request.args.len() as u32)?;
    for arg in &request.args {
        write_string(stream, arg)?;
    }
    stream.flush().map_err(|error| {
        ClientRuntimeError::Io("failed to flush spawn request".to_string(), error)
    })?;
    Ok(())
}

fn read_spawn_response(
    stream: &mut impl Read,
) -> Result<DelegatedSpawnAcceptance, ClientRuntimeError> {
    let status = read_u8(stream)?;
    let payload = read_string(stream)?;
    if status == 0 {
        Ok(DelegatedSpawnAcceptance {
            session_address: payload,
        })
    } else {
        Err(ClientRuntimeError::Protocol(payload))
    }
}

fn read_u8(stream: &mut impl Read) -> Result<u8, ClientRuntimeError> {
    let mut buf = [0_u8; 1];
    stream.read_exact(&mut buf).map_err(|error| {
        ClientRuntimeError::Io("failed to read control byte".to_string(), error)
    })?;
    Ok(buf[0])
}

fn write_u8(stream: &mut impl Write, value: u8) -> Result<(), ClientRuntimeError> {
    stream
        .write_all(&[value])
        .map_err(|error| ClientRuntimeError::Io("failed to write control byte".to_string(), error))
}

fn read_u32(stream: &mut impl Read) -> Result<u32, ClientRuntimeError> {
    let mut buf = [0_u8; 4];
    stream.read_exact(&mut buf).map_err(|error| {
        ClientRuntimeError::Io("failed to read control length".to_string(), error)
    })?;
    Ok(u32::from_be_bytes(buf))
}

fn write_u32(stream: &mut impl Write, value: u32) -> Result<(), ClientRuntimeError> {
    stream.write_all(&value.to_be_bytes()).map_err(|error| {
        ClientRuntimeError::Io("failed to write control length".to_string(), error)
    })
}

fn read_string(stream: &mut impl Read) -> Result<String, ClientRuntimeError> {
    let len = read_u32(stream)? as usize;
    let mut buf = vec![0_u8; len];
    stream.read_exact(&mut buf).map_err(|error| {
        ClientRuntimeError::Io("failed to read control string".to_string(), error)
    })?;
    String::from_utf8(buf)
        .map_err(|error| ClientRuntimeError::Protocol(format!("invalid utf-8 payload: {error}")))
}

fn write_string(stream: &mut impl Write, value: &str) -> Result<(), ClientRuntimeError> {
    write_u32(stream, value.len() as u32)?;
    stream.write_all(value.as_bytes()).map_err(|error| {
        ClientRuntimeError::Io("failed to write control string".to_string(), error)
    })
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Debug)]
pub enum ClientRuntimeError {
    Io(String, io::Error),
    Protocol(String),
    Transport(TransportError),
}

impl fmt::Display for ClientRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(context, error) => write!(f, "{context}: {error}"),
            Self::Protocol(message) => write!(f, "control protocol error: {message}"),
            Self::Transport(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ClientRuntimeError {}

#[cfg(test)]
mod tests {
    use super::{
        normalize_endpoint, read_delegated_spawn_request, write_delegated_spawn_response,
        ClientRuntime, ClientRuntimeConfig, ClientRuntimeEvent, DelegatedSpawnRequest,
    };
    use crate::session::SessionRegistry;
    use crate::terminal::{TerminalEngine, TerminalSize};
    use crate::transport::TransportPayload;
    use std::io::Cursor;

    #[test]
    fn normalizes_endpoint_schemes() {
        assert_eq!(normalize_endpoint("ws://127.0.0.1:7474"), "127.0.0.1:7474");
        assert_eq!(normalize_endpoint("tcp://127.0.0.1:7474"), "127.0.0.1:7474");
        assert_eq!(normalize_endpoint("127.0.0.1:7474"), "127.0.0.1:7474");
    }

    #[test]
    fn prepares_hello_and_heartbeat_transport_envelopes() {
        let mut runtime =
            ClientRuntime::new_for_test(ClientRuntimeConfig::new("ws://127.0.0.1:7474", "node-a"));
        let (_subscriber_id, rx) = runtime.subscribe_events();

        let hello = runtime
            .prepare_client_hello()
            .expect("client hello should validate");
        let heartbeat = runtime
            .prepare_heartbeat(3, Some(11))
            .expect("heartbeat should validate");

        match hello.payload {
            TransportPayload::ClientHello(message) => {
                assert_eq!(message.node_id, "node-a");
                assert!(message
                    .capabilities
                    .contains(&"delegated-spawn".to_string()));
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        match heartbeat.payload {
            TransportPayload::Heartbeat(message) => {
                assert_eq!(message.node_id, "node-a");
                assert_eq!(message.session_count, 3);
                assert_eq!(message.last_local_event_id, Some(11));
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        let hello_event = rx.recv().expect("hello event should arrive");
        let heartbeat_event = rx.recv().expect("heartbeat event should arrive");
        match hello_event.payload {
            ClientRuntimeEvent::TransportPrepared { envelope, .. } => {
                assert!(matches!(envelope.payload, TransportPayload::ClientHello(_)));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match heartbeat_event.payload {
            ClientRuntimeEvent::TransportPrepared { envelope, .. } => {
                assert!(matches!(envelope.payload, TransportPayload::Heartbeat(_)));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn encodes_and_decodes_temporary_spawn_control_payloads() {
        let request = DelegatedSpawnRequest {
            node_id: "node-a".to_string(),
            program: "claude".to_string(),
            args: vec!["--print".to_string(), "hello".to_string()],
        };

        let mut request_bytes = Vec::new();
        super::write_spawn_request(&mut request_bytes, &request).expect("request should encode");
        let decoded_request = read_delegated_spawn_request(&mut Cursor::new(request_bytes))
            .expect("decode should work");
        assert_eq!(decoded_request, request);

        let mut response_bytes = Vec::new();
        write_delegated_spawn_response(&mut response_bytes, Ok("node-a/session-9".to_string()))
            .expect("response should encode");
        let accepted = super::read_spawn_response(&mut Cursor::new(response_bytes))
            .expect("response should decode");
        assert_eq!(accepted.session_address, "node-a/session-9");
    }

    #[test]
    fn registration_requires_live_stream() {
        let mut runtime =
            ClientRuntime::new_for_test(ClientRuntimeConfig::new("ws://127.0.0.1:7474", "node-a"));

        let error = runtime
            .register_node(0, None)
            .expect_err("test runtime should not register without a stream");
        assert!(error
            .to_string()
            .contains("node registration requires an active tcp stream"));
    }

    #[test]
    fn prepares_session_lifecycle_envelopes() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "node-a".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );
        registry.mark_running(session.address(), Some(42));
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 10,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"hello");
        registry.update_screen_state(session.address(), engine.state());
        registry.mark_output_at(session.address(), 200);
        registry.mark_input_at(session.address(), 150);
        let record = registry
            .get(session.address())
            .expect("session record should exist")
            .clone();

        let mut runtime =
            ClientRuntime::new_for_test(ClientRuntimeConfig::new("ws://127.0.0.1:7474", "node-a"));
        let started = runtime
            .prepare_session_started(&record)
            .expect("session started should validate");
        let updated = runtime
            .prepare_session_updated(&record)
            .expect("session updated should validate");
        let exited = runtime
            .prepare_session_exited(record.address().clone(), Some(0), 300)
            .expect("session exited should validate");

        match started.payload {
            TransportPayload::SessionStarted(message) => {
                assert_eq!(message.address, record.address().clone());
                assert_eq!(message.process_id, Some(42));
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        match updated.payload {
            TransportPayload::SessionUpdated(message) => {
                assert_eq!(message.status, "running");
                assert_eq!(message.last_output_at_unix_ms, Some(200));
                assert_eq!(message.last_input_at_unix_ms, Some(150));
                assert_eq!(message.screen_version, 1);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        match exited.payload {
            TransportPayload::SessionExited(message) => {
                assert_eq!(message.exit_code, Some(0));
                assert_eq!(message.exited_at_unix_ms, 300);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }
}
