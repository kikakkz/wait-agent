use crate::application::target_registry_service::{TargetCatalogGateway, TargetRegistryService};
use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::remote_protocol::{
    ControlPlanePayload, CreateSessionAcceptedPayload, CreateSessionRejectedPayload,
    CreateSessionRequestPayload, NodeSessionChannel, NodeSessionEnvelope, ProtocolEnvelope,
    REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_node_session_envelope, write_node_session_envelope,
};
use crate::runtime::remote_node_ingress_server_runtime::{
    remote_node_ingress_owner_socket_path, RemoteNodeIngressServerRuntime,
};
use std::fmt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CATALOG_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CATALOG_POLL_INTERVAL: Duration = Duration::from_millis(100);

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

pub trait RemoteSessionCreationTransport {
    type Error;

    fn create_session(
        &self,
        request: CreateSessionRequestPayload,
        accept_timeout: Duration,
    ) -> Result<CreateSessionReply, Self::Error>;
}

pub trait RemoteSessionCreationCatalog {
    type Error;

    fn list_targets_on_authority(
        &self,
        authority_id: &str,
    ) -> Result<Vec<ManagedSessionRecord>, Self::Error>;
}

#[derive(Debug, Clone)]
pub struct GrpcRemoteSessionCreationTransport {
    network: RemoteNetworkConfig,
}

impl GrpcRemoteSessionCreationTransport {
    pub fn new(network: RemoteNetworkConfig) -> Self {
        Self { network }
    }
}

impl RemoteSessionCreationTransport for GrpcRemoteSessionCreationTransport {
    type Error = RemoteSessionCreationTransportError;

    fn create_session(
        &self,
        request: CreateSessionRequestPayload,
        accept_timeout: Duration,
    ) -> Result<CreateSessionReply, Self::Error> {
        RemoteNodeIngressServerRuntime::ensure_owner_running("__shared__", &self.network)
            .map_err(|error| RemoteSessionCreationTransportError::new(error.to_string()))?;
        let socket_path = remote_node_ingress_owner_socket_path(&self.network);
        let mut stream = UnixStream::connect(socket_path)
            .map_err(|error| RemoteSessionCreationTransportError::new(error.to_string()))?;
        stream
            .set_read_timeout(Some(accept_timeout))
            .map_err(|error| RemoteSessionCreationTransportError::new(error.to_string()))?;
        write_node_session_envelope(
            &mut stream,
            &NodeSessionEnvelope {
                channel: NodeSessionChannel::Authority,
                envelope: create_session_request_envelope(&request),
            },
        )
        .map_err(|error| RemoteSessionCreationTransportError::new(error.to_string()))?;

        let reply = read_node_session_envelope(&mut stream)
            .map_err(|error| RemoteSessionCreationTransportError::new(error.to_string()))?;
        match reply.envelope.payload {
            ControlPlanePayload::CreateSessionAccepted(payload) => {
                Ok(CreateSessionReply::Accepted(payload))
            }
            ControlPlanePayload::CreateSessionRejected(payload) => {
                Ok(CreateSessionReply::Rejected(payload))
            }
            other => Err(RemoteSessionCreationTransportError::new(format!(
                "unexpected create-session reply payload `{}`",
                other.message_type()
            ))),
        }
    }
}

fn create_session_request_envelope(
    request: &CreateSessionRequestPayload,
) -> ProtocolEnvelope<ControlPlanePayload> {
    ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: format!("local-create-session-{}", next_request_id()),
        message_type: "create_session_request",
        timestamp: format!("{}Z", now_millis()),
        sender_id: "waitagent-local-create-session".to_string(),
        correlation_id: Some(request.request_id.clone()),
        session_id: None,
        target_id: None,
        attachment_id: None,
        console_id: None,
        payload: ControlPlanePayload::CreateSessionRequest(request.clone()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionCreationTransportError {
    message: String,
}

impl RemoteSessionCreationTransportError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteSessionCreationTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteSessionCreationTransportError {}

impl<G> RemoteSessionCreationCatalog for TargetRegistryService<G>
where
    G: TargetCatalogGateway,
{
    type Error = G::Error;

    fn list_targets_on_authority(
        &self,
        authority_id: &str,
    ) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        TargetRegistryService::list_targets_on_authority(self, authority_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateSessionReply {
    Accepted(CreateSessionAcceptedPayload),
    Rejected(CreateSessionRejectedPayload),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionCreationRequest {
    pub authority_node_id: String,
    pub cwd_hint: Option<PathBuf>,
    pub cols: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionCreationConfig {
    pub accept_timeout: Duration,
    pub catalog_timeout: Duration,
    pub catalog_poll_interval: Duration,
}

impl Default for RemoteSessionCreationConfig {
    fn default() -> Self {
        Self {
            accept_timeout: DEFAULT_ACCEPT_TIMEOUT,
            catalog_timeout: DEFAULT_CATALOG_TIMEOUT,
            catalog_poll_interval: DEFAULT_CATALOG_POLL_INTERVAL,
        }
    }
}

pub struct RemoteSessionCreationService<T, C> {
    transport: T,
    catalog: C,
    config: RemoteSessionCreationConfig,
}

impl<T, C> RemoteSessionCreationService<T, C> {
    pub fn new(transport: T, catalog: C) -> Self {
        Self {
            transport,
            catalog,
            config: RemoteSessionCreationConfig::default(),
        }
    }

    #[cfg(test)]
    pub fn with_config(mut self, config: RemoteSessionCreationConfig) -> Self {
        self.config = config;
        self
    }
}

impl<T, C> RemoteSessionCreationService<T, C>
where
    T: RemoteSessionCreationTransport,
    T::Error: ToString,
    C: RemoteSessionCreationCatalog,
    C::Error: ToString,
{
    pub fn create_session(
        &self,
        request: RemoteSessionCreationRequest,
    ) -> Result<ManagedSessionRecord, RemoteSessionCreationError> {
        if request.authority_node_id.trim().is_empty() {
            return Err(RemoteSessionCreationError::InvalidRequest(
                "authority node id is required".to_string(),
            ));
        }
        let request_id = next_request_id();
        let reply = self
            .transport
            .create_session(
                CreateSessionRequestPayload {
                    request_id: request_id.clone(),
                    authority_node_id: request.authority_node_id.clone(),
                    cwd_hint: request
                        .cwd_hint
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    cols: request.cols,
                    rows: request.rows,
                },
                self.config.accept_timeout,
            )
            .map_err(|error| RemoteSessionCreationError::Transport(error.to_string()))?;

        let accepted = match reply {
            CreateSessionReply::Accepted(payload) => {
                if payload.request_id != request_id {
                    return Err(RemoteSessionCreationError::Protocol(format!(
                        "create-session reply id `{}` did not match request `{request_id}`",
                        payload.request_id
                    )));
                }
                payload
            }
            CreateSessionReply::Rejected(payload) => {
                return Err(RemoteSessionCreationError::Rejected {
                    code: payload.code,
                    message: payload.message,
                });
            }
        };

        self.wait_for_catalog_target(&request.authority_node_id, &accepted)
    }

    fn wait_for_catalog_target(
        &self,
        authority_node_id: &str,
        accepted: &CreateSessionAcceptedPayload,
    ) -> Result<ManagedSessionRecord, RemoteSessionCreationError> {
        let deadline = Instant::now() + self.config.catalog_timeout;
        loop {
            let targets = self
                .catalog
                .list_targets_on_authority(authority_node_id)
                .map_err(|error| RemoteSessionCreationError::Catalog(error.to_string()))?;
            if let Some(target) = targets.into_iter().find(|target| {
                target.address.session_id() == accepted.session_id
                    || target.address.id().as_str() == accepted.target_id
                    || target.address.qualified_target() == accepted.target_id
            }) {
                return Ok(target);
            }
            if Instant::now() >= deadline {
                return Err(RemoteSessionCreationError::CatalogTimeout {
                    authority_node_id: authority_node_id.to_string(),
                    session_id: accepted.session_id.clone(),
                    target_id: accepted.target_id.clone(),
                });
            }
            thread::sleep(self.config.catalog_poll_interval);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSessionCreationError {
    InvalidRequest(String),
    Transport(String),
    Rejected {
        code: &'static str,
        message: String,
    },
    Protocol(String),
    Catalog(String),
    CatalogTimeout {
        authority_node_id: String,
        session_id: String,
        target_id: String,
    },
}

impl fmt::Display for RemoteSessionCreationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid remote session creation request: {message}"),
            Self::Transport(message) => write!(f, "remote session creation transport failed: {message}"),
            Self::Rejected { code, message } => {
                write!(f, "remote session creation rejected ({code}): {message}")
            }
            Self::Protocol(message) => write!(f, "remote session creation protocol error: {message}"),
            Self::Catalog(message) => write!(f, "remote session creation catalog lookup failed: {message}"),
            Self::CatalogTimeout {
                authority_node_id,
                session_id,
                target_id,
            } => write!(
                f,
                "remote session `{authority_node_id}:{session_id}` accepted as `{target_id}` but did not appear in the catalog before timeout"
            ),
        }
    }
}

impl std::error::Error for RemoteSessionCreationError {}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn next_request_id() -> String {
    let millis = now_millis();
    let seq = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("create-session-{}-{millis}-{seq}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::{WorkspaceInstanceId, WorkspaceSessionRole};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FakeTransport {
        requests: Rc<RefCell<Vec<CreateSessionRequestPayload>>>,
        reply: CreateSessionReply,
    }

    impl RemoteSessionCreationTransport for FakeTransport {
        type Error = String;

        fn create_session(
            &self,
            request: CreateSessionRequestPayload,
            _accept_timeout: Duration,
        ) -> Result<CreateSessionReply, Self::Error> {
            self.requests.borrow_mut().push(request.clone());
            Ok(match &self.reply {
                CreateSessionReply::Accepted(payload) => {
                    CreateSessionReply::Accepted(CreateSessionAcceptedPayload {
                        request_id: request.request_id,
                        ..payload.clone()
                    })
                }
                CreateSessionReply::Rejected(payload) => {
                    CreateSessionReply::Rejected(CreateSessionRejectedPayload {
                        request_id: request.request_id,
                        ..payload.clone()
                    })
                }
            })
        }
    }

    #[derive(Clone)]
    struct FakeCatalog {
        calls: Rc<RefCell<usize>>,
        targets_by_call: Rc<RefCell<Vec<Vec<ManagedSessionRecord>>>>,
    }

    impl RemoteSessionCreationCatalog for FakeCatalog {
        type Error = String;

        fn list_targets_on_authority(
            &self,
            _authority_id: &str,
        ) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            *self.calls.borrow_mut() += 1;
            Ok(self.targets_by_call.borrow_mut().pop().unwrap_or_default())
        }
    }

    #[test]
    fn create_session_waits_for_catalog_convergence_after_acceptance() {
        let requests = Rc::new(RefCell::new(Vec::new()));
        let transport = FakeTransport {
            requests: requests.clone(),
            reply: CreateSessionReply::Accepted(CreateSessionAcceptedPayload {
                request_id: String::new(),
                session_id: "session-1".to_string(),
                target_id: "remote-peer:peer-a:session-1".to_string(),
            }),
        };
        let catalog = FakeCatalog {
            calls: Rc::new(RefCell::new(0)),
            targets_by_call: Rc::new(RefCell::new(vec![
                vec![remote_target("peer-a", "session-1")],
                Vec::new(),
            ])),
        };
        let service = RemoteSessionCreationService::new(transport, catalog.clone()).with_config(
            RemoteSessionCreationConfig {
                catalog_poll_interval: Duration::from_millis(1),
                ..RemoteSessionCreationConfig::default()
            },
        );

        let created = service
            .create_session(RemoteSessionCreationRequest {
                authority_node_id: "peer-a".to_string(),
                cwd_hint: Some(PathBuf::from("/tmp/demo")),
                cols: 120,
                rows: 40,
            })
            .expect("session creation should converge");

        assert_eq!(created.address.session_id(), "session-1");
        assert_eq!(requests.borrow()[0].authority_node_id, "peer-a");
        assert_eq!(requests.borrow()[0].cwd_hint.as_deref(), Some("/tmp/demo"));
        assert_eq!(*catalog.calls.borrow(), 2);
    }

    #[test]
    fn create_session_returns_rejection_without_catalog_wait() {
        let catalog = FakeCatalog {
            calls: Rc::new(RefCell::new(0)),
            targets_by_call: Rc::new(RefCell::new(Vec::new())),
        };
        let service = RemoteSessionCreationService::new(
            FakeTransport {
                requests: Rc::new(RefCell::new(Vec::new())),
                reply: CreateSessionReply::Rejected(CreateSessionRejectedPayload {
                    request_id: String::new(),
                    code: "create_session_failed",
                    message: "no pty".to_string(),
                }),
            },
            catalog.clone(),
        );

        let error = service
            .create_session(RemoteSessionCreationRequest {
                authority_node_id: "peer-a".to_string(),
                cwd_hint: None,
                cols: 0,
                rows: 0,
            })
            .expect_err("rejection should be returned");

        assert!(matches!(
            error,
            RemoteSessionCreationError::Rejected {
                code: "create_session_failed",
                ..
            }
        ));
        assert_eq!(*catalog.calls.borrow(), 0);
    }

    #[test]
    fn create_session_times_out_when_catalog_never_publishes_target() {
        let service = RemoteSessionCreationService::new(
            FakeTransport {
                requests: Rc::new(RefCell::new(Vec::new())),
                reply: CreateSessionReply::Accepted(CreateSessionAcceptedPayload {
                    request_id: String::new(),
                    session_id: "session-1".to_string(),
                    target_id: "remote-peer:peer-a:session-1".to_string(),
                }),
            },
            FakeCatalog {
                calls: Rc::new(RefCell::new(0)),
                targets_by_call: Rc::new(RefCell::new(Vec::new())),
            },
        )
        .with_config(RemoteSessionCreationConfig {
            catalog_timeout: Duration::from_millis(1),
            catalog_poll_interval: Duration::from_millis(1),
            ..RemoteSessionCreationConfig::default()
        });

        let error = service
            .create_session(RemoteSessionCreationRequest {
                authority_node_id: "peer-a".to_string(),
                cwd_hint: None,
                cols: 80,
                rows: 24,
            })
            .expect_err("missing catalog target should time out");

        assert!(matches!(
            error,
            RemoteSessionCreationError::CatalogTimeout { .. }
        ));
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some(format!("{authority_id}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some(WorkspaceInstanceId::new(session_id).as_str().to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }
}
