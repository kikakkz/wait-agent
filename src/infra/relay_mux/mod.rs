//! Relay mux frame layer.
//!
//! Governing design: `docs/relay-design.md` (协议分层). Multiplexes ordered,
//! reliable, full-duplex byte streams over one connection: every `MuxStream`
//! satisfies the `PeerConnection` seam from `crate::infra::peer_connection`
//! through the blanket impl, so the node-to-node inner TLS and session
//! protocols run over a relay stream unmodified. Relay control frames
//! (register / open_stream routing / heartbeat) are a later slice; this
//! module is only the stream mux (open / close / data / window).
//!
//! # Lock order
//!
//! All shared state uses short critical sections; no lock is held across an
//! `.await`. Every path acquires locks in this order (earlier → later), and
//! no path acquires them in reverse:
//!
//! 1. `StreamTable` (`Mutex<HashMap<u32, StreamEntry>>`, connection.rs)
//! 2. per-stream mutexes (stream.rs): `Mutex<StreamState>` and
//!    `Mutex<Option<MuxResetError>>` (the relay-teardown payload)
//! 3. `close_reason` (`Mutex<Option<String>>`, connection.rs)
//! 4. `outbound_waker` (`Mutex<Option<Waker>>`, connection.rs)
//!
//! The only path that holds two locks at once is connection failure
//! handling (1 → 2). Reader dispatch takes (1), drops it, then takes each
//! per-stream lock separately. Stream read/write paths take a (2) lock alone
//! and never (1); the teardown payload lock is a leaf and is never held
//! while another lock is taken.
//!
//! # Concurrency model
//!
//! I/O-bound, async (tokio). The writer task owns the write half and pumps
//! one bounded `mpsc<Frame>`; the reader task decodes frames and dispatches
//! to per-stream bounded queues. Backpressure is applied by awaiting queue
//! capacity — nothing is dropped (see project rule: no defensive-drop
//! designs). Window frames carry per-stream credit; a sender with exhausted
//! credit parks until the peer grants more.
//!
//! # Connection teardown
//!
//! The four frame types have no connection-close frame, so local shutdown is
//! abrupt by design: dropping `MuxConnection` aborts the reader/writer tasks
//! and fails live streams with [`MuxError::ConnectionClosed`]. Peers observe
//! the connection loss (EOF on an idle link, or a truncated frame when abort
//! lands mid-frame) and fail their streams with the reason. A graceful
//! connection close belongs with the relay control frames (later slice).

pub mod connection;
pub mod frame;
pub mod stream;

use connection::MuxRole;
use frame::FrameType;

/// Initial per-stream send credit in each direction, granted implicitly by
/// `Open`. A sender parks once this credit is exhausted.
pub const DEFAULT_STREAM_WINDOW: u64 = 256 * 1024;

/// Receiver grants more credit after the peer has consumed this many bytes.
pub const WINDOW_GRANT_THRESHOLD: u64 = DEFAULT_STREAM_WINDOW / 2;

/// Maximum frame payload accepted on decode and produced per `Data` frame.
pub const MAX_FRAME_PAYLOAD: u32 = 16 * 1024;

/// Bounded queue of outbound frames between streams and the writer task.
pub const OUTBOUND_QUEUE: usize = 1024;

/// Bounded queue of inbound byte chunks between the reader task and one
/// stream's consumer. In-flight bytes are additionally bounded by
/// [`DEFAULT_STREAM_WINDOW`]; this caps queued chunks per stream.
pub const INBOUND_CHUNK_QUEUE: usize = 64;

/// Bounded queue of freshly opened peer streams waiting for `accept`.
pub const ACCEPT_QUEUE: usize = 64;

use std::io;
use std::num::NonZeroU32;

use thiserror::Error;

/// Errors of the mux frame layer.
#[derive(Debug, Error)]
pub enum MuxError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid frame type 0x{0:02x}")]
    InvalidFrameType(u8),
    #[error("reserved frame flags 0x{0:02x} are not zero")]
    InvalidFlags(u8),
    #[error("frame payload length {0} exceeds max {MAX_FRAME_PAYLOAD}")]
    PayloadTooLong(u32),
    #[error("invalid {0:?} payload length {1}")]
    InvalidPayload(FrameType, u32),
    #[error("invalid stream id {1} for control frame {0:?}")]
    InvalidStreamId(FrameType, u32),
    #[error("truncated frame header or payload")]
    TruncatedFrame,
    #[error("frame for unknown or fully closed stream {0}")]
    UnknownStream(u32),
    #[error("stream {0} is closed for writing")]
    StreamClosed(u32),
    #[error("peer violated the mux protocol: {0}")]
    ProtocolViolation(String),
    #[error("mux connection closed: {0}")]
    ConnectionClosed(String),
    /// An operation that is only legal on a relay-link-mode connection was
    /// attempted on a node-to-node connection.
    #[error("operation requires a relay-link mux connection")]
    NotRelayLink,
}

impl From<MuxError> for io::Error {
    fn from(error: MuxError) -> Self {
        use std::io::ErrorKind;
        match &error {
            MuxError::Io(io_error) => io::Error::new(io_error.kind(), error.to_string()),
            MuxError::TruncatedFrame => io::Error::new(ErrorKind::UnexpectedEof, error.to_string()),
            MuxError::StreamClosed(_) => io::Error::new(ErrorKind::BrokenPipe, error.to_string()),
            MuxError::UnknownStream(_)
            | MuxError::InvalidFrameType(_)
            | MuxError::InvalidFlags(_)
            | MuxError::PayloadTooLong(_)
            | MuxError::InvalidPayload(_, _)
            | MuxError::InvalidStreamId(_, _)
            | MuxError::ProtocolViolation(_)
            | MuxError::NotRelayLink => io::Error::new(ErrorKind::InvalidData, error.to_string()),
            MuxError::ConnectionClosed(_) => {
                io::Error::new(ErrorKind::ConnectionReset, error.to_string())
            }
        }
    }
}

/// Checks the stream-id parity convention for `role`.
///
/// The client allocates odd ids, the server even ids, so both sides can open
/// streams without negotiation. `MuxConnection` starts its counter at 1
/// (client) or 2 (server) and steps by 2.
pub(crate) fn owns_stream_id(role: MuxRole, stream_id: u32) -> bool {
    let odd = stream_id % 2 == 1;
    matches!(role, MuxRole::Client) == odd
}

/// Returns the first locally allocated stream id for `role`.
pub(crate) fn first_stream_id(role: MuxRole) -> NonZeroU32 {
    match role {
        MuxRole::Client => NonZeroU32::new(1).expect("1 is non-zero"),
        MuxRole::Server => NonZeroU32::new(2).expect("2 is non-zero"),
    }
}
