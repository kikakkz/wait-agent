// Legacy tmux-era remote protocol kept during the ratatui migration; most items are currently unused.

use crate::domain::session_catalog::ConsoleLocation;

pub const REMOTE_PROTOCOL_VERSION: &str = "1.1";
// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
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
    RawPtyInput(RawPtyInputPayload),
    PasteFileRequest(PasteFileRequestPayload),
    TargetOutput(TargetOutputPayload),
    RawPtyOutput(RawPtyOutputPayload),
    ApplyResize(ApplyResizePayload),
    ResizeApplied(ResizeAppliedPayload),
    TargetGeometryChanged(TargetGeometryChangedPayload),
    CreateSessionRequest(CreateSessionRequestPayload),
    CreateSessionAccepted(CreateSessionAcceptedPayload),
    CreateSessionRejected(CreateSessionRejectedPayload),
    TargetPublished(TargetPublishedPayload),
    TargetExited(TargetExitedPayload),
    TargetPublicationAck(TargetPublicationAckPayload),
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
            Self::RawPtyInput(_) => "raw_pty_input",
            Self::PasteFileRequest(_) => "paste_file_request",
            Self::TargetOutput(_) => "target_output",
            Self::RawPtyOutput(_) => "raw_pty_output",
            Self::ApplyResize(_) => "apply_resize",
            Self::ResizeApplied(_) => "resize_applied",
            Self::TargetGeometryChanged(_) => "target_geometry_changed",
            Self::CreateSessionRequest(_) => "create_session_request",
            Self::CreateSessionAccepted(_) => "create_session_accepted",
            Self::CreateSessionRejected(_) => "create_session_rejected",
            Self::TargetPublished(_) => "target_published",
            Self::TargetExited(_) => "target_exited",
            Self::TargetPublicationAck(_) => "target_publication_ack",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapMode {
    Full,
    VisibleOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenMirrorRequestPayload {
    pub session_id: String,
    pub target_id: String,
    pub console_id: String,
    pub cols: usize,
    pub rows: usize,
    pub raw_pty_passthrough: bool,
    pub bootstrap_mode: BootstrapMode,
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
    pub output_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorBootstrapCompletePayload {
    pub session_id: String,
    pub target_id: String,
    pub last_chunk_seq: u64,
    pub alternate_screen_active: bool,
    pub application_cursor_keys: bool,
    pub cursor_visible: bool,
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
pub struct RawPtyInputPayload {
    pub attachment_id: String,
    pub session_id: String,
    pub target_id: String,
    pub console_id: String,
    pub console_host_id: String,
    pub input_seq: u64,
    pub input_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteFileRequestPayload {
    pub session_id: String,
    pub target_id: String,
    pub filename_hint: String,
    pub file_id: String,
    pub total_chunks: u32,
    pub chunk_index: u32,
    pub chunk_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOutputPayload {
    pub session_id: String,
    pub target_id: String,
    pub output_seq: u64,
    pub stream: &'static str,
    pub output_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPtyOutputPayload {
    pub session_id: String,
    pub target_id: String,
    pub output_seq: u64,
    pub output_bytes: Vec<u8>,
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
pub struct ResizeAppliedPayload {
    pub session_id: String,
    pub target_id: String,
    pub resize_epoch: u64,
    pub resize_authority_console_id: String,
    pub cols: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetGeometryChangedPayload {
    pub session_id: String,
    pub target_id: String,
    pub cols: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSessionRequestPayload {
    pub request_id: String,
    pub authority_node_id: String,
    pub cwd_hint: Option<String>,
    pub cols: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSessionAcceptedPayload {
    pub request_id: String,
    pub session_id: String,
    pub target_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSessionRejectedPayload {
    pub request_id: String,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPublishedPayload {
    pub transport_session_id: String,
    pub node_instance_id: String,
    pub revision: u64,
    pub authority_host_session_name: Option<String>,
    pub selector: Option<String>,
    pub availability: &'static str,
    pub session_role: Option<&'static str>,
    pub workspace_key: Option<String>,
    pub command_name: Option<String>,
    pub display_command_name: Option<String>,
    pub current_path: Option<String>,
    pub attached_clients: usize,
    pub window_count: usize,
    pub task_state: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetExitedPayload {
    pub transport_session_id: String,
    pub node_instance_id: String,
    pub revision: u64,
    pub authority_host_session_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPublicationAckStatus {
    Applied,
    StaleRevision,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPublicationAckPayload {
    pub node_id: String,
    pub node_instance_id: String,
    pub target_id: String,
    pub revision: u64,
    pub status: TargetPublicationAckStatus,
    pub message: Option<String>,
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

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::{
        ControlPlanePayload, ProtocolEnvelope, TargetExitedPayload, TargetPublishedPayload,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, write_control_plane_envelope,
    };

    fn round_trip_target_published() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-tp".to_string(),
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
                authority_host_session_name: Some("target-host-1".to_string()),
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
        }
    }

    #[test]
    fn target_published_round_trips_authority_host_session_name() {
        let envelope = round_trip_target_published();
        let mut bytes = Vec::new();
        write_control_plane_envelope(&mut bytes, &envelope).expect("encode");
        let decoded = read_control_plane_envelope(&mut bytes.as_slice()).expect("decode");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn target_exited_round_trips_authority_host_session_name() {
        let envelope = ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-te".to_string(),
            message_type: "target_exited",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetExited(TargetExitedPayload {
                transport_session_id: "shell-1".to_string(),
                node_instance_id: "node-inst-1".to_string(),
                revision: 7,
                authority_host_session_name: Some("target-host-1".to_string()),
            }),
        };
        let mut bytes = Vec::new();
        write_control_plane_envelope(&mut bytes, &envelope).expect("encode");
        let decoded = read_control_plane_envelope(&mut bytes.as_slice()).expect("decode");
        assert_eq!(decoded, envelope);
    }
}
