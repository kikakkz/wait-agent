//! Frame codec: deterministic wire layout, pure encode/decode.
//!
//! Every frame is a fixed 10-byte big-endian header followed by a
//! type-specific payload. Two frame families share the wire format on a
//! relay link (docs/relay-design.md 协议分层): stream frames and relay
//! control frames.
//!
//! ```text
//! +0  type       u8    0x01 Open | 0x02 Close | 0x03 Data | 0x04 Window
//!                    | 0x05 Register | 0x06 Unregister | 0x07 Heartbeat
//!                    | 0x08 OpenStream | 0x09 Error | 0x0A CloseStream
//!                    | 0x0B Enroll | 0x0C EnrollResponse
//!                    | 0x0D Presence | 0x0E Watch
//! +1  flags      u8    reserved, must be zero
//! +2  stream_id  u32   stream frames: odd = client-initiated, even =
//!                    server-initiated; control frames on relay links:
//!                    Register/Unregister/Heartbeat reserved 0; OpenStream:
//!                    the routed stream id; Error/CloseStream: the stream
//!                    the frame concerns (0 = none); Presence/Watch
//!                    reserved 0
//! +6  length     u32   payload length in bytes (<= MAX_FRAME_PAYLOAD)
//! +10 payload    type-specific:
//!                Open:       empty
//!                Close:      empty — closes the sender's write leg
//!                            (half-close); the stream ends when both legs
//!                            are closed
//!                Data:       application bytes
//!                Window:     u64 big-endian credit delta (bytes newly
//!                            granted)
//!                Register:   UTF-8 node id (<= MAX_NODE_ID_LEN bytes); the
//!                            relay requires it to equal the peer's mTLS
//!                            certificate fingerprint
//!                Unregister: empty — graceful node shutdown
//!                Heartbeat:  empty — liveness, one per heartbeat interval
//!                OpenStream: UTF-8 target node id (<= MAX_NODE_ID_LEN) —
//!                            node→relay: open a routed stream to the
//!                            target; relay→node: a stream opened by the
//!                            given peer (stream_id is relay-allocated,
//!                            even)
//!                Error:      u16 big-endian code + UTF-8 message
//!                            (structured reason, see relay_routing)
//!                CloseStream: empty — full teardown of the routed stream
//!                            `stream_id`, both directions
//!                Enroll:       UTF-8 enrollment token (issued by
//!                            `relay invite`); first frame on the
//!                            enrollment listener connection only
//!                EnrollResponse: UTF-8 relay certificate fingerprint the
//!                            joining node pins into relay.toml
//!                Presence:   UTF-8 node id (<= MAX_NODE_ID_LEN) + 1 byte
//!                            bool (0 = offline, nonzero = online);
//!                            relay→node only: a watched node's presence
//!                            transition
//!                Watch:      UTF-8 node id (<= MAX_NODE_ID_LEN);
//!                            node→relay only: subscribe to presence
//!                            transitions of the node
//! ```
//!
//! `Close` is the half-close: the sender will send no more `Data` on this
//! stream; the peer's read side reaches EOF once buffered data is drained
//! while its own write side stays usable until it also closes.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{MuxError, MAX_FRAME_PAYLOAD};

/// Fixed header length in bytes.
pub const HEADER_LEN: usize = 10;

/// Maximum byte length of a Register node id payload.
pub const MAX_NODE_ID_LEN: u32 = 256;

const TYPE_OPEN: u8 = 0x01;
const TYPE_CLOSE: u8 = 0x02;
const TYPE_DATA: u8 = 0x03;
const TYPE_WINDOW: u8 = 0x04;
const TYPE_REGISTER: u8 = 0x05;
const TYPE_UNREGISTER: u8 = 0x06;
const TYPE_HEARTBEAT: u8 = 0x07;
const TYPE_OPEN_STREAM: u8 = 0x08;
const TYPE_ERROR: u8 = 0x09;
const TYPE_CLOSE_STREAM: u8 = 0x0A;
const TYPE_ENROLL: u8 = 0x0B;
const TYPE_ENROLL_RESPONSE: u8 = 0x0C;
const TYPE_PRESENCE: u8 = 0x0D;
const TYPE_WATCH: u8 = 0x0E;

/// Frame type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Open,
    Close,
    Data,
    Window,
    Register,
    Unregister,
    Heartbeat,
    OpenStream,
    Error,
    CloseStream,
    Enroll,
    EnrollResponse,
    Presence,
    Watch,
}

impl FrameType {
    fn wire_value(self) -> u8 {
        match self {
            FrameType::Open => TYPE_OPEN,
            FrameType::Close => TYPE_CLOSE,
            FrameType::Data => TYPE_DATA,
            FrameType::Window => TYPE_WINDOW,
            FrameType::Register => TYPE_REGISTER,
            FrameType::Unregister => TYPE_UNREGISTER,
            FrameType::Heartbeat => TYPE_HEARTBEAT,
            FrameType::OpenStream => TYPE_OPEN_STREAM,
            FrameType::Error => TYPE_ERROR,
            FrameType::CloseStream => TYPE_CLOSE_STREAM,
            FrameType::Enroll => TYPE_ENROLL,
            FrameType::EnrollResponse => TYPE_ENROLL_RESPONSE,
            FrameType::Presence => TYPE_PRESENCE,
            FrameType::Watch => TYPE_WATCH,
        }
    }

    fn from_wire(value: u8) -> Result<Self, MuxError> {
        match value {
            TYPE_OPEN => Ok(FrameType::Open),
            TYPE_CLOSE => Ok(FrameType::Close),
            TYPE_DATA => Ok(FrameType::Data),
            TYPE_WINDOW => Ok(FrameType::Window),
            TYPE_REGISTER => Ok(FrameType::Register),
            TYPE_UNREGISTER => Ok(FrameType::Unregister),
            TYPE_HEARTBEAT => Ok(FrameType::Heartbeat),
            TYPE_OPEN_STREAM => Ok(FrameType::OpenStream),
            TYPE_ERROR => Ok(FrameType::Error),
            TYPE_CLOSE_STREAM => Ok(FrameType::CloseStream),
            TYPE_ENROLL => Ok(FrameType::Enroll),
            TYPE_ENROLL_RESPONSE => Ok(FrameType::EnrollResponse),
            TYPE_PRESENCE => Ok(FrameType::Presence),
            TYPE_WATCH => Ok(FrameType::Watch),
            other => Err(MuxError::InvalidFrameType(other)),
        }
    }
}

/// A decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Open a new stream; `stream_id` must use the initiator's parity.
    Open { stream_id: u32 },
    /// Half-close the sender's write leg of `stream_id`.
    Close { stream_id: u32 },
    /// Application bytes for `stream_id`.
    Data { stream_id: u32, payload: Vec<u8> },
    /// Grant `credit` additional send bytes to the peer's write leg of
    /// `stream_id`.
    Window { stream_id: u32, credit: u64 },
    /// Relay control: register this link as `node_id` (must equal the
    /// peer's mTLS certificate fingerprint on the relay link).
    Register { node_id: String },
    /// Relay control: graceful node shutdown.
    Unregister,
    /// Relay control: liveness ping from a registered node.
    Heartbeat,
    /// Relay control: open a routed stream toward `target_node_id`;
    /// `stream_id` is the routed stream id (odd = node-initiated, even =
    /// relay-initiated on relay links).
    OpenStream {
        stream_id: u32,
        target_node_id: String,
    },
    /// Relay control: structured error concerning `stream_id` (0 = none).
    Error {
        stream_id: u32,
        code: u16,
        message: String,
    },
    /// Relay control: full teardown of the routed stream `stream_id`.
    CloseStream { stream_id: u32 },
    /// Enrollment: first frame on an enrollment-listener connection; carries
    /// the invite/deploy token issued by `relay invite`.
    Enroll { token: String },
    /// Enrollment: the relay's certificate fingerprint, delivered over the
    /// token-authenticated enrollment session for the node to pin.
    EnrollResponse { fingerprint: String },
    /// Presence: relay→node only; a watched node's presence transitioned.
    /// Payload: node id + 1 byte bool (see the module header table).
    Presence {
        /// The node whose presence changed (its certificate fingerprint).
        node_id: String,
        /// `true` when the node came online, `false` when it went offline.
        online: bool,
    },
    /// Watch: node→relay only; subscribe to presence transitions of the
    /// node. The relay answers immediately with a `Presence` replay frame
    /// and then on every transition.
    Watch {
        /// The node to watch (its certificate fingerprint).
        node_id: String,
    },
}

impl Frame {
    /// Returns the stream id the frame belongs to (0 for stream-less
    /// control frames).
    pub fn stream_id(&self) -> u32 {
        match self {
            Frame::Open { stream_id }
            | Frame::Close { stream_id }
            | Frame::Data { stream_id, .. }
            | Frame::Window { stream_id, .. }
            | Frame::OpenStream { stream_id, .. }
            | Frame::Error { stream_id, .. }
            | Frame::CloseStream { stream_id } => *stream_id,
            Frame::Register { .. }
            | Frame::Unregister
            | Frame::Heartbeat
            | Frame::Enroll { .. }
            | Frame::EnrollResponse { .. }
            | Frame::Presence { .. }
            | Frame::Watch { .. } => 0,
        }
    }

    fn frame_type(&self) -> FrameType {
        match self {
            Frame::Open { .. } => FrameType::Open,
            Frame::Close { .. } => FrameType::Close,
            Frame::Data { .. } => FrameType::Data,
            Frame::Window { .. } => FrameType::Window,
            Frame::Register { .. } => FrameType::Register,
            Frame::Unregister => FrameType::Unregister,
            Frame::Heartbeat => FrameType::Heartbeat,
            Frame::OpenStream { .. } => FrameType::OpenStream,
            Frame::Error { .. } => FrameType::Error,
            Frame::CloseStream { .. } => FrameType::CloseStream,
            Frame::Enroll { .. } => FrameType::Enroll,
            Frame::EnrollResponse { .. } => FrameType::EnrollResponse,
            Frame::Presence { .. } => FrameType::Presence,
            Frame::Watch { .. } => FrameType::Watch,
        }
    }

    fn payload_len(&self) -> usize {
        match self {
            Frame::Open { .. } | Frame::Close { .. } => 0,
            Frame::Data { payload, .. } => payload.len(),
            Frame::Window { .. } => 8,
            Frame::Register { node_id }
            | Frame::OpenStream {
                target_node_id: node_id,
                ..
            } => node_id.len(),
            Frame::Error { message, .. } => 2 + message.len(),
            Frame::Unregister | Frame::Heartbeat | Frame::CloseStream { .. } => 0,
            Frame::Enroll { token } => token.len(),
            Frame::EnrollResponse { fingerprint } => fingerprint.len(),
            Frame::Presence { node_id, .. } => node_id.len() + 1,
            Frame::Watch { node_id } => node_id.len(),
        }
    }

    fn encode_payload(&self, out: &mut Vec<u8>) {
        match self {
            Frame::Data { payload, .. } => out.extend_from_slice(payload),
            Frame::Window { credit, .. } => out.extend_from_slice(&credit.to_be_bytes()),
            Frame::Register { node_id }
            | Frame::OpenStream {
                target_node_id: node_id,
                ..
            } => out.extend_from_slice(node_id.as_bytes()),
            Frame::Error { code, message, .. } => {
                out.extend_from_slice(&code.to_be_bytes());
                out.extend_from_slice(message.as_bytes());
            }
            Frame::Enroll { token } => out.extend_from_slice(token.as_bytes()),
            Frame::EnrollResponse { fingerprint } => out.extend_from_slice(fingerprint.as_bytes()),
            Frame::Presence { node_id, online } => {
                out.extend_from_slice(node_id.as_bytes());
                out.push(u8::from(*online));
            }
            Frame::Watch { node_id } => out.extend_from_slice(node_id.as_bytes()),
            Frame::Open { .. }
            | Frame::Close { .. }
            | Frame::Unregister
            | Frame::Heartbeat
            | Frame::CloseStream { .. } => {}
        }
    }

    /// Appends the encoded frame (header + payload) to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let payload_len = self.payload_len();
        out.reserve(HEADER_LEN + payload_len);
        out.push(self.frame_type().wire_value());
        out.push(0);
        out.extend_from_slice(&self.stream_id().to_be_bytes());
        out.extend_from_slice(&(payload_len as u32).to_be_bytes());
        self.encode_payload(out);
    }

    /// Decodes a frame from a fully read `header` and `payload`.
    pub fn decode(header: &[u8; HEADER_LEN], payload: &[u8]) -> Result<Self, MuxError> {
        let frame_type = FrameType::from_wire(header[0])?;
        let flags = header[1];
        if flags != 0 {
            return Err(MuxError::InvalidFlags(flags));
        }
        let stream_id = u32::from_be_bytes([header[2], header[3], header[4], header[5]]);
        let length = u32::from_be_bytes([header[6], header[7], header[8], header[9]]);
        if length as usize != payload.len() {
            return Err(MuxError::TruncatedFrame);
        }
        match frame_type {
            FrameType::Open if payload.is_empty() => Ok(Frame::Open { stream_id }),
            FrameType::Close if payload.is_empty() => Ok(Frame::Close { stream_id }),
            FrameType::Data => Ok(Frame::Data {
                stream_id,
                payload: payload.to_vec(),
            }),
            FrameType::Window => {
                let credit = payload
                    .try_into()
                    .map(u64::from_be_bytes)
                    .map_err(|_| MuxError::InvalidPayload(FrameType::Window, length))?;
                Ok(Frame::Window { stream_id, credit })
            }
            FrameType::Register => {
                if stream_id != 0 {
                    return Err(MuxError::InvalidStreamId(FrameType::Register, stream_id));
                }
                if length > MAX_NODE_ID_LEN {
                    return Err(MuxError::InvalidPayload(FrameType::Register, length));
                }
                let node_id = String::from_utf8(payload.to_vec())
                    .map_err(|_| MuxError::InvalidPayload(FrameType::Register, length))?;
                Ok(Frame::Register { node_id })
            }
            FrameType::Unregister if payload.is_empty() && stream_id == 0 => Ok(Frame::Unregister),
            FrameType::Heartbeat if payload.is_empty() && stream_id == 0 => Ok(Frame::Heartbeat),
            FrameType::Unregister => {
                Err(MuxError::InvalidStreamId(FrameType::Unregister, stream_id))
            }
            FrameType::Heartbeat => Err(MuxError::InvalidStreamId(FrameType::Heartbeat, stream_id)),
            FrameType::OpenStream => {
                if length > MAX_NODE_ID_LEN {
                    return Err(MuxError::InvalidPayload(FrameType::OpenStream, length));
                }
                let target_node_id = String::from_utf8(payload.to_vec())
                    .map_err(|_| MuxError::InvalidPayload(FrameType::OpenStream, length))?;
                Ok(Frame::OpenStream {
                    stream_id,
                    target_node_id,
                })
            }
            FrameType::Error => {
                let code_bytes: [u8; 2] = payload[..payload.len().min(2)]
                    .try_into()
                    .map_err(|_| MuxError::InvalidPayload(FrameType::Error, length))?;
                let code = u16::from_be_bytes(code_bytes);
                let message = String::from_utf8(payload[2.min(payload.len())..].to_vec())
                    .map_err(|_| MuxError::InvalidPayload(FrameType::Error, length))?;
                Ok(Frame::Error {
                    stream_id,
                    code,
                    message,
                })
            }
            FrameType::CloseStream if payload.is_empty() => Ok(Frame::CloseStream { stream_id }),
            FrameType::Enroll => {
                if stream_id != 0 {
                    return Err(MuxError::InvalidStreamId(FrameType::Enroll, stream_id));
                }
                let token = String::from_utf8(payload.to_vec())
                    .map_err(|_| MuxError::InvalidPayload(FrameType::Enroll, length))?;
                Ok(Frame::Enroll { token })
            }
            FrameType::EnrollResponse => {
                if stream_id != 0 {
                    return Err(MuxError::InvalidStreamId(
                        FrameType::EnrollResponse,
                        stream_id,
                    ));
                }
                let fingerprint = String::from_utf8(payload.to_vec())
                    .map_err(|_| MuxError::InvalidPayload(FrameType::EnrollResponse, length))?;
                Ok(Frame::EnrollResponse { fingerprint })
            }
            FrameType::Presence => {
                if stream_id != 0 {
                    return Err(MuxError::InvalidStreamId(FrameType::Presence, stream_id));
                }
                let Some((&online_byte, id_bytes)) = payload.split_last() else {
                    return Err(MuxError::InvalidPayload(FrameType::Presence, length));
                };
                if id_bytes.len() as u32 > MAX_NODE_ID_LEN {
                    return Err(MuxError::InvalidPayload(FrameType::Presence, length));
                }
                let node_id = String::from_utf8(id_bytes.to_vec())
                    .map_err(|_| MuxError::InvalidPayload(FrameType::Presence, length))?;
                Ok(Frame::Presence {
                    node_id,
                    online: online_byte != 0,
                })
            }
            FrameType::Watch => {
                if stream_id != 0 {
                    return Err(MuxError::InvalidStreamId(FrameType::Watch, stream_id));
                }
                if length > MAX_NODE_ID_LEN {
                    return Err(MuxError::InvalidPayload(FrameType::Watch, length));
                }
                let node_id = String::from_utf8(payload.to_vec())
                    .map_err(|_| MuxError::InvalidPayload(FrameType::Watch, length))?;
                Ok(Frame::Watch { node_id })
            }
            other => Err(MuxError::InvalidPayload(other, length)),
        }
    }
}

/// Reads one frame from `reader`.
///
/// Returns [`MuxError::TruncatedFrame`] on a clean end of input inside a
/// frame and [`MuxError::PayloadTooLong`] before allocating when the
/// declared payload exceeds [`MAX_FRAME_PAYLOAD`].
pub async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Result<Frame, MuxError> {
    let mut header = [0u8; HEADER_LEN];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof => MuxError::TruncatedFrame,
            _ => MuxError::Io(error),
        })?;
    let length = u32::from_be_bytes([header[6], header[7], header[8], header[9]]);
    if length > MAX_FRAME_PAYLOAD {
        return Err(MuxError::PayloadTooLong(length));
    }
    let mut payload = vec![0u8; length as usize];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof => MuxError::TruncatedFrame,
            _ => MuxError::Io(error),
        })?;
    Frame::decode(&header, &payload)
}

/// Encodes `frame` and writes it to `writer`, then flushes so framed traffic
/// is not delayed by buffering.
pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &Frame,
) -> Result<(), MuxError> {
    let mut buf = Vec::with_capacity(HEADER_LEN + frame.payload_len());
    frame.encode(&mut buf);
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        frame.encode(&mut out);
        out
    }

    #[test]
    fn open_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Open {
            stream_id: 0x0102_0304,
        });
        assert_eq!(
            bytes,
            vec![0x01, 0x00, 0x01, 0x02, 0x03, 0x04, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn close_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Close { stream_id: 7 });
        assert_eq!(
            bytes,
            vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn data_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Data {
            stream_id: 0x0000_00ff,
            payload: b"hi".to_vec(),
        });
        assert_eq!(
            bytes,
            vec![0x03, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0x02, b'h', b'i']
        );
    }

    #[test]
    fn window_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Window {
            stream_id: 1,
            credit: 0x0102_0304_0506_0708,
        });
        assert_eq!(
            bytes,
            vec![
                0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x08, 0x01, 0x02, 0x03, 0x04,
                0x05, 0x06, 0x07, 0x08
            ]
        );
    }

    #[test]
    fn all_frame_types_round_trip() {
        let frames = vec![
            Frame::Open { stream_id: 1 },
            Frame::Close { stream_id: 2 },
            Frame::Data {
                stream_id: 3,
                payload: vec![0xabu8; 300],
            },
            Frame::Window {
                stream_id: 4,
                credit: u64::MAX,
            },
        ];
        for frame in frames {
            let bytes = encoded(&frame);
            let mut slice = bytes.as_slice();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let decoded = runtime
                .block_on(read_frame(&mut slice))
                .expect("frame should decode");
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn decode_rejects_unknown_frame_type() {
        // 0x0F is unallocated; 0x01..=0x0E are all assigned frame types.
        let header = [0x0fu8, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        let error = Frame::decode(&header, &[]).expect_err("type 0x0F should be rejected");
        assert!(matches!(error, MuxError::InvalidFrameType(0x0f)));
    }

    #[test]
    fn decode_rejects_nonzero_reserved_flags() {
        let header = [0x01u8, 0x80, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        let error = Frame::decode(&header, &[]).expect_err("nonzero flags should be rejected");
        assert!(matches!(error, MuxError::InvalidFlags(0x80)));
    }

    #[test]
    fn decode_rejects_payload_length_mismatch() {
        let header = [0x01u8, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x04];
        let error = Frame::decode(&header, &[1, 2]).expect_err("short payload should fail");
        assert!(matches!(error, MuxError::TruncatedFrame));
    }

    #[test]
    fn decode_rejects_wrong_window_payload_length() {
        let header = [0x04u8, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x04];
        let error = Frame::decode(&header, &[0, 0, 0, 1]).expect_err("window needs 8 bytes");
        assert!(
            matches!(error, MuxError::InvalidPayload(FrameType::Window, 4)),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn read_frame_maps_eof_inside_header_to_truncated() {
        let mut slice: &[u8] = &[0x01, 0x00, 0x00];
        let error = read_frame(&mut slice)
            .await
            .expect_err("short header should fail");
        assert!(matches!(error, MuxError::TruncatedFrame));
    }

    #[tokio::test]
    async fn read_frame_maps_eof_inside_payload_to_truncated() {
        let mut slice: &[u8] = &[0x03, 0x00, 0, 0, 0, 1, 0, 0, 0, 4, 1, 2];
        let error = read_frame(&mut slice)
            .await
            .expect_err("short payload should fail");
        assert!(matches!(error, MuxError::TruncatedFrame));
    }

    #[tokio::test]
    async fn read_frame_rejects_oversize_payload_before_allocating() {
        let mut slice: &[u8] = &[0x03, 0x00, 0, 0, 0, 1, 0x00, 0x10, 0x00, 0x01];
        let error = read_frame(&mut slice)
            .await
            .expect_err("oversize payload should fail");
        assert!(matches!(error, MuxError::PayloadTooLong(0x0010_0001)));
    }

    #[tokio::test]
    async fn write_then_read_round_trips_over_duplex() {
        let (mut left, mut right) = tokio::io::duplex(1024);
        let frame = Frame::Data {
            stream_id: 9,
            payload: b"relay-mux".to_vec(),
        };
        write_frame(&mut left, &frame).await.expect("write");
        let decoded = read_frame(&mut right).await.expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn register_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Register {
            node_id: "node-a".to_string(),
        });
        assert_eq!(
            bytes,
            vec![
                0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, b'n', b'o', b'd', b'e',
                b'-', b'a'
            ]
        );
    }

    #[test]
    fn unregister_and_heartbeat_have_zero_stream_id_and_empty_payload() {
        let unregister = encoded(&Frame::Unregister);
        assert_eq!(
            unregister,
            vec![0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        let heartbeat = encoded(&Frame::Heartbeat);
        assert_eq!(
            heartbeat,
            vec![0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn control_frames_round_trip() {
        let frames = vec![
            Frame::Register {
                node_id: "7c857a105486f46defdfc06ffc2dbc54531a4cdb25515488565aae15264543e4"
                    .to_string(),
            },
            Frame::Unregister,
            Frame::Heartbeat,
        ];
        for frame in frames {
            let bytes = encoded(&frame);
            let mut slice = bytes.as_slice();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let decoded = runtime
                .block_on(read_frame(&mut slice))
                .expect("frame should decode");
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn decode_rejects_control_frames_with_nonzero_stream_id() {
        let header = [0x06u8, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00];
        let error = Frame::decode(&header, &[]).expect_err("stream id must be zero");
        assert!(matches!(
            error,
            MuxError::InvalidStreamId(FrameType::Unregister, 7)
        ));
    }

    #[test]
    fn decode_rejects_non_utf8_register_payload() {
        let header = [0x05u8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
        let error = Frame::decode(&header, &[0xff]).expect_err("node id must be utf-8");
        assert!(matches!(
            error,
            MuxError::InvalidPayload(FrameType::Register, 1)
        ));
    }

    #[test]
    fn open_stream_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::OpenStream {
            stream_id: 1,
            target_node_id: "node-b".to_string(),
        });
        assert_eq!(
            bytes,
            vec![
                0x08, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, b'n', b'o', b'd', b'e',
                b'-', b'b'
            ]
        );
    }

    #[test]
    fn error_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Error {
            stream_id: 9,
            code: 0x0002,
            message: "nope".to_string(),
        });
        assert_eq!(
            bytes,
            vec![
                0x09, 0x00, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x06, 0x00, 0x02, b'n', b'o',
                b'p', b'e'
            ]
        );
    }

    #[test]
    fn close_stream_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::CloseStream { stream_id: 4 });
        assert_eq!(
            bytes,
            vec![0x0A, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn routing_control_frames_round_trip() {
        let frames = vec![
            Frame::OpenStream {
                stream_id: 3,
                target_node_id: "peer-node".to_string(),
            },
            Frame::Error {
                stream_id: 3,
                code: u16::MAX,
                message: "structured reason".to_string(),
            },
            Frame::CloseStream { stream_id: 2 },
        ];
        for frame in frames {
            let bytes = encoded(&frame);
            let mut slice = bytes.as_slice();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let decoded = runtime
                .block_on(read_frame(&mut slice))
                .expect("frame should decode");
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn decode_rejects_error_payload_shorter_than_code() {
        let header = [0x09u8, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01];
        let error = Frame::decode(&header, &[0x00]).expect_err("error needs a 2-byte code");
        assert!(matches!(
            error,
            MuxError::InvalidPayload(FrameType::Error, 1)
        ));
    }

    #[test]
    fn enroll_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Enroll {
            token: "tok-1".to_string(),
        });
        assert_eq!(
            bytes,
            vec![
                0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, b't', b'o', b'k', b'-',
                b'1'
            ]
        );
    }

    #[test]
    fn enroll_response_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::EnrollResponse {
            fingerprint: "7c857a".to_string(),
        });
        assert_eq!(
            bytes,
            vec![
                0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, b'7', b'c', b'8', b'5',
                b'7', b'a'
            ]
        );
    }

    #[test]
    fn enrollment_frames_round_trip() {
        let frames = vec![
            Frame::Enroll {
                token: "inv-A9_b".to_string(),
            },
            Frame::EnrollResponse {
                fingerprint: "7c857a105486f46defdfc06ffc2dbc54531a4cdb25515488565aae15264543e4"
                    .to_string(),
            },
        ];
        for frame in frames {
            let bytes = encoded(&frame);
            let mut slice = bytes.as_slice();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let decoded = runtime
                .block_on(read_frame(&mut slice))
                .expect("frame should decode");
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn decode_rejects_enrollment_frames_with_nonzero_stream_id() {
        let header = [0x0bu8, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00];
        let error = Frame::decode(&header, &[]).expect_err("stream id must be zero");
        assert!(matches!(
            error,
            MuxError::InvalidStreamId(FrameType::Enroll, 7)
        ));
        let header = [0x0cu8, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00];
        let error = Frame::decode(&header, &[]).expect_err("stream id must be zero");
        assert!(matches!(
            error,
            MuxError::InvalidStreamId(FrameType::EnrollResponse, 7)
        ));
    }

    #[test]
    fn presence_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Presence {
            node_id: "node-a".to_string(),
            online: true,
        });
        assert_eq!(
            bytes,
            vec![
                0x0D, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, b'n', b'o', b'd', b'e',
                b'-', b'a', 0x01
            ]
        );
        let bytes = encoded(&Frame::Presence {
            node_id: "node-a".to_string(),
            online: false,
        });
        assert_eq!(
            bytes,
            vec![
                0x0D, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, b'n', b'o', b'd', b'e',
                b'-', b'a', 0x00
            ]
        );
    }

    #[test]
    fn watch_has_deterministic_golden_layout() {
        let bytes = encoded(&Frame::Watch {
            node_id: "node-b".to_string(),
        });
        assert_eq!(
            bytes,
            vec![
                0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, b'n', b'o', b'd', b'e',
                b'-', b'b'
            ]
        );
    }

    #[test]
    fn presence_and_watch_round_trip() {
        let frames = vec![
            Frame::Presence {
                node_id: "7c857a105486f46defdfc06ffc2dbc54531a4cdb25515488565aae15264543e4"
                    .to_string(),
                online: true,
            },
            Frame::Presence {
                node_id: "node-x".to_string(),
                online: false,
            },
            Frame::Watch {
                node_id: "node-y".to_string(),
            },
        ];
        for frame in frames {
            let bytes = encoded(&frame);
            let mut slice = bytes.as_slice();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let decoded = runtime
                .block_on(read_frame(&mut slice))
                .expect("frame should decode");
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn decode_rejects_presence_and_watch_with_nonzero_stream_id() {
        let header = [0x0du8, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x02];
        let error = Frame::decode(&header, &[b'a', 0x01]).expect_err("stream id must be zero");
        assert!(matches!(
            error,
            MuxError::InvalidStreamId(FrameType::Presence, 7)
        ));
        let header = [0x0eu8, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x01];
        let error = Frame::decode(&header, b"a").expect_err("stream id must be zero");
        assert!(matches!(
            error,
            MuxError::InvalidStreamId(FrameType::Watch, 7)
        ));
    }

    #[test]
    fn decode_rejects_empty_presence_payload() {
        // Presence needs at least the bool byte; an empty payload cannot
        // carry it.
        let header = [0x0du8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let error = Frame::decode(&header, &[]).expect_err("empty presence payload must fail");
        assert!(matches!(
            error,
            MuxError::InvalidPayload(FrameType::Presence, 0)
        ));
    }
}
