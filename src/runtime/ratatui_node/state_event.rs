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
    LocalSessionChildExit {
        session_id: String,
        exit_code: i32,
    },
    /// A local `alacritty_terminal` session changed its window title.
    LocalSessionTitleChanged {
        session_id: String,
        title: String,
    },
    /// An authority-host session's shell child exited.
    AuthorityHostSessionChildExited {
        session_id: String,
        exit_code: i32,
    },
    /// An authority-host PTY master reached EOF or an unrecoverable read error.
    AuthorityHostSessionPtyClosed {
        session_id: String,
    },
    /// A TUI client connected to the local Unix socket.
    ClientConnected {
        client_id: usize,
    },
    /// A TUI client disconnected from the local Unix socket.
    ClientDisconnected {
        client_id: usize,
    },
    /// Keyboard input received from a client for a specific session.
    ClientInput {
        session_id: String,
        bytes: Vec<u8>,
    },
    /// A client asked to activate a different session target.
    ClientActivatedTarget {
        target_id: String,
    },
    /// A client reported a new terminal size.
    ClientResized {
        cols: u16,
        rows: u16,
    },
    /// A client asked to create a new local PTY session.
    ClientCreateLocalSession,
    /// Session sync runtime asked this node to host a new authority-target
    /// session for a remote viewer.
    CreateAuthorityHostSession {
        request_id: String,
        cols: u16,
        rows: u16,
        reply_tx: std::sync::mpsc::Sender<Result<CreatedAuthorityHostTarget, crate::lifecycle::LifecycleError>>,
    },
    /// A remote session viewer closed, so the mirrored remote target should be
    /// removed from the local catalog.
    RemoteSessionClosed {
        target_id: String,
    },
}

/// Minimal reply payload returned by `StateEventLoop` when it creates an
/// authority-host session.  Kept in this module so `state_event.rs` does not
/// depend on session-sync types.
#[derive(Debug, Clone)]
pub(crate) struct CreatedAuthorityHostTarget {
    pub session_id: String,
    pub target_id: String,
}
