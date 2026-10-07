use super::logical_key::LogicalKey;
use crate::ratatui_node::runtime::RemoteNodeConnectionInfo;
use std::sync::Arc;

/// Outcome of an asynchronous remote-host connect operation, delivered back to
/// the state loop so it can apply session/active-target mutations on its single
/// writer thread.
#[derive(Debug, Clone)]
pub(crate) struct RemoteHostConnectedOutcome {
    pub target_id: String,
    pub authority_node_id: String,
    pub created_target: crate::domain::session_catalog::ManagedSessionRecord,
    pub connection_info: Option<crate::ratatui_node::runtime::RemoteNodeConnectionInfo>,
    /// Whether the connect filled an empty `tls_pin_sha256` through relay
    /// fingerprint auto-discovery (issue #156 slice 3); the response
    /// message names the discovery so the operator sees where the pin
    /// came from.
    pub pin_auto_discovered: bool,
}

/// Events that converge on `StateEventLoop`, the single writer of `SharedState`.
///
/// All lifecycle mutations (local child exit, authority-host child exit,
/// session creation, remote viewer close) are sent as events and applied
/// sequentially by the loop.  Raw PTY data for authority-host sessions does
/// not travel through this channel; it is forwarded directly by
/// `AuthorityHostIoLoop`.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum StateEvent {
    /// A local `alacritty_terminal` session's child process exited.
    LocalSessionChildExit { target_id: String, exit_code: i32 },
    /// A local `alacritty_terminal` session changed its window title.
    LocalSessionTitleChanged { target_id: String, title: String },
    /// The detected task state of a session changed (e.g., shell at prompt vs
    /// running a foreground command).
    SessionTaskStateChanged {
        target_id: String,
        task_state: crate::domain::session_catalog::ManagedSessionTaskState,
    },
    /// The detected foreground command name of a session changed.
    SessionCommandNameChanged {
        target_id: String,
        command_name: String,
    },
    /// The detected foreground command name was cleared (e.g. the foreground
    /// process exited and the shell is back at an empty prompt).
    SessionCommandNameCleared { target_id: String },
    /// A local `alacritty_terminal` session produced output and the TUI
    /// clients should be refreshed.  This is deliberately sent to the single
    /// writer loop so that snapshots are broadcast without holding the
    /// terminal lock.
    LocalSessionOutput { target_id: String },
    /// An authority-host session's shell child exited.
    AuthorityHostSessionChildExited { target_id: String, exit_code: i32 },
    /// An authority-host PTY master reached EOF or an unrecoverable read error.
    AuthorityHostSessionPtyClosed { target_id: String },
    /// A TUI client connected to the local Unix socket.
    ClientConnected { client_id: u64 },
    /// A TUI client disconnected from the local Unix socket.
    ClientDisconnected { client_id: u64 },
    /// A command received from a TUI client.
    ClientCommand {
        client_id: u64,
        command: ClientCommand,
    },
    /// Session sync runtime asked this node to host a new authority-target
    /// session for a remote viewer.
    CreateAuthorityHostSession {
        request_id: String,
        cols: u16,
        rows: u16,
        reply_tx: std::sync::mpsc::Sender<
            Result<CreatedAuthorityHostTarget, crate::lifecycle::LifecycleError>,
        >,
    },
    /// A remote session produced output; the TUI clients should be refreshed.
    RemoteSessionOutput { target_id: String },
    /// The authority transport for a remote session dropped. The session should
    /// be marked offline and a reconnect worker should be started.
    RemoteSessionDisconnected { target_id: String },
    /// A reconnect worker successfully re-established the authority transport
    /// for a remote session. The new runtime should be inserted and marked online.
    RemoteSessionReconnected {
        target_id: String,
        session: Arc<super::remote_session::RatatuiRemoteSession>,
    },
    /// The remote runtime owner received a published update for a remote-peer
    /// session (e.g., cwd or task-state changed). The local catalog record should
    /// be updated to match.
    RemoteSessionCatalogUpdated {
        record: Box<crate::domain::session_catalog::ManagedSessionRecord>,
    },
    /// A session has exited and should be removed from the local catalog.
    ///
    /// This covers both a remote session viewer closing and a local
    /// authority-host session being closed by a remote peer.
    SessionClosed { target_id: String },
    /// An agent hook sent a lifecycle signal for a local session.
    AgentSignalReceived {
        target_id: String,
        agent: String,
        event: String,
        payload: serde_json::Value,
    },
    /// A remote peer went offline (the last ingress session to it closed).
    /// Any remote-peer sessions that were views into that node are stale and
    /// should be removed from the local catalog.
    RemoteNodeOffline { node_id: String },
    /// A remote peer actively rejected this host's operator key during the
    /// outbound dial. Re-dialing cannot succeed until the stale authorized
    /// operator keys are removed on the remote host, so the connect flow must
    /// not fall back to an SSH bootstrap that would only spawn redundant
    /// node servers.
    RemoteNodeAuthRejected { node_id: String, message: String },
    /// A remote peer that was offline has re-established its gRPC node session.
    /// This cancels any outbound-dial retry worker for the node and lets
    /// per-session reconnect workers proceed.
    RemoteNodeOnline { node_id: String },
    /// The outbound-dial retry worker exhausted its budget without reconnecting
    /// the node. The state loop should remove the node's sessions and snapshot.
    RemoteNodeReconnectFailed { node_id: String },
    /// Record connection metadata for a remote peer so reconnect can reuse the
    /// endpoint, TLS pin, and operator key without re-bootstrapping.
    RecordRemoteNodeConnection {
        node_id: String,
        info: RemoteNodeConnectionInfo,
    },
    /// The control plane's upstream connectivity changed.
    ///
    /// Sent by `NetworkProbe` so the state loop can distinguish a transient
    /// control-plane outage from a permanent remote host failure.
    NetworkConnectivityChanged { online: bool },
    /// The persistent outbound link to the pinned relay connected. Log-only
    /// presence-wise: peer presence arrives via `RelayPeerOnline` /
    /// `RelayPeerOffline` (relay `Presence` frames), not from link state.
    RelayLinkConnected {
        /// The relay address the link registered with.
        relay_address: String,
    },
    /// The persistent outbound link to the pinned relay dropped; the relay
    /// client is backing off and will reconnect. Log-only: peer presence
    /// arrives via `RelayPeerOnline` / `RelayPeerOffline`.
    RelayLinkDisconnected {
        /// The relay address that was dialed.
        relay_address: String,
        /// Why the link ended (see `RelayClientEvent::Disconnected`).
        reason: String,
    },
    /// The relay sent a connection-level `Error` frame on the persistent link;
    /// a `RelayLinkDisconnected` follows immediately as the link tears down.
    /// The raw wire code is preserved; type it via
    /// `RelayErrorCode::from_wire`.
    RelayLinkError {
        /// The wire error code from `Frame::Error.code`.
        code: u16,
        /// The relay's human-readable explanation.
        message: String,
    },
    /// A relay-watched peer came online (relay `Presence` transition).
    RelayPeerOnline {
        /// The peer's certificate fingerprint (lowercase).
        node_id: String,
    },
    /// A relay-watched peer went offline (relay `Presence` transition).
    RelayPeerOffline {
        /// The peer's certificate fingerprint (lowercase).
        node_id: String,
    },
    /// A previously unreachable outbound-dial peer is now reachable at L4.
    ///
    /// Sent by `PeerReachabilityProbeWorker` so the state loop can reset the
    /// retry worker and attempt an immediate reconnect.
    RemoteNodeReachable { node_id: String },
    /// Reconnect to all outbound-dial hosts recorded in the persistent snapshot.
    ///
    /// Sent once at startup and again after the control plane recovers from a
    /// network outage.
    ReconnectSnapshotHosts,
    /// The asynchronous snapshot reconnect operation finished. The state loop
    /// applies session/active-target mutations on success or logs the error.
    SnapshotHostReconnectResult {
        profile_name: String,
        authority_node_id: String,
        result: Box<Result<RemoteHostConnectedOutcome, String>>,
    },
    /// The asynchronous remote-host connect operation finished. The state loop
    /// applies session/active-target mutations and reports success or error to
    /// the originating client.
    RemoteHostConnectResult {
        client_id: u64,
        profile_name: String,
        result: Box<Result<RemoteHostConnectedOutcome, String>>,
        activate: bool,
    },
    /// The asynchronous remote session creation operation finished. The state
    /// loop applies catalog/active-target mutations and reports success or error
    /// to the originating client.
    RemoteSessionCreateResult {
        client_id: u64,
        authority_node_id: String,
        result: Box<Result<crate::domain::session_catalog::ManagedSessionRecord, String>>,
    },
    /// The asynchronous e2e relay probe finished; the summary (JSON) or the
    /// error is reported back to the originating one-shot client.
    E2eRelayProbeResult {
        client_id: u64,
        result: Box<Result<String, String>>,
    },
    /// The asynchronous relay join finished. The state loop applies the
    /// outcome on its single-writer thread: restart the persistent relay
    /// link when the pin changed, clear stale relay errors, broadcast, and
    /// report back to the originating client.
    RelayJoinResult {
        client_id: u64,
        address: String,
        result: Box<Result<RelayJoinApplied, String>>,
    },
    /// The asynchronous relay removal finished; the state loop stops the
    /// persistent relay link, clears relay presence/error state, broadcasts,
    /// and reports back to the originating client.
    RelayRemoveResult {
        client_id: u64,
        result: Box<Result<String, String>>,
    },
    /// Timer tick telling the state loop to flush a pending output-driven
    /// snapshot broadcast. Sent by a detached interval thread; only the state
    /// loop consumes it. PTY output events only set a dirty flag and are
    /// coalesced into one broadcast per interval so a screen repaint arriving
    /// as many small chunks does not serialize a full snapshot per chunk.
    FlushOutputBroadcast,
}

/// A command sent by a TUI client and processed by `StateEventLoop`.
#[derive(Debug)]
pub(crate) enum ClientCommand {
    /// Attach request: triggers an initial snapshot for the client.
    Attach,
    /// STATUS one-shot command.
    Status,
    /// STOP one-shot command.
    Stop,
    /// LIST_SESSIONS one-shot command.
    ListSessions,
    /// Create a new local PTY session. `cwd` is the creating client's working
    /// directory; the shell starts there so the pane lands where the operator
    /// launched the TUI instead of wherever the node server's cwd happens to
    /// be. Falls back to the node server's cwd when absent or invalid.
    CreateLocalSession { cwd: Option<String> },
    /// Activate a specific session target.
    ActivateTarget { target_id: String },
    /// Connect to a saved remote host profile.
    ConnectRemoteHost { profile_name: String },
    /// Detach all attached clients.
    DetachAll,
    /// Resize the active session.
    Resize { cols: u16, rows: u16 },
    /// Forward a logical keyboard key to a specific session.
    Input { target_id: String, key: LogicalKey },
    /// Paste plain text into a specific session.
    PasteText { target_id: String, text: String },
    /// Paste a file whose bytes should be cached on the receiving node.
    PasteFile {
        target_id: String,
        filename_hint: String,
        bytes: Vec<u8>,
    },
    /// Request the full scrollback history for a session.
    GetHistory { target_id: String },
    /// Create a new remote session on the authority of the selected target.
    CreateRemoteSession {
        authority_node_id: String,
        /// The creating client's working directory, forwarded as the
        /// authority's cwd hint so the remote shell starts where the
        /// operator is working.
        cwd: Option<String>,
    },
    /// Close a session and cancel any pending reconnect for it.
    CloseSession { target_id: String },
    /// Set or clear the public endpoint advertised to remote peers.
    SetPublic {
        endpoint: Option<String>,
        save: bool,
    },
    /// Test-only probe (issue #37): open `streams` concurrent relay streams
    /// to `peer` through this node's relay client, hold them open for
    /// `hold_secs`, then close them. Used by the docker e2e harness to
    /// exercise concurrent multi-session topology without a TUI.
    E2eRelayProbe {
        peer: String,
        streams: u32,
        hold_secs: u32,
    },
    /// Enroll this node at the relay `address` (base64-decoded on the wire)
    /// with the invite/deploy `token`, pin the learned fingerprint into
    /// `relay.toml`, and (re)start the persistent relay link. `force`
    /// confirms a pin mismatch after the TUI's explicit operator warning
    /// (issue #156 slice 1).
    RelayJoin {
        address: String,
        token: String,
        force: bool,
    },
    /// Remove the pinned relay: delete `relay.toml`, stop the persistent
    /// relay link, and reset relay state (issue #156 slice 1).
    RelayRemove,
}

/// Reply returned by `StateEventLoop` for control commands.
#[derive(Debug, Clone)]
pub(crate) enum CommandOutcome {
    Ok,
    Message(String),
    Error(String),
    Data(serde_json::Value),
}

/// Minimal reply payload returned by `StateEventLoop` when it creates an
/// authority-host session.  Kept in this module so `state_event.rs` does not
/// depend on session-sync types.
#[derive(Debug, Clone)]
pub(crate) struct CreatedAuthorityHostTarget {
    pub session_id: String,
    pub target_id: String,
}

/// What a successful relay join established, as applied by the state loop.
#[derive(Debug, Clone)]
pub(crate) struct RelayJoinApplied {
    /// The pin `join_relay` just wrote, carried here so the state loop can
    /// restart the link without re-reading the file on its thread.
    pub pinned: crate::infra::relay_toml_store::RelayTomlConfig,
    /// True when the pin replaced a different address/fingerprint (or no
    /// link is currently installed) and the persistent relay link must be
    /// (re)started; false when the re-pinned config was already in place.
    pub link_restarted: bool,
}
