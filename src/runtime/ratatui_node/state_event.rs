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
    ClientConnected { client_id: usize },
    /// A TUI client disconnected from the local Unix socket.
    ClientDisconnected { client_id: usize },
    /// Keyboard input received from a client for a specific session.
    ClientInput { target_id: String, bytes: Vec<u8> },
    /// A client asked to activate a different session target.
    ClientActivatedTarget {
        target_id: String,
        reply_tx: std::sync::mpsc::Sender<CommandOutcome>,
    },
    /// A client reported a new terminal size.
    ClientResized { cols: u16, rows: u16 },
    /// A client asked to create a new local PTY session.
    ClientCreateLocalSession {
        reply_tx: std::sync::mpsc::Sender<CommandOutcome>,
    },
    /// A client asked to stop the server.
    ClientStop {
        reply_tx: std::sync::mpsc::Sender<CommandOutcome>,
    },
    /// A client asked to connect to a saved remote host profile.
    ClientConnectRemoteHost {
        profile_name: String,
        reply_tx: std::sync::mpsc::Sender<CommandOutcome>,
    },
    /// A client asked to detach all attached TUI clients.
    ClientDetachAll {
        reply_tx: std::sync::mpsc::Sender<CommandOutcome>,
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
    /// A remote session viewer closed, so the mirrored remote target should be
    /// removed from the local catalog.
    RemoteSessionClosed { target_id: String },
}

/// Reply returned by `StateEventLoop` for one-shot control commands that mutate
/// `SharedState`.  Kept in this module so `state_event.rs` does not depend on
/// `snapshot.rs`.
#[derive(Debug, Clone)]
pub(crate) enum CommandOutcome {
    Ok,
    Message(String),
    Error(String),
}

/// Minimal reply payload returned by `StateEventLoop` when it creates an
/// authority-host session.  Kept in this module so `state_event.rs` does not
/// depend on session-sync types.
#[derive(Debug, Clone)]
pub(crate) struct CreatedAuthorityHostTarget {
    pub session_id: String,
    pub target_id: String,
}
