#![allow(dead_code)]

use crate::event::{EventBus, EventBusMessage, EventGroup};
use crate::transport::{
    ClientHello, ConnectionId, Heartbeat, MessageId, ProtocolVersion, SenderIdentity, ServerHello,
    TransportEnvelope, TransportError, TransportMessageKind, TransportPayload,
};
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct ServerRuntimeConfig {
    pub listen_addr: String,
    pub server_version: String,
    pub capabilities: Vec<String>,
    pub heartbeat_timeout: Duration,
}

impl ServerRuntimeConfig {
    pub fn new(listen_addr: impl Into<String>) -> Self {
        Self {
            listen_addr: listen_addr.into(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: vec![
                "delegated-spawn".to_string(),
                "node-registration".to_string(),
            ],
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
        }
    }
}

#[derive(Debug)]
pub struct AcceptedConnection {
    pub connection_id: ConnectionId,
    pub peer_addr: SocketAddr,
    pub stream: TcpStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeConnectionStatus {
    Online,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    pub node_id: String,
    pub connection_status: NodeConnectionStatus,
    pub last_connection_id: ConnectionId,
    pub peer_addr: SocketAddr,
    pub client_version: String,
    pub capabilities: Vec<String>,
    pub registered_at_unix_ms: u128,
    pub last_heartbeat_at_unix_ms: u128,
    pub last_session_count: usize,
}

#[derive(Debug)]
pub struct NodeRegistry {
    nodes: HashMap<String, NodeRecord>,
    heartbeat_timeout: Duration,
}

impl NodeRegistry {
    pub fn new(heartbeat_timeout: Duration) -> Self {
        Self {
            nodes: HashMap::new(),
            heartbeat_timeout,
        }
    }

    pub fn register_client(
        &mut self,
        connection_id: ConnectionId,
        peer_addr: SocketAddr,
        hello: ClientHello,
        registered_at_unix_ms: u128,
    ) -> &NodeRecord {
        let node_id = hello.node_id;
        let record = NodeRecord {
            node_id: node_id.clone(),
            connection_status: NodeConnectionStatus::Online,
            last_connection_id: connection_id,
            peer_addr,
            client_version: hello.client_version,
            capabilities: hello.capabilities,
            registered_at_unix_ms,
            last_heartbeat_at_unix_ms: registered_at_unix_ms,
            last_session_count: 0,
        };
        self.nodes.insert(node_id.clone(), record);
        self.nodes
            .get(&node_id)
            .expect("registered node should exist")
    }

    pub fn record_heartbeat(
        &mut self,
        node_id: &str,
        connection_id: ConnectionId,
        peer_addr: SocketAddr,
        heartbeat: Heartbeat,
        heartbeat_at_unix_ms: u128,
    ) -> Result<&NodeRecord, NodeRegistryError> {
        let record = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| NodeRegistryError::UnknownNode(node_id.to_string()))?;
        record.connection_status = NodeConnectionStatus::Online;
        record.last_connection_id = connection_id;
        record.peer_addr = peer_addr;
        record.last_heartbeat_at_unix_ms = heartbeat_at_unix_ms;
        record.last_session_count = heartbeat.session_count;
        Ok(record)
    }

    pub fn expire_stale_nodes(&mut self, now_unix_ms: u128) -> Vec<String> {
        let timeout_ms = self.heartbeat_timeout.as_millis();
        let mut offline = Vec::new();

        for record in self.nodes.values_mut() {
            if matches!(record.connection_status, NodeConnectionStatus::Online)
                && now_unix_ms.saturating_sub(record.last_heartbeat_at_unix_ms) > timeout_ms
            {
                record.connection_status = NodeConnectionStatus::Offline;
                offline.push(record.node_id.clone());
            }
        }

        offline.sort();
        offline
    }

    pub fn get(&self, node_id: &str) -> Option<&NodeRecord> {
        self.nodes.get(node_id)
    }

    pub fn list(&self) -> Vec<&NodeRecord> {
        let mut nodes = self.nodes.values().collect::<Vec<_>>();
        nodes.sort_by(|left, right| left.node_id.cmp(&right.node_id));
        nodes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerRuntimeEvent {
    ConnectionAccepted {
        connection_id: ConnectionId,
        peer_addr: SocketAddr,
    },
    NodeRegistered {
        node_id: String,
        connection_id: ConnectionId,
        peer_addr: SocketAddr,
    },
    NodeHeartbeat {
        node_id: String,
        session_count: usize,
    },
    SessionPublished {
        address: crate::session::SessionAddress,
        title: String,
    },
    SessionUpdated {
        address: crate::session::SessionAddress,
        status: String,
        screen_version: u64,
    },
    SessionExited {
        address: crate::session::SessionAddress,
        exit_code: Option<i32>,
    },
    NodeOffline {
        node_id: String,
    },
}

impl EventBusMessage for ServerRuntimeEvent {
    fn event_group(&self) -> EventGroup {
        EventGroup::Transport
    }
}

#[derive(Debug)]
pub struct ServerRuntime {
    config: ServerRuntimeConfig,
    listener: TcpListener,
    next_connection_id: u64,
    next_message_id: u64,
    nodes: NodeRegistry,
    events: EventBus<ServerRuntimeEvent>,
}

impl ServerRuntime {
    pub fn bind(config: ServerRuntimeConfig) -> Result<Self, ServerRuntimeError> {
        let listener = TcpListener::bind(&config.listen_addr)
            .map_err(|error| ServerRuntimeError::Bind(config.listen_addr.clone(), error))?;
        listener
            .set_nonblocking(true)
            .map_err(ServerRuntimeError::ConfigureNonBlocking)?;

        let heartbeat_timeout = config.heartbeat_timeout;
        Ok(Self {
            config,
            listener,
            next_connection_id: 0,
            next_message_id: 0,
            nodes: NodeRegistry::new(heartbeat_timeout),
            events: EventBus::new(),
        })
    }

    pub fn listen_addr(&self) -> &str {
        &self.config.listen_addr
    }

    pub fn accept_pending(&mut self) -> Result<Vec<AcceptedConnection>, ServerRuntimeError> {
        let mut accepted = Vec::new();

        loop {
            match self.listener.accept() {
                Ok((stream, peer_addr)) => {
                    self.next_connection_id += 1;
                    let connection_id =
                        ConnectionId::new(format!("conn-{}", self.next_connection_id));
                    self.events.publish(ServerRuntimeEvent::ConnectionAccepted {
                        connection_id: connection_id.clone(),
                        peer_addr,
                    });
                    accepted.push(AcceptedConnection {
                        connection_id,
                        peer_addr,
                        stream,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(ServerRuntimeError::Accept(error)),
            }
        }

        Ok(accepted)
    }

    pub fn apply_transport_envelope(
        &mut self,
        connection_id: &ConnectionId,
        peer_addr: SocketAddr,
        envelope: TransportEnvelope,
    ) -> Result<Option<TransportEnvelope>, ServerRuntimeError> {
        envelope.validate().map_err(ServerRuntimeError::Transport)?;

        match envelope.payload {
            TransportPayload::ClientHello(hello) => {
                let node = self.nodes.register_client(
                    connection_id.clone(),
                    peer_addr,
                    hello,
                    envelope.timestamp_unix_ms,
                );
                let node_id = node.node_id.clone();
                self.events.publish(ServerRuntimeEvent::NodeRegistered {
                    node_id,
                    connection_id: connection_id.clone(),
                    peer_addr,
                });
                Ok(Some(self.server_hello_response(envelope.message_id)))
            }
            TransportPayload::Heartbeat(heartbeat) => {
                let node_id = heartbeat.node_id.clone();
                let node = self.nodes.record_heartbeat(
                    &node_id,
                    connection_id.clone(),
                    peer_addr,
                    heartbeat,
                    envelope.timestamp_unix_ms,
                )?;
                let node_id = node.node_id.clone();
                let session_count = node.last_session_count;
                self.events.publish(ServerRuntimeEvent::NodeHeartbeat {
                    node_id,
                    session_count,
                });
                Ok(None)
            }
            TransportPayload::SessionStarted(session) => {
                self.events.publish(ServerRuntimeEvent::SessionPublished {
                    address: session.address,
                    title: session.title,
                });
                Ok(None)
            }
            TransportPayload::SessionUpdated(session) => {
                self.events.publish(ServerRuntimeEvent::SessionUpdated {
                    address: session.address,
                    status: session.status,
                    screen_version: session.screen_version,
                });
                Ok(None)
            }
            TransportPayload::SessionExited(session) => {
                self.events.publish(ServerRuntimeEvent::SessionExited {
                    address: session.address,
                    exit_code: session.exit_code,
                });
                Ok(None)
            }
            other => Err(ServerRuntimeError::UnsupportedTransportMessage(
                other.kind(),
            )),
        }
    }

    pub fn expire_stale_nodes(&mut self, now_unix_ms: u128) -> usize {
        let offline_nodes = self.nodes.expire_stale_nodes(now_unix_ms);
        for node_id in &offline_nodes {
            self.events.publish(ServerRuntimeEvent::NodeOffline {
                node_id: node_id.clone(),
            });
        }
        offline_nodes.len()
    }

    pub fn nodes(&self) -> Vec<&NodeRecord> {
        self.nodes.list()
    }

    pub fn subscribe_events(
        &mut self,
    ) -> (
        crate::event::SubscriberId,
        std::sync::mpsc::Receiver<crate::event::EventEnvelope<ServerRuntimeEvent>>,
    ) {
        self.events.subscribe()
    }

    fn server_hello_response(&mut self, correlation_id: MessageId) -> TransportEnvelope {
        self.next_message_id += 1;
        TransportEnvelope {
            protocol_version: ProtocolVersion::current(),
            message_id: MessageId::new(self.next_message_id),
            timestamp_unix_ms: 0,
            sender: SenderIdentity::Server,
            correlation_id: Some(correlation_id),
            session_address: None,
            console_id: None,
            payload: TransportPayload::ServerHello(ServerHello {
                server_version: self.config.server_version.clone(),
                accepted_protocol_version: ProtocolVersion::current(),
                capabilities: self.config.capabilities.clone(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRegistryError {
    UnknownNode(String),
}

impl fmt::Display for NodeRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownNode(node_id) => write!(f, "unknown node {node_id}"),
        }
    }
}

impl std::error::Error for NodeRegistryError {}

#[derive(Debug)]
pub enum ServerRuntimeError {
    Bind(String, io::Error),
    ConfigureNonBlocking(io::Error),
    Accept(io::Error),
    Transport(TransportError),
    UnsupportedTransportMessage(TransportMessageKind),
    NodeRegistry(NodeRegistryError),
}

impl fmt::Display for ServerRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(addr, error) => {
                write!(f, "failed to bind server runtime on {addr}: {error}")
            }
            Self::ConfigureNonBlocking(error) => {
                write!(
                    f,
                    "failed to configure server runtime nonblocking mode: {error}"
                )
            }
            Self::Accept(error) => write!(f, "failed to accept incoming client: {error}"),
            Self::Transport(error) => write!(f, "{error}"),
            Self::UnsupportedTransportMessage(kind) => {
                write!(
                    f,
                    "unsupported transport message for server runtime: {kind:?}"
                )
            }
            Self::NodeRegistry(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ServerRuntimeError {}

impl From<NodeRegistryError> for ServerRuntimeError {
    fn from(value: NodeRegistryError) -> Self {
        Self::NodeRegistry(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeConnectionStatus, NodeRegistry, NodeRegistryError};
    use crate::transport::{ClientHello, ConnectionId, Heartbeat};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;

    fn peer_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn registers_nodes_and_updates_heartbeat_liveness() {
        let mut registry = NodeRegistry::new(Duration::from_secs(15));
        let hello = ClientHello {
            node_id: "node-a".to_string(),
            client_version: "0.1.0".to_string(),
            capabilities: vec!["delegated-spawn".to_string()],
        };

        let node =
            registry.register_client(ConnectionId::new("conn-1"), peer_addr(7001), hello, 100);
        assert_eq!(node.connection_status, NodeConnectionStatus::Online);
        assert_eq!(node.last_session_count, 0);

        let node = registry
            .record_heartbeat(
                "node-a",
                ConnectionId::new("conn-1"),
                peer_addr(7001),
                Heartbeat {
                    node_id: "node-a".to_string(),
                    session_count: 4,
                    last_local_event_id: Some(9),
                },
                120,
            )
            .expect("heartbeat should apply");
        assert_eq!(node.last_session_count, 4);
        assert_eq!(node.last_heartbeat_at_unix_ms, 120);
    }

    #[test]
    fn expires_stale_nodes_to_offline() {
        let mut registry = NodeRegistry::new(Duration::from_secs(15));
        registry.register_client(
            ConnectionId::new("conn-1"),
            peer_addr(7001),
            ClientHello {
                node_id: "node-a".to_string(),
                client_version: "0.1.0".to_string(),
                capabilities: vec![],
            },
            100,
        );

        let expired = registry.expire_stale_nodes(16_000);
        assert_eq!(expired, vec!["node-a".to_string()]);
        assert_eq!(
            registry
                .get("node-a")
                .expect("node should exist")
                .connection_status,
            NodeConnectionStatus::Offline
        );
    }

    #[test]
    fn rejects_heartbeat_for_unknown_node() {
        let mut registry = NodeRegistry::new(Duration::from_secs(15));
        let error = registry
            .record_heartbeat(
                "missing",
                ConnectionId::new("conn-1"),
                peer_addr(7001),
                Heartbeat {
                    node_id: "missing".to_string(),
                    session_count: 1,
                    last_local_event_id: None,
                },
                200,
            )
            .expect_err("unknown nodes should be rejected");
        assert_eq!(error, NodeRegistryError::UnknownNode("missing".to_string()));
    }
}
