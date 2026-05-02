use crate::domain::session_catalog::ConsoleLocation;

pub const REMOTE_PROTOCOL_VERSION: &str = "1.1";
pub const SERVER_SENDER_ID: &str = "server";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSessionChannel {
    Authority,
    Publication,
}

impl NodeSessionChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authority => "authority",
            Self::Publication => "publication",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSessionEnvelope {
    pub channel: NodeSessionChannel,
    pub envelope: ProtocolEnvelope<ControlPlanePayload>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolEnvelope<P> {
    pub protocol_version: String,
    pub message_id: String,
    pub message_type: &'static str,
    pub timestamp: String,
    pub sender_id: String,
    pub correlation_id: Option<String>,
    pub session_id: Option<String>,
    pub target_id: Option<String>,
    pub attachment_id: Option<String>,
    pub console_id: Option<String>,
    pub payload: P,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlanePayload {
    ClientHello(ClientHelloPayload),
    ServerHello(ServerHelloPayload),
    OpenMirrorRequest(OpenMirrorRequestPayload),
    OpenMirrorAccepted(OpenMirrorAcceptedPayload),
    OpenMirrorRejected(OpenMirrorRejectedPayload),
    CloseMirrorRequest(CloseMirrorRequestPayload),
    MirrorBootstrapChunk(MirrorBootstrapChunkPayload),
    MirrorBootstrapComplete(MirrorBootstrapCompletePayload),
    OpenTargetOk(OpenTargetOkPayload),
    OpenTargetRejected(OpenTargetRejectedPayload),
    ResizeAuthorityChanged(ResizeAuthorityChangedPayload),
    TargetInput(TargetInputPayload),
    TargetOutput(TargetOutputPayload),
    ApplyResize(ApplyResizePayload),
    TargetPublished(TargetPublishedPayload),
    TargetExited(TargetExitedPayload),
    Error(ErrorPayload),
}

impl ControlPlanePayload {
    pub fn message_type(&self) -> &'static str {
        match self {
            Self::ClientHello(_) => "client_hello",
            Self::ServerHello(_) => "server_hello",
            Self::OpenMirrorRequest(_) => "open_mirror_request",
            Self::OpenMirrorAccepted(_) => "open_mirror_accepted",
            Self::OpenMirrorRejected(_) => "open_mirror_rejected",
            Self::CloseMirrorRequest(_) => "close_mirror_request",
            Self::MirrorBootstrapChunk(_) => "mirror_bootstrap_chunk",
            Self::MirrorBootstrapComplete(_) => "mirror_bootstrap_complete",
            Self::OpenTargetOk(_) => "open_target_ok",
            Self::OpenTargetRejected(_) => "open_target_rejected",
            Self::ResizeAuthorityChanged(_) => "resize_authority_changed",
            Self::TargetInput(_) => "target_input",
            Self::TargetOutput(_) => "target_output",
            Self::ApplyResize(_) => "apply_resize",
            Self::TargetPublished(_) => "target_published",
            Self::TargetExited(_) => "target_exited",
            Self::Error(_) => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHelloPayload {
    pub node_id: String,
    pub client_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHelloPayload {
    pub server_id: String,
    pub server_version: String,
    pub accepted_protocol_version: String,
    pub heartbeat_interval_ms: u64,
    pub session_recovery_policy: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenMirrorRequestPayload {
    pub session_id: String,
    pub target_id: String,
    pub console_id: String,
    pub cols: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenMirrorAcceptedPayload {
    pub session_id: String,
    pub target_id: String,
    pub availability: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenMirrorRejectedPayload {
    pub session_id: String,
    pub target_id: String,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseMirrorRequestPayload {
    pub session_id: String,
    pub target_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorBootstrapChunkPayload {
    pub session_id: String,
    pub target_id: String,
    pub chunk_seq: u64,
    pub stream: &'static str,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorBootstrapCompletePayload {
    pub session_id: String,
    pub target_id: String,
    pub last_chunk_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTargetOkPayload {
    pub session_id: String,
    pub target_id: String,
    pub attachment_id: String,
    pub console_id: String,
    pub resize_epoch: u64,
    pub resize_authority_console_id: String,
    pub resize_authority_host_id: String,
    pub availability: &'static str,
    pub initial_snapshot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTargetRejectedPayload {
    pub session_id: String,
    pub target_id: String,
    pub console_id: String,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeAuthorityChangedPayload {
    pub session_id: String,
    pub target_id: String,
    pub resize_epoch: u64,
    pub resize_authority_console_id: String,
    pub resize_authority_host_id: String,
    pub cols: Option<usize>,
    pub rows: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInputPayload {
    pub attachment_id: String,
    pub session_id: String,
    pub target_id: String,
    pub console_id: String,
    pub console_host_id: String,
    pub input_seq: u64,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOutputPayload {
    pub session_id: String,
    pub target_id: String,
    pub output_seq: u64,
    pub stream: &'static str,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResizePayload {
    pub session_id: String,
    pub target_id: String,
    pub resize_epoch: u64,
    pub resize_authority_console_id: String,
    pub cols: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPublishedPayload {
    pub transport_session_id: String,
    pub source_session_name: Option<String>,
    pub selector: Option<String>,
    pub availability: &'static str,
    pub session_role: Option<&'static str>,
    pub workspace_key: Option<String>,
    pub command_name: Option<String>,
    pub current_path: Option<String>,
    pub attached_clients: usize,
    pub window_count: usize,
    pub task_state: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetExitedPayload {
    pub transport_session_id: String,
    pub source_session_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorPayload {
    pub code: &'static str,
    pub message: String,
    pub details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConsoleDescriptor {
    pub console_id: String,
    pub console_host_id: String,
    pub location: ConsoleLocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlaneDestination {
    ObserverNode(String),
    AuthorityNode(String),
    AllOpenedObservers { session_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedControlPlaneMessage {
    pub destination: ControlPlaneDestination,
    pub envelope: ProtocolEnvelope<ControlPlanePayload>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBoundControlPlaneMessage {
    pub node_id: String,
    pub envelope: ProtocolEnvelope<ControlPlanePayload>,
}
