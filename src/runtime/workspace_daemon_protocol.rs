use crate::lifecycle::LifecycleError;
use crate::pty::PtySize;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

#[derive(Debug, Clone)]
pub(crate) enum Frame {
    Attach(PtySize),
    Input(Vec<u8>),
    Resize(PtySize),
    SnapshotRequest,
    StatusRequest,
    DetachRequest,
    Ack(String),
    Snapshot(Vec<u8>),
    Output(Vec<u8>),
    StatusResponse(String),
    Error(String),
}

const FRAME_ATTACH: u8 = 1;
const FRAME_INPUT: u8 = 2;
const FRAME_RESIZE: u8 = 3;
const FRAME_SNAPSHOT_REQUEST: u8 = 4;
const FRAME_STATUS_REQUEST: u8 = 5;
const FRAME_DETACH_REQUEST: u8 = 6;
const FRAME_ACK: u8 = 101;
const FRAME_SNAPSHOT: u8 = 102;
const FRAME_OUTPUT: u8 = 103;
const FRAME_STATUS_RESPONSE: u8 = 104;
const FRAME_ERROR: u8 = 105;

pub(crate) fn write_frame(stream: &mut UnixStream, frame: &Frame) -> Result<(), LifecycleError> {
    let (tag, payload) = match frame {
        Frame::Attach(size) => (FRAME_ATTACH, encode_size(*size)),
        Frame::Input(bytes) => (FRAME_INPUT, bytes.clone()),
        Frame::Resize(size) => (FRAME_RESIZE, encode_size(*size)),
        Frame::SnapshotRequest => (FRAME_SNAPSHOT_REQUEST, Vec::new()),
        Frame::StatusRequest => (FRAME_STATUS_REQUEST, Vec::new()),
        Frame::DetachRequest => (FRAME_DETACH_REQUEST, Vec::new()),
        Frame::Ack(text) => (FRAME_ACK, text.as_bytes().to_vec()),
        Frame::Snapshot(bytes) => (FRAME_SNAPSHOT, bytes.clone()),
        Frame::Output(bytes) => (FRAME_OUTPUT, bytes.clone()),
        Frame::StatusResponse(text) => (FRAME_STATUS_RESPONSE, text.as_bytes().to_vec()),
        Frame::Error(text) => (FRAME_ERROR, text.as_bytes().to_vec()),
    };

    let mut header = [0_u8; 5];
    header[0] = tag;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header).map_err(|error| {
        LifecycleError::Io("failed to write daemon frame header".to_string(), error)
    })?;
    if !payload.is_empty() {
        stream.write_all(&payload).map_err(|error| {
            LifecycleError::Io("failed to write daemon frame payload".to_string(), error)
        })?;
    }
    stream
        .flush()
        .map_err(|error| LifecycleError::Io("failed to flush daemon frame".to_string(), error))?;
    Ok(())
}

pub(crate) fn read_frame(stream: &mut UnixStream) -> Result<Frame, LifecycleError> {
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header).map_err(|error| {
        LifecycleError::Io("failed to read daemon frame header".to_string(), error)
    })?;
    let tag = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0_u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).map_err(|error| {
            LifecycleError::Io("failed to read daemon frame payload".to_string(), error)
        })?;
    }

    match tag {
        FRAME_ATTACH => Ok(Frame::Attach(decode_size(&payload)?)),
        FRAME_INPUT => Ok(Frame::Input(payload)),
        FRAME_RESIZE => Ok(Frame::Resize(decode_size(&payload)?)),
        FRAME_SNAPSHOT_REQUEST => Ok(Frame::SnapshotRequest),
        FRAME_STATUS_REQUEST => Ok(Frame::StatusRequest),
        FRAME_DETACH_REQUEST => Ok(Frame::DetachRequest),
        FRAME_ACK => Ok(Frame::Ack(String::from_utf8(payload).map_err(|_| {
            LifecycleError::Protocol("invalid utf-8 in daemon ack".to_string())
        })?)),
        FRAME_SNAPSHOT => Ok(Frame::Snapshot(payload)),
        FRAME_OUTPUT => Ok(Frame::Output(payload)),
        FRAME_STATUS_RESPONSE => Ok(Frame::StatusResponse(String::from_utf8(payload).map_err(
            |_| LifecycleError::Protocol("invalid utf-8 in daemon status response".to_string()),
        )?)),
        FRAME_ERROR => Ok(Frame::Error(String::from_utf8(payload).map_err(|_| {
            LifecycleError::Protocol("invalid utf-8 in daemon error".to_string())
        })?)),
        other => Err(LifecycleError::Protocol(format!(
            "unknown daemon frame tag: {other}"
        ))),
    }
}

fn encode_size(size: PtySize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&size.rows.to_be_bytes());
    payload.extend_from_slice(&size.cols.to_be_bytes());
    payload.extend_from_slice(&size.pixel_width.to_be_bytes());
    payload.extend_from_slice(&size.pixel_height.to_be_bytes());
    payload
}

fn decode_size(bytes: &[u8]) -> Result<PtySize, LifecycleError> {
    if bytes.len() != 8 {
        return Err(LifecycleError::Protocol(format!(
            "invalid size payload length: {}",
            bytes.len()
        )));
    }
    Ok(PtySize {
        rows: u16::from_be_bytes([bytes[0], bytes[1]]),
        cols: u16::from_be_bytes([bytes[2], bytes[3]]),
        pixel_width: u16::from_be_bytes([bytes[4], bytes[5]]),
        pixel_height: u16::from_be_bytes([bytes[6], bytes[7]]),
    })
}
