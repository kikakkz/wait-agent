//! `MuxStream`: one multiplexed stream.
//!
//! Implements [`AsyncRead`] + [`AsyncWrite`] over the mux connection's frame
//! channel, so it becomes a [`PeerConnection`](crate::infra::peer_connection)
//! through the blanket impl — the property that lets the node-to-node inner
//! TLS run over a relay stream unmodified.
//!
//! Semantics mirror TCP over the connection:
//! - reads return buffered data in order, then EOF once the peer has
//!   half-closed and the buffer is drained;
//! - writes consume per-stream credit and park when it is exhausted;
//! - [`AsyncWrite::poll_shutdown`] half-closes the write leg (sends `Close`);
//!   after that, writes fail and the stream ends when both legs are closed.
//!
//! Locking: the per-stream [`Mutex`] is a leaf lock (never held across an
//! `.await` and never taken while holding the connection's stream-table
//! lock); see the lock-order table in the module docs.

use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

use super::connection::{ConnectionShared, StreamEntry};
use super::frame::Frame;
use super::{MuxError, MAX_FRAME_PAYLOAD, WINDOW_GRANT_THRESHOLD};

/// Shared per-stream state, guarded by one leaf mutex.
pub(crate) struct StreamState {
    /// Send credit the peer has granted us (via initial window + `Window`).
    pub(crate) peer_credit: u64,
    /// We half-closed our write leg; further writes fail.
    pub(crate) fin_sent: bool,
    /// Bytes consumed by the reader that are not yet granted back to the
    /// peer. If a `Window` frame cannot be queued because the outbound
    /// channel is momentarily full, the amount stays here and is retried on
    /// the next read — grants are deferred, never lost.
    pub(crate) consumed_accum: u64,
    /// Writer parked on exhausted credit, to wake on `Window`.
    pub(crate) write_waker: Option<Waker>,
}

/// One leg of a multiplexed stream.
///
/// Obtained from [`MuxConnection::open_stream`](super::MuxConnection::open_stream)
/// or [`MuxConnection::accept`](super::MuxConnection::accept).
pub struct MuxStream {
    id: u32,
    shared: Arc<ConnectionShared>,
    state: Arc<Mutex<StreamState>>,
    inbound: mpsc::Receiver<Vec<u8>>,
    peer_fin: Arc<std::sync::atomic::AtomicBool>,
    /// Tail of the last popped chunk that did not fit the caller's buffer.
    read_leftover: Vec<u8>,
    read_offset: usize,
}

// Compile-time seam assertion: a mux'd stream IS a PeerConnection, so inner
// TLS and the session protocols run over it without any glue.
const _: () = {
    fn assert_peer_connection<T: crate::infra::peer_connection::PeerConnection>() {}
    fn assert() {
        assert_peer_connection::<MuxStream>();
    }
};

impl MuxStream {
    /// Builds the stream handle together with its table entry; the entry is
    /// what the reader task uses to reach this stream's queues.
    pub(crate) fn new(id: u32, shared: Arc<ConnectionShared>) -> (MuxStream, StreamEntry) {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(super::INBOUND_CHUNK_QUEUE);
        let state = Arc::new(Mutex::new(StreamState {
            peer_credit: super::DEFAULT_STREAM_WINDOW,
            fin_sent: false,
            consumed_accum: 0,
            write_waker: None,
        }));
        let peer_fin = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stream = MuxStream {
            id,
            shared,
            state: state.clone(),
            inbound: inbound_rx,
            peer_fin: peer_fin.clone(),
            read_leftover: Vec::new(),
            read_offset: 0,
        };
        let entry = StreamEntry {
            inbound: inbound_tx,
            state,
            peer_fin,
        };
        (stream, entry)
    }

    /// Returns the stream id (odd for client-initiated, even for
    /// server-initiated).
    pub fn id(&self) -> u32 {
        self.id
    }

    fn lock_state(&self) -> std::io::Result<MutexGuard<'_, StreamState>> {
        self.state
            .lock()
            .map_err(|_| std::io::Error::other("mux stream state lock poisoned"))
    }

    fn connection_dead_error(&self) -> std::io::Error {
        let reason = self
            .shared
            .close_reason
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| "connection closed".to_string());
        MuxError::ConnectionClosed(reason).into()
    }

    /// Registers consumed bytes and, past the grant threshold, queues a
    /// `Window` frame granting the peer more credit.
    fn account_consumed(&self, consumed: u64) {
        let delta = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.consumed_accum += consumed;
            if state.consumed_accum >= WINDOW_GRANT_THRESHOLD {
                let delta = state.consumed_accum;
                state.consumed_accum = 0;
                delta
            } else {
                0
            }
        };
        if delta == 0 {
            return;
        }
        let frame = Frame::Window {
            stream_id: self.id,
            credit: delta,
        };
        match self.shared.outbound.try_send(frame) {
            Ok(()) => {}
            // Defer the grant: keep the accumulated amount and retry on the
            // next read. The sender still has its previously granted credit,
            // so this cannot deadlock the stream.
            Err(mpsc::error::TrySendError::Full(frame)) => {
                if let Ok(mut state) = self.state.lock() {
                    if let Frame::Window { credit, .. } = frame {
                        state.consumed_accum += credit;
                    }
                }
            }
            // Connection is dead; further grants are moot.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Parks the current task until the writer task drains the outbound
    /// queue, then wakes it to retry the send.
    fn park_outbound(&self, cx: &Context<'_>) {
        if let Ok(mut waker) = self.shared.outbound_waker.lock() {
            *waker = Some(cx.waker().clone());
        }
    }
}

impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.read_offset < self.read_leftover.len() {
            let end = (self.read_offset + buf.remaining()).min(self.read_leftover.len());
            buf.put_slice(&self.read_leftover[self.read_offset..end]);
            self.read_offset = end;
            return Poll::Ready(Ok(()));
        }
        match self.inbound.poll_recv(cx) {
            Poll::Ready(Some(chunk)) => {
                let n = buf.remaining().min(chunk.len());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.read_leftover = chunk;
                    self.read_offset = n;
                }
                self.account_consumed(n as u64);
                Poll::Ready(Ok(()))
            }
            // The reader task dropped its sender half: the connection is
            // gone. Deliver an error (reset) when a reason was recorded, and
            // a clean EOF otherwise (peer shut the connection down).
            Poll::Ready(None) => {
                if self.shared.dead.load(Ordering::Acquire) {
                    Poll::Ready(Err(self.connection_dead_error()))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            // No buffered data. `Close` was the last frame the peer will
            // send on this stream, so an empty queue here means EOF.
            Poll::Pending => {
                if self.peer_fin.load(Ordering::Acquire) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.shared.dead.load(Ordering::Acquire) {
            return Poll::Ready(Err(self.connection_dead_error()));
        }
        let n = {
            let mut state = self.lock_state()?;
            if state.fin_sent {
                return Poll::Ready(Err(MuxError::StreamClosed(self.id).into()));
            }
            if state.peer_credit == 0 {
                state.write_waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            buf.len()
                .min(MAX_FRAME_PAYLOAD as usize)
                .min(state.peer_credit as usize)
        };
        let frame = Frame::Data {
            stream_id: self.id,
            payload: buf[..n].to_vec(),
        };
        match self.shared.outbound.try_send(frame) {
            Ok(()) => {
                let mut state = self.lock_state()?;
                state.peer_credit -= n as u64;
                Poll::Ready(Ok(n))
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.park_outbound(cx);
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Poll::Ready(Err(self.connection_dead_error()))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Frames are handed to the writer task, which flushes the
        // connection after every frame; there is no per-stream buffer.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        {
            let state = self.lock_state()?;
            if state.fin_sent {
                return Poll::Ready(Ok(()));
            }
        }
        if self.shared.dead.load(Ordering::Acquire) {
            return Poll::Ready(Err(self.connection_dead_error()));
        }
        match self
            .shared
            .outbound
            .try_send(Frame::Close { stream_id: self.id })
        {
            Ok(()) => {
                let mut state = self.lock_state()?;
                state.fin_sent = true;
                Poll::Ready(Ok(()))
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.park_outbound(cx);
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                let mut state = self.lock_state()?;
                state.fin_sent = true;
                Poll::Ready(Err(self.connection_dead_error()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_is_peer_connection() {
        fn assert<T: crate::infra::peer_connection::PeerConnection>() {}
        assert::<MuxStream>();
    }
}
