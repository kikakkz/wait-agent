//! [`MuxConnection`]: multiplexes [`MuxStream`]s over one ordered reliable
//! duplex (the `PeerConnection` seam), plus the reader/writer loops and the
//! per-connection stream table. See the module docs for the frame layout,
//! the lock-order table, and the teardown semantics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Waker;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::frame::{read_frame, write_frame, Frame};
use super::stream::{MuxStream, StreamState};
use super::{first_stream_id, owns_stream_id, MuxError, ACCEPT_QUEUE, OUTBOUND_QUEUE};

/// Which side of the connection this handle is. The client allocates odd
/// stream ids, the server even ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxRole {
    Client,
    Server,
}

/// Per-stream state as stored in the connection's stream table. Dropping an
/// entry drops the inbound sender, which the stream's reader observes as the
/// end of the stream.
#[derive(Clone)]
pub(crate) struct StreamEntry {
    pub(crate) inbound: mpsc::Sender<Vec<u8>>,
    pub(crate) state: Arc<Mutex<StreamState>>,
    pub(crate) peer_fin: Arc<AtomicBool>,
}

pub(crate) type StreamTable = HashMap<u32, StreamEntry>;

/// Connection-wide shared state. Fields are documented at the lock-order
/// table in the module docs; every mutex here is a leaf and no lock in this
/// struct is held across an `.await`.
pub(crate) struct ConnectionShared {
    /// Queue feeding the writer task.
    pub(crate) outbound: mpsc::Sender<Frame>,
    /// Single-slot waker for senders parked on a full outbound queue.
    pub(crate) outbound_waker: Mutex<Option<Waker>>,
    /// Set once the connection has failed or shut down.
    pub(crate) dead: AtomicBool,
    /// Why the connection died (also distinguishes reset from clean EOF).
    pub(crate) close_reason: Mutex<Option<String>>,
    /// Signals both loops to exit on failure.
    shutdown_tx: watch::Sender<bool>,
    role: MuxRole,
    next_local_id: AtomicU32,
}

impl ConnectionShared {
    pub(crate) fn role(&self) -> MuxRole {
        self.role
    }

    pub(crate) fn wake_outbound(&self) {
        let waker = self
            .outbound_waker
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn dead_reason(&self) -> String {
        self.close_reason
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| "connection closed".to_string())
    }
}

fn lock_table(table: &Mutex<StreamTable>) -> Result<MutexGuard<'_, StreamTable>, MuxError> {
    table
        .lock()
        .map_err(|_| MuxError::ConnectionClosed("mux stream table lock poisoned".to_string()))
}

/// Fails the connection: records the first reason, signals the loops, and
/// fails every live stream. Wakers are collected under the locks and woken
/// after they are released (no callbacks while holding a lock). Lock order:
/// `dead`/`close_reason`, then stream table, then per-stream state.
fn fail_connection(
    shared: &ConnectionShared,
    table: &Mutex<StreamTable>,
    reason: impl Into<String>,
) {
    if shared.dead.swap(true, Ordering::SeqCst) {
        return; // the first failure recorded the reason; keep it
    }
    if let Ok(mut guard) = shared.close_reason.lock() {
        *guard = Some(reason.into());
    }
    let _ = shared.shutdown_tx.send(true);
    let wakers: Vec<Waker> = {
        let Ok(mut table) = table.lock() else {
            return;
        };
        table
            .drain()
            .filter_map(|(_, entry)| {
                entry
                    .state
                    .lock()
                    .ok()
                    .and_then(|mut state| state.write_waker.take())
            })
            .collect()
    };
    for waker in wakers {
        waker.wake();
    }
}

/// A multiplexed connection over one ordered reliable duplex.
///
/// Spawn with [`MuxConnection::spawn`]; open local streams with
/// [`MuxConnection::open_stream`] and consume peer streams with
/// [`MuxConnection::accept`]. Dropping the connection shuts it down abruptly
/// (see the module docs on teardown).
pub struct MuxConnection {
    shared: Arc<ConnectionShared>,
    table: Arc<Mutex<StreamTable>>,
    accept_rx: mpsc::Receiver<MuxStream>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl MuxConnection {
    /// Spawns the reader and writer loops over `io`.
    pub fn spawn(io: impl AsyncRead + AsyncWrite + Send + Unpin + 'static, role: MuxRole) -> Self {
        let (reader, writer) = tokio::io::split(io);
        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let (accept_tx, accept_rx) = mpsc::channel(ACCEPT_QUEUE);
        let (shutdown_tx, shutdown_rx_reader) = watch::channel(false);
        let shutdown_rx_writer = shutdown_tx.subscribe();
        let shared = Arc::new(ConnectionShared {
            outbound: outbound_tx,
            outbound_waker: Mutex::new(None),
            dead: AtomicBool::new(false),
            close_reason: Mutex::new(None),
            shutdown_tx,
            role,
            next_local_id: AtomicU32::new(first_stream_id(role).get()),
        });
        let table = Arc::new(Mutex::new(HashMap::new()));
        let reader_task = tokio::spawn(reader_loop(
            reader,
            shared.clone(),
            table.clone(),
            accept_tx,
            shutdown_rx_reader,
        ));
        let writer_task = tokio::spawn(writer_loop(
            writer,
            shared.clone(),
            table.clone(),
            outbound_rx,
            shutdown_rx_writer,
        ));
        MuxConnection {
            shared,
            table,
            accept_rx,
            reader_task,
            writer_task,
        }
    }

    /// Returns whether the connection has failed or shut down.
    pub fn is_closed(&self) -> bool {
        self.shared.dead.load(Ordering::Acquire)
    }

    /// Opens a new stream toward the peer.
    pub async fn open_stream(&self) -> Result<MuxStream, MuxError> {
        if self.shared.dead.load(Ordering::Acquire) {
            return Err(MuxError::ConnectionClosed(self.shared.dead_reason()));
        }
        // Wraps after ~2^31 streams; a colliding id is still live would be a
        // protocol state bug, and the occupied check below surfaces it.
        let stream_id = self.shared.next_local_id.fetch_add(2, Ordering::SeqCst);
        let (stream, entry) = MuxStream::new(stream_id, self.shared.clone());
        let occupied = lock_table(&self.table)?.insert(stream_id, entry).is_some();
        if occupied {
            fail_connection(
                &self.shared,
                &self.table,
                format!("stream id {stream_id} is still live"),
            );
            return Err(MuxError::ProtocolViolation(format!(
                "stream id {stream_id} is still live"
            )));
        }
        if self
            .shared
            .outbound
            .send(Frame::Open { stream_id })
            .await
            .is_err()
        {
            let _ = lock_table(&self.table).map(|mut table| table.remove(&stream_id));
            return Err(MuxError::ConnectionClosed(self.shared.dead_reason()));
        }
        Ok(stream)
    }

    /// Accepts the next stream opened by the peer. Returns `None` once the
    /// connection is closed and all pending streams have been delivered.
    pub async fn accept(&mut self) -> Option<MuxStream> {
        self.accept_rx.recv().await
    }
}

impl Drop for MuxConnection {
    fn drop(&mut self) {
        // No connection-close frame exists in the four-frame design, so
        // local shutdown is abrupt: abort the loops and let live streams
        // observe ConnectionClosed / EOF. A graceful close belongs with the
        // relay control frames (later slice).
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

async fn reader_loop(
    mut reader: tokio::io::ReadHalf<impl AsyncRead + Unpin>,
    shared: Arc<ConnectionShared>,
    table: Arc<Mutex<StreamTable>>,
    accept_tx: mpsc::Sender<MuxStream>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let frame = tokio::select! {
            _ = shutdown.changed() => return,
            result = read_frame(&mut reader) => match result {
                Ok(frame) => frame,
                Err(error) => {
                    fail_connection(&shared, &table, error.to_string());
                    return;
                }
            },
        };
        match frame {
            Frame::Open { stream_id } => {
                if owns_stream_id(shared.role(), stream_id) {
                    fail_connection(
                        &shared,
                        &table,
                        format!("peer opened stream {stream_id} with local id parity"),
                    );
                    return;
                }
                let (stream, entry) = MuxStream::new(stream_id, shared.clone());
                let occupied = match lock_table(&table) {
                    Ok(mut table) => table.insert(stream_id, entry).is_some(),
                    Err(error) => {
                        fail_connection(&shared, &table, error.to_string());
                        return;
                    }
                };
                if occupied {
                    fail_connection(
                        &shared,
                        &table,
                        format!("peer reopened live stream {stream_id}"),
                    );
                    return;
                }
                if accept_tx.send(stream).await.is_err() {
                    fail_connection(
                        &shared,
                        &table,
                        "connection handle dropped while accepting a stream",
                    );
                    return;
                }
            }
            Frame::Data { stream_id, payload } => {
                let Some(entry) = lookup(&shared, &table, stream_id) else {
                    return;
                };
                if entry.peer_fin.load(Ordering::Acquire) {
                    fail_connection(
                        &shared,
                        &table,
                        format!("peer sent data on closed stream {stream_id}"),
                    );
                    return;
                }
                if entry.inbound.send(payload).await.is_err() {
                    fail_connection(
                        &shared,
                        &table,
                        format!(
                            "stream {stream_id} dropped by the local reader while the peer was writing"
                        ),
                    );
                    return;
                }
            }
            Frame::Close { stream_id } => {
                let Some(entry) = lookup(&shared, &table, stream_id) else {
                    return;
                };
                if entry.peer_fin.swap(true, Ordering::SeqCst) {
                    fail_connection(
                        &shared,
                        &table,
                        format!("peer closed stream {stream_id} twice"),
                    );
                    return;
                }
                let both_closed = entry
                    .state
                    .lock()
                    .map(|state| state.fin_sent)
                    .unwrap_or(false);
                if both_closed {
                    // Lock order table → stream state is respected: the
                    // state guard was dropped above before the table lock.
                    if let Ok(mut table) = table.lock() {
                        table.remove(&stream_id);
                    }
                }
            }
            Frame::Window { stream_id, credit } => {
                let Some(entry) = lookup(&shared, &table, stream_id) else {
                    return;
                };
                // A Window crossing our Close (or the peer's) in flight is
                // legal — ordering on one leg does not synchronize the
                // other — so the grant is accepted; credit on a closed leg
                // simply goes unused.
                let waker = match entry.state.lock() {
                    Ok(mut state) => match state.peer_credit.checked_add(credit) {
                        Some(total) => {
                            state.peer_credit = total;
                            state.write_waker.take()
                        }
                        None => {
                            fail_connection(
                                &shared,
                                &table,
                                format!("window credit overflow on stream {stream_id}"),
                            );
                            return;
                        }
                    },
                    Err(_) => {
                        fail_connection(&shared, &table, "mux stream state lock poisoned");
                        return;
                    }
                };
                if let Some(waker) = waker {
                    waker.wake();
                }
            }
            // Relay control frames (register / unregister / heartbeat) belong
            // on the node-to-relay link, which the daemon reads directly —
            // never on a node-to-node mux connection.
            Frame::Register { .. } | Frame::Unregister | Frame::Heartbeat => {
                fail_connection(
                    &shared,
                    &table,
                    "relay control frame on a node-to-node mux connection",
                );
                return;
            }
        }
    }
}

async fn writer_loop(
    mut writer: tokio::io::WriteHalf<impl AsyncWrite + Unpin>,
    shared: Arc<ConnectionShared>,
    table: Arc<Mutex<StreamTable>>,
    mut outbound: mpsc::Receiver<Frame>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            frame = outbound.recv() => {
                let Some(frame) = frame else {
                    return; // every handle is gone: local teardown
                };
                if let Err(error) = write_frame(&mut writer, &frame).await {
                    fail_connection(&shared, &table, error.to_string());
                    return;
                }
                shared.wake_outbound();
            }
        }
    }
}

/// Looks a stream up by id, failing the connection when the id is unknown
/// (data, close, or window for a never-opened or fully closed stream is a
/// protocol violation and must not be silently ignored).
fn lookup(
    shared: &ConnectionShared,
    table: &Mutex<StreamTable>,
    stream_id: u32,
) -> Option<StreamEntry> {
    let entry = match lock_table(table) {
        Ok(table) => table.get(&stream_id).cloned(),
        Err(_) => None,
    };
    if entry.is_none() {
        fail_connection(
            shared,
            table,
            MuxError::UnknownStream(stream_id).to_string(),
        );
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::relay_mux::frame::write_frame;
    use crate::infra::relay_mux::DEFAULT_STREAM_WINDOW;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    const NO_DEADLOCK: Duration = Duration::from_secs(10);

    fn pair(buffer: usize) -> (MuxConnection, MuxConnection) {
        let (a, b) = tokio::io::duplex(buffer);
        (
            MuxConnection::spawn(a, MuxRole::Client),
            MuxConnection::spawn(b, MuxRole::Server),
        )
    }

    #[tokio::test]
    async fn open_accept_and_echo_both_directions() {
        let (client, mut server) = pair(4096);
        let mut client_stream = client.open_stream().await.expect("open");
        let mut server_stream = timeout(NO_DEADLOCK, server.accept())
            .await
            .expect("accept should not deadlock")
            .expect("server should accept");
        assert_eq!(server_stream.id(), client_stream.id());

        client_stream
            .write_all(b"ping")
            .await
            .expect("client write");
        let mut buf = [0u8; 4];
        timeout(NO_DEADLOCK, server_stream.read_exact(&mut buf))
            .await
            .expect("read should not deadlock")
            .expect("server read");
        assert_eq!(&buf, b"ping");

        server_stream
            .write_all(b"pong")
            .await
            .expect("server write");
        timeout(NO_DEADLOCK, client_stream.read_exact(&mut buf))
            .await
            .expect("read should not deadlock")
            .expect("client read");
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn stream_ids_follow_role_parity() {
        let (mut client, mut server) = pair(4096);
        let first = client.open_stream().await.expect("open first");
        let second = client.open_stream().await.expect("open second");
        assert_eq!(first.id(), 1, "client allocates odd ids");
        assert_eq!(second.id(), 3, "client ids step by 2");
        let accepted_first = server.accept().await.expect("accept first");
        let accepted_second = server.accept().await.expect("accept second");
        assert_eq!(accepted_first.id(), 1);
        assert_eq!(accepted_second.id(), 3);

        let server_stream = server.open_stream().await.expect("server open");
        assert_eq!(server_stream.id(), 2, "server allocates even ids");
        let accepted = client.accept().await.expect("client accept");
        assert_eq!(accepted.id(), 2);
    }

    #[tokio::test]
    async fn half_close_drains_buffered_data_then_eof_and_reverse_leg_stays_open() {
        let (client, mut server) = pair(4096);
        let mut client_stream = client.open_stream().await.expect("open");
        let mut server_stream = server.accept().await.expect("accept");

        client_stream
            .write_all(b"before-close")
            .await
            .expect("client write");
        client_stream.shutdown().await.expect("client half-close");

        let mut received = Vec::new();
        timeout(NO_DEADLOCK, server_stream.read_to_end(&mut received))
            .await
            .expect("read should not deadlock")
            .expect("server read to eof");
        assert_eq!(received, b"before-close");

        // The peer's write leg is still open after our half-close.
        server_stream
            .write_all(b"after-peer-close")
            .await
            .expect("server can still write");
        let mut buf = [0u8; 16];
        client_stream
            .read_exact(&mut buf)
            .await
            .expect("client read on open leg");
        assert_eq!(&buf, b"after-peer-close");
    }

    #[tokio::test]
    async fn write_after_shutdown_fails() {
        let (client, mut server) = pair(4096);
        let mut client_stream = client.open_stream().await.expect("open");
        let _server_stream = server.accept().await.expect("accept");
        client_stream.shutdown().await.expect("half-close");
        let error = client_stream
            .write_all(b"nope")
            .await
            .expect_err("write after half-close must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn window_backpressure_parks_sender_until_peer_reads() {
        let (client, mut server) = pair(64 * 1024);
        let client_stream = client.open_stream().await.expect("open");
        let mut server_stream = server.accept().await.expect("accept");

        let total = (DEFAULT_STREAM_WINDOW + 64 * 1024) as usize;
        let payload = vec![0xabu8; total];
        let mut writer = client_stream;
        let writer = tokio::spawn(async move {
            writer
                .write_all(&payload)
                .await
                .expect("write_all should finish once credit flows");
        });
        let mut writer = writer;

        // The sender exhausts the initial window and parks: without a reader
        // it must not complete on its own.
        let premature = timeout(Duration::from_millis(300), &mut writer).await;
        assert!(
            premature.is_err(),
            "sender must park once the initial window is exhausted"
        );

        // Once the peer reads, grants flow and the parked writer completes
        // with every byte delivered in order.
        let mut received = vec![0u8; total];
        timeout(NO_DEADLOCK, server_stream.read_exact(&mut received))
            .await
            .expect("read should not deadlock")
            .expect("server read all");
        assert!(received.iter().all(|byte| *byte == 0xab));

        timeout(NO_DEADLOCK, writer)
            .await
            .expect("writer should complete after grants")
            .expect("writer task");
    }

    #[tokio::test]
    async fn concurrent_streams_keep_per_stream_order() {
        let (client, mut server) = pair(64 * 1024);
        const STREAMS: usize = 8;
        const CHUNKS: usize = 4;
        const CHUNK: usize = 16 * 1024;

        let mut client_streams = Vec::with_capacity(STREAMS);
        for _ in 0..STREAMS {
            client_streams.push(client.open_stream().await.expect("open"));
        }

        // Interleave writes across streams; each stream's payload is a
        // single repeated tag byte so any cross-stream corruption shows up
        // as a wrong tag.
        let mut writers = Vec::with_capacity(STREAMS);
        for (index, mut stream) in client_streams.into_iter().enumerate() {
            writers.push(tokio::spawn(async move {
                let tag = index as u8;
                for _ in 0..CHUNKS {
                    stream
                        .write_all(&vec![tag; CHUNK])
                        .await
                        .expect("interleaved write");
                }
            }));
        }

        let mut accepted = Vec::with_capacity(STREAMS);
        for expected_id in [1u32, 3, 5, 7, 9, 11, 13, 15] {
            let stream = server.accept().await.expect("accept");
            assert_eq!(stream.id(), expected_id);
            accepted.push(stream);
        }

        let mut readers = Vec::with_capacity(STREAMS);
        for (index, mut stream) in accepted.into_iter().enumerate() {
            readers.push(tokio::spawn(async move {
                let mut payload = vec![0u8; CHUNKS * CHUNK];
                stream
                    .read_exact(&mut payload)
                    .await
                    .expect("read full stream");
                let tag = index as u8;
                assert!(
                    payload.iter().all(|byte| *byte == tag),
                    "stream {index} must carry only its tag byte"
                );
            }));
        }

        for handle in writers {
            timeout(NO_DEADLOCK, handle)
                .await
                .expect("writer should finish")
                .expect("writer task");
        }
        for handle in readers {
            timeout(NO_DEADLOCK, handle)
                .await
                .expect("reader should finish")
                .expect("reader task");
        }
    }

    #[tokio::test]
    async fn data_for_unknown_stream_resets_connection() {
        // The raw end acts as the server peer (even stream ids).
        let (raw, mux_io) = tokio::io::duplex(4096);
        let mut mux = MuxConnection::spawn(mux_io, MuxRole::Client);
        let mut raw = raw;

        write_frame(&mut raw, &Frame::Open { stream_id: 2 })
            .await
            .expect("raw open");
        let mut accepted = mux.accept().await.expect("accept raw-opened stream");
        write_frame(
            &mut raw,
            &Frame::Data {
                stream_id: 3,
                payload: b"stray".to_vec(),
            },
        )
        .await
        .expect("raw data for unknown stream");

        let mut buf = [0u8; 1];
        let error = timeout(NO_DEADLOCK, accepted.read(&mut buf))
            .await
            .expect("read should fail, not hang")
            .expect_err("live stream must observe the reset");
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        assert!(mux.is_closed());
        assert!(
            mux.open_stream().await.is_err(),
            "opens must fail after the reset"
        );
    }

    #[tokio::test]
    async fn open_with_local_parity_resets_connection() {
        // The raw end sends an odd id (client parity) to a client-role
        // connection: only the peer's parity is legal for peer opens.
        let (raw, mux_io) = tokio::io::duplex(4096);
        let mut mux = MuxConnection::spawn(mux_io, MuxRole::Client);
        let mut raw = raw;

        write_frame(&mut raw, &Frame::Open { stream_id: 3 })
            .await
            .expect("raw open with client parity");

        let accepted = timeout(NO_DEADLOCK, mux.accept())
            .await
            .expect("accept should not hang");
        assert!(
            accepted.is_none(),
            "a violating open must fail the connection"
        );
        assert!(mux.is_closed());
    }

    #[tokio::test]
    async fn dropping_connection_unblocks_peer_streams() {
        let (client, mut server) = pair(4096);
        let mut client_stream = client.open_stream().await.expect("open");
        client_stream
            .write_all(b"payload")
            .await
            .expect("client write");
        let mut server_stream = server.accept().await.expect("accept");

        drop(client);
        drop(client_stream);

        // The peer must observe the connection end within the deadline —
        // either a clean EOF after draining or a reset — and never hang.
        let mut received = Vec::new();
        let outcome = timeout(NO_DEADLOCK, server_stream.read_to_end(&mut received)).await;
        let result = outcome.expect("peer read must not hang");
        if result.is_err() {
            assert_eq!(
                result.expect_err("reset path").kind(),
                std::io::ErrorKind::ConnectionReset
            );
        }
    }
}
