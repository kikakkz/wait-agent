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

use crate::infra::error_log::ERROR_LOG;

use super::frame::{read_frame, write_frame, Frame, MAX_NODE_ID_LEN};
use super::stream::{MuxStream, StreamState};
use super::{first_stream_id, owns_stream_id, MuxError, ACCEPT_QUEUE, OUTBOUND_QUEUE};

/// Which side of the connection this handle is. The client allocates odd
/// stream ids, the server even ids.
#[allow(dead_code)]
// `Server` parity is exercised by the node-to-node unit tests; runtime
// node-to-node mux links land with the relay mux adoption (issue #51). The
// relay client is always `Client` parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxRole {
    Client,
    Server,
}

/// What speaks on the other end of the connection. Node-to-node links carry
/// only the four mux frames; relay links additionally carry the relay
/// control frames (`OpenStream`/`CloseStream`/`Error` routing plus the
/// register/heartbeat channel the node client drives through
/// [`MuxOpener::send_control`]).
#[allow(dead_code)]
// `NodeToNode` is selected by `spawn`, which the node-to-node unit tests
// exercise; the runtime consumer lands with issue #51.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    NodeToNode,
    RelayLink,
}

/// Per-stream state as stored in the connection's stream table. Dropping an
/// entry drops the inbound sender, which the stream's reader observes as the
/// end of the stream.
#[derive(Clone)]
pub(crate) struct StreamEntry {
    pub(crate) inbound: mpsc::Sender<Vec<u8>>,
    pub(crate) state: Arc<Mutex<StreamState>>,
    pub(crate) peer_fin: Arc<AtomicBool>,
    /// Full teardown by the relay (`CloseStream` / stream-scoped `Error`):
    /// pending and future reads/writes must fail with an error, not EOF.
    pub(crate) reset: Arc<AtomicBool>,
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
    /// What speaks on the other end; selects the reader dispatch and which
    /// outbound helpers are legal.
    mode: Mode,
    // Read by `open_stream`/`open_stream_to`; node-to-node opens are
    // test-only until issue #51, so the counter is test-read today.
    #[allow(dead_code)]
    next_local_id: AtomicU32,
}

impl ConnectionShared {
    pub(crate) fn role(&self) -> MuxRole {
        self.role
    }

    pub(crate) fn is_relay_link(&self) -> bool {
        matches!(self.mode, Mode::RelayLink)
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
    #[allow(dead_code)]
    // Node-to-node mux links are exercised by the unit tests; the runtime
    // consumer lands with the relay mux adoption (issue #51).
    pub fn spawn(io: impl AsyncRead + AsyncWrite + Send + Unpin + 'static, role: MuxRole) -> Self {
        Self::build(io, role, Mode::NodeToNode, None)
    }

    /// Spawns the loops over a node-to-relay link: `Client` parity (node
    /// streams are odd, relay-routed inbound streams even) and the relay
    /// control-frame dispatch. Connection-level `Error` frames (stream id 0)
    /// are forwarded to `control_tx`; the stream-scoped relay frames
    /// (`OpenStream` even / `CloseStream` / `Error`) are dispatched onto
    /// streams without involving the control channel.
    pub fn spawn_relay_link(
        io: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
        control_tx: mpsc::Sender<Frame>,
    ) -> Self {
        Self::build(io, MuxRole::Client, Mode::RelayLink, Some(control_tx))
    }

    fn build(
        io: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
        role: MuxRole,
        mode: Mode,
        control: Option<mpsc::Sender<Frame>>,
    ) -> Self {
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
            mode,
            next_local_id: AtomicU32::new(first_stream_id(role).get()),
        });
        let table = Arc::new(Mutex::new(HashMap::new()));
        let reader_task = tokio::spawn(reader_loop(
            reader,
            shared.clone(),
            table.clone(),
            accept_tx,
            shutdown_rx_reader,
            control,
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

    /// Returns why the connection died (the generic close message when no
    /// reason was recorded).
    pub(crate) fn dead_reason(&self) -> String {
        self.shared.dead_reason()
    }

    /// Returns a cloneable handle for opening relay-routed streams and
    /// sending relay control frames, usable while this connection is owned by
    /// a driver task (the node relay client installs it in its handle).
    pub fn opener(&self) -> MuxOpener {
        MuxOpener {
            shared: self.shared.clone(),
            table: self.table.clone(),
        }
    }

    /// Opens a new stream toward the peer.
    #[allow(dead_code)]
    // Node-to-node opens are exercised by the unit tests; the runtime
    // consumer lands with the relay mux adoption (issue #51).
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

    /// Opens a new stream toward `target_node_id` through the relay
    /// (relay-link mode only). Returns immediately; a refusal arrives
    /// asynchronously as an `Error` frame for the stream and surfaces on
    /// stream use.
    #[allow(dead_code)]
    // The relay client drives the `MuxOpener` it gets from
    // `MuxConnection::opener` while the supervisor owns the connection; this
    // wrapper exists for connection-owning callers and the tests.
    pub async fn open_stream_to(&self, target_node_id: &str) -> Result<MuxStream, MuxError> {
        self.opener().open_stream_to(target_node_id).await
    }

    /// Sends a relay control frame on a relay-link connection (relay-link
    /// mode only). Only `Register` / `Unregister` / `Heartbeat` are accepted.
    #[allow(dead_code)]
    // See `open_stream_to` above: connection-owning callers and tests use
    // this wrapper; the relay client drives its `MuxOpener`.
    pub fn send_control(&self, frame: Frame) -> Result<(), MuxError> {
        self.opener().send_control(frame)
    }

    /// Accepts the next stream opened by the peer. Returns `None` once the
    /// connection is closed and all pending streams have been delivered.
    pub async fn accept(&mut self) -> Option<MuxStream> {
        self.accept_rx.recv().await
    }
}

/// Cloneable stream/control handle of a [`MuxConnection`] (see
/// [`MuxConnection::opener`]). Holds only the connection-wide arcs, so it is
/// cheap to clone into driver tasks and client handles while the connection
/// itself stays owned by its driver.
#[derive(Clone)]
pub struct MuxOpener {
    shared: Arc<ConnectionShared>,
    // Inserted-stream table for `open_stream_to`; exercised end-to-end by
    // the relay stream integration tests.
    #[allow(dead_code)]
    table: Arc<Mutex<StreamTable>>,
}

impl MuxOpener {
    /// Opens a relay-routed stream toward `target_node_id` (relay-link mode
    /// only). The relay answers with an `OpenStream` for the peer and routes
    /// the stream's frames; an unknown target (or any other refusal) arrives
    /// asynchronously as an `Error` frame for the returned stream and
    /// surfaces as an error on stream use — the connection stays up.
    pub async fn open_stream_to(&self, target_node_id: &str) -> Result<MuxStream, MuxError> {
        if !self.shared.is_relay_link() {
            return Err(MuxError::NotRelayLink);
        }
        if target_node_id.len() as u32 > MAX_NODE_ID_LEN {
            return Err(MuxError::ProtocolViolation(format!(
                "target node id of {} bytes exceeds the {MAX_NODE_ID_LEN}-byte limit",
                target_node_id.len()
            )));
        }
        if self.shared.dead.load(Ordering::Acquire) {
            return Err(MuxError::ConnectionClosed(self.shared.dead_reason()));
        }
        // Relay-link streams are node-initiated: odd ids, stepping by 2
        // (wraps after ~2^31 streams; the occupied check surfaces a bug).
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
            .send(Frame::OpenStream {
                stream_id,
                target_node_id: target_node_id.to_string(),
            })
            .await
            .is_err()
        {
            let _ = lock_table(&self.table).map(|mut table| table.remove(&stream_id));
            return Err(MuxError::ConnectionClosed(self.shared.dead_reason()));
        }
        Ok(stream)
    }

    /// Sends one relay control frame: only `Register` / `Unregister` /
    /// `Heartbeat` are legal on a relay link, and only in relay-link mode.
    /// Uses `try_send`; a full queue maps to an error (the caller decides
    /// whether to retry — the heartbeat task skips a beat, the node client
    /// reconnects).
    pub fn send_control(&self, frame: Frame) -> Result<(), MuxError> {
        if !self.shared.is_relay_link() {
            return Err(MuxError::NotRelayLink);
        }
        match &frame {
            Frame::Register { node_id } if node_id.len() as u32 > MAX_NODE_ID_LEN => {
                return Err(MuxError::ProtocolViolation(format!(
                    "Register node id of {} bytes exceeds the {MAX_NODE_ID_LEN}-byte limit",
                    node_id.len()
                )));
            }
            Frame::Register { .. } | Frame::Unregister | Frame::Heartbeat => {}
            other => {
                return Err(MuxError::ProtocolViolation(format!(
                    "{other:?} is not a relay control frame"
                )));
            }
        }
        self.shared
            .outbound
            .try_send(frame)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    MuxError::ConnectionClosed("mux outbound queue is full".to_string())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    MuxError::ConnectionClosed(self.shared.dead_reason())
                }
            })
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
    control: Option<mpsc::Sender<Frame>>,
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
                if shared.is_relay_link() {
                    fail_connection(&shared, &table, "node-to-node Open frame on a relay link");
                    return;
                }
                if owns_stream_id(shared.role(), stream_id) {
                    fail_connection(
                        &shared,
                        &table,
                        format!("peer opened stream {stream_id} with local id parity"),
                    );
                    return;
                }
                if push_inbound_stream(&shared, &table, &accept_tx, stream_id)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Relay-routed inbound stream: the relay allocates even ids on
            // relay links. An odd id is a misroute of a stream we initiated.
            Frame::OpenStream { stream_id, .. } => {
                if !relay_link_or_fail(&shared, &table) {
                    return;
                }
                if stream_id % 2 != 0 {
                    fail_connection(
                        &shared,
                        &table,
                        format!("relay routed an odd (locally initiated) OpenStream {stream_id}"),
                    );
                    return;
                }
                if push_inbound_stream(&shared, &table, &accept_tx, stream_id)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Full teardown of one routed stream (both legs), driven by the
            // relay. Unknown ids are expected teardown races: log and ignore.
            Frame::CloseStream { stream_id } => {
                if !relay_link_or_fail(&shared, &table) {
                    return;
                }
                teardown_stream(&table, stream_id);
            }
            Frame::Error {
                stream_id,
                code,
                message,
            } => {
                if !relay_link_or_fail(&shared, &table) {
                    return;
                }
                if stream_id == 0 {
                    // Connection-level error: forward to the control channel
                    // and stay alive — the client (not the mux) decides
                    // whether the link survives.
                    let Some(control) = &control else {
                        fail_connection(
                            &shared,
                            &table,
                            "connection-level relay error without a control channel",
                        );
                        return;
                    };
                    if control
                        .send(Frame::Error {
                            stream_id,
                            code,
                            message,
                        })
                        .await
                        .is_err()
                    {
                        fail_connection(
                            &shared,
                            &table,
                            "relay control receiver dropped while forwarding a connection error",
                        );
                        return;
                    }
                } else {
                    // Stream-scoped error (OpenStream refusal, leg-closed
                    // race): reset exactly that stream; the link stays up.
                    teardown_stream(&table, stream_id);
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
            // Node-to-relay control frames never travel node-to-relay in the
            // relay→node direction: the relay never sends them. The
            // enrollment pair belongs on the enrollment listener only.
            Frame::Register { .. }
            | Frame::Unregister
            | Frame::Heartbeat
            | Frame::Enroll { .. }
            | Frame::EnrollResponse { .. } => {
                fail_connection(
                    &shared,
                    &table,
                    if shared.is_relay_link() {
                        "node-only control frame received from the relay"
                    } else {
                        "relay control frame on a node-to-node mux connection"
                    },
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

/// Fails the connection unless it is a relay link; returns whether the loop
/// arm may continue. Keeps the node-to-node dispatch byte-identical.
fn relay_link_or_fail(shared: &ConnectionShared, table: &Mutex<StreamTable>) -> bool {
    if shared.is_relay_link() {
        return true;
    }
    fail_connection(
        shared,
        table,
        "relay control frame on a node-to-node mux connection",
    );
    false
}

/// Creates a stream for a peer-initiated id (inbound `Open` on node-to-node
/// links, relay-routed `OpenStream` on relay links) and queues it for
/// `accept`. Fails the connection on id collision or a dropped accept queue.
async fn push_inbound_stream(
    shared: &Arc<ConnectionShared>,
    table: &Mutex<StreamTable>,
    accept_tx: &mpsc::Sender<MuxStream>,
    stream_id: u32,
) -> Result<(), ()> {
    let (stream, entry) = MuxStream::new(stream_id, shared.clone());
    let occupied = match lock_table(table) {
        Ok(mut table) => table.insert(stream_id, entry).is_some(),
        Err(error) => {
            fail_connection(shared, table, error.to_string());
            return Err(());
        }
    };
    if occupied {
        fail_connection(
            shared,
            table,
            format!("peer reopened live stream {stream_id}"),
        );
        return Err(());
    }
    if accept_tx.send(stream).await.is_err() {
        fail_connection(
            shared,
            table,
            "connection handle dropped while accepting a stream",
        );
        return Err(());
    }
    Ok(())
}

/// Full teardown of one stream on a relay link (`CloseStream` or a
/// stream-scoped `Error`): the reset flag is stored first so a reader woken
/// by the inbound-sender drop never observes a clean EOF, the entry is
/// removed (dropping the sender closes the reader's queue), and a parked
/// writer's waker is taken and woken only after the state lock is released.
fn teardown_stream(table: &Mutex<StreamTable>, stream_id: u32) {
    let entry = match lock_table(table) {
        Ok(mut table) => table.remove(&stream_id),
        Err(_) => None,
    };
    let Some(entry) = entry else {
        // Teardown races with in-flight frames are expected on routed
        // streams; the relay also notifies both legs independently.
        ERROR_LOG.log_debug(format!(
            "[mux] relay teardown for unknown or closed stream {stream_id}; ignoring the race"
        ));
        return;
    };
    entry.reset.store(true, Ordering::SeqCst);
    let waker = entry
        .state
        .lock()
        .ok()
        .and_then(|mut state| state.write_waker.take());
    drop(entry);
    if let Some(waker) = waker {
        waker.wake();
    }
}

#[cfg(test)]
mod node_tests;

#[cfg(test)]
mod relay_tests;
