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
    /// A remote session rendered a local input echo; the TUI clients should be
    /// refreshed.
    RemoteSessionInputEcho { target_id: String },
    /// A session has exited and should be removed from the local catalog.
    ///
    /// This covers both a remote session viewer closing and a local
    /// authority-host session being closed by a remote peer.
    SessionClosed { target_id: String },
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
    /// Create a new local PTY session.
    CreateLocalSession,
    /// Activate a specific session target.
    ActivateTarget { target_id: String },
    /// Connect to a saved remote host profile.
    ConnectRemoteHost { profile_name: String },
    /// Detach all attached clients.
    DetachAll,
    /// Resize the active session.
    Resize { cols: u16, rows: u16 },
    /// Forward keyboard input to a specific session.
    Input { target_id: String, bytes: Vec<u8> },
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
