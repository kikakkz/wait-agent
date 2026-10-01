//! Mux frame codec: deterministic wire layout, pure encode/decode.
//!
//! Every frame is a fixed 10-byte big-endian header followed by a
//! type-specific payload:
//!
//! ```text
//! +0  type       u8    0x01 Open | 0x02 Close | 0x03 Data | 0x04 Window
//! +1  flags      u8    reserved, must be zero
//! +2  stream_id  u32   odd = client-initiated, even = server-initiated
//! +6  length     u32   payload length in bytes (<= MAX_FRAME_PAYLOAD)
//! +10 payload    type-specific:
//!                Open:   empty
//!                Close:  empty — closes the sender's write leg (half-close);
//!                        the stream ends when both legs are closed
//!                Data:   application bytes
//!                Window: u64 big-endian credit delta (bytes newly granted)
//! ```
//!
//! `Close` is the half-close: the sender will send no more `Data` on this
//! stream; the peer's read side reaches EOF once buffered data is drained
//! while its own write side stays usable until it also closes.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{MuxError, MAX_FRAME_PAYLOAD};

/// Fixed header length in bytes.
pub const HEADER_LEN: usize = 10;

const TYPE_OPEN: u8 = 0x01;
const TYPE_CLOSE: u8 = 0x02;
const TYPE_DATA: u8 = 0x03;
const TYPE_WINDOW: u8 = 0x04;

/// Mux frame type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Open,
    Close,
    Data,
    Window,
}

impl FrameType {
    fn wire_value(self) -> u8 {
        match self {
            FrameType::Open => TYPE_OPEN,
            FrameType::Close => TYPE_CLOSE,
            FrameType::Data => TYPE_DATA,
            FrameType::Window => TYPE_WINDOW,
        }
    }

    fn from_wire(value: u8) -> Result<Self, MuxError> {
        match value {
            TYPE_OPEN => Ok(FrameType::Open),
            TYPE_CLOSE => Ok(FrameType::Close),
            TYPE_DATA => Ok(FrameType::Data),
            TYPE_WINDOW => Ok(FrameType::Window),
            other => Err(MuxError::InvalidFrameType(other)),
        }
    }
}

/// A decoded mux frame.
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
}

impl Frame {
    /// Returns the stream id the frame belongs to.
    pub fn stream_id(&self) -> u32 {
        match self {
            Frame::Open { stream_id }
            | Frame::Close { stream_id }
            | Frame::Data { stream_id, .. }
            | Frame::Window { stream_id, .. } => *stream_id,
        }
    }

    fn frame_type(&self) -> FrameType {
        match self {
            Frame::Open { .. } => FrameType::Open,
            Frame::Close { .. } => FrameType::Close,
            Frame::Data { .. } => FrameType::Data,
            Frame::Window { .. } => FrameType::Window,
        }
    }

    fn payload_len(&self) -> usize {
        match self {
            Frame::Open { .. } | Frame::Close { .. } => 0,
            Frame::Data { payload, .. } => payload.len(),
            Frame::Window { .. } => 8,
        }
    }

    fn encode_payload(&self, out: &mut Vec<u8>) {
        match self {
            Frame::Data { payload, .. } => out.extend_from_slice(payload),
            Frame::Window { credit, .. } => out.extend_from_slice(&credit.to_be_bytes()),
            Frame::Open { .. } | Frame::Close { .. } => {}
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
        let header = [0x09u8, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        let error = Frame::decode(&header, &[]).expect_err("type 0x09 should be rejected");
        assert!(matches!(error, MuxError::InvalidFrameType(0x09)));
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
}
