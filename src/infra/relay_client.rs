//! The node-side relay client: one persistent outbound mTLS link to the
//! pinned relay (docs/relay-design.md 协议分层). After `relay join` writes
//! `relay.toml`, the node runtime keeps an always-on connection registered
//! with the relay: dial → TLS (client auth + fingerprint pin) → `Register` →
//! heartbeat loop, with backoff-reconnect forever — the relay is
//! infrastructure, so a dropped link is retried until it comes back.
//!
//! The registered link is a relay-mode [`MuxConnection`]: the node drives
//! `Register`/`Heartbeat` through [`MuxConnection::send_control`], opens
//! node-to-node streams with [`RelayClientHandle::open_stream`], and consumes
//! relay-routed inbound streams with [`RelayClientHandle::accept_inbound`].
//! Stream frames (`OpenStream`/`Data`/`Window`/`Close`/`CloseStream`) are
//! dispatched by the mux; connection-level `Error` frames (stream id 0) reach
//! the supervisor through the mux control channel and are fatal for the link
//! (the retry loop re-registers), matching the server-side link loop.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio_rustls::TlsConnector;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::peer_connection::{dial_tcp_peer_connection, PeerConnection};
use crate::infra::relay_mux::connection::{MuxConnection, MuxOpener};
use crate::infra::relay_mux::frame::Frame;
use crate::infra::relay_mux::stream::MuxStream;
use crate::infra::relay_mux::{MuxError, ACCEPT_QUEUE};
use crate::infra::relay_server::DEFAULT_RELAY_LISTEN_PORT;
use crate::infra::relay_toml_store::RelayTomlConfig;

/// Default cadence of `Frame::Heartbeat` on an established link: the relay
/// evicts after 30s of silence (three missed 10s beats). Single source of
/// truth is the seconds constant
/// [`DEFAULT_RELAY_HEARTBEAT_INTERVAL_SECS`](crate::infra::relay_toml_store::DEFAULT_RELAY_HEARTBEAT_INTERVAL_SECS)
/// in the relay.toml store.
pub const DEFAULT_RELAY_HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(crate::infra::relay_toml_store::DEFAULT_RELAY_HEARTBEAT_INTERVAL_SECS);

/// Upper bound on the relay TLS handshake, matching the server's own
/// `HANDSHAKE_TIMEOUT`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the control channel the mux forwards connection-level relay
/// `Error` frames into.
const CONTROL_QUEUE: usize = 16;

/// The client's link to the relay.
type ClientLink = tokio_rustls::client::TlsStream<Box<dyn PeerConnection>>;

/// Errors of the stream-opening handle API.
#[derive(Debug, Error)]
pub enum RelayClientError {
    /// No relay link is currently established (still connecting, between
    /// retries, or after a connection-level refusal).
    #[allow(dead_code)]
    // Constructed by `open_stream`/`watch`, consumed by the relay
    // stream/presence integration tests; the runtime caller lands with the
    // relay session-sync wiring.
    #[error("no relay link is currently established")]
    NotConnected,
    /// The underlying mux rejected the operation.
    #[error("mux error: {0}")]
    Mux(#[from] MuxError),
}

/// Configuration for [`RelayClient::spawn`].
#[derive(Debug, Clone)]
pub struct RelayClientConfig {
    /// The pinned relay: address and expected TLS certificate fingerprint.
    pub relay: RelayTomlConfig,
    /// This node's mTLS identity (self-signed certificate and key).
    pub credentials: NodeCredentialPaths,
    /// How often `Frame::Heartbeat` is written on an established link.
    pub heartbeat_interval: Duration,
    /// Backoff between reconnect attempts.
    pub retry: RelayRetryPolicy,
}

impl RelayClientConfig {
    /// Builds a config from a parsed `relay.toml`, defaulting the heartbeat
    /// cadence to [`DEFAULT_RELAY_HEARTBEAT_INTERVAL`] and the retry policy to
    /// [`RelayRetryPolicy::default`].
    /// Builds a config from a parsed `relay.toml`, applying the file's
    /// `heartbeat_interval_secs` when present and
    /// [`DEFAULT_RELAY_HEARTBEAT_INTERVAL`] when absent.
    pub fn from_relay_toml(relay: RelayTomlConfig, credentials: NodeCredentialPaths) -> Self {
        let heartbeat_interval = relay
            .heartbeat_interval_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_RELAY_HEARTBEAT_INTERVAL);
        Self {
            relay,
            credentials,
            heartbeat_interval,
            retry: RelayRetryPolicy::default(),
        }
    }
}

/// Exponential backoff between reconnect attempts: `initial_delay`, doubling
/// per attempt up to `max_delay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayRetryPolicy {
    /// Delay before the first retry after a dropped link.
    pub initial_delay: Duration,
    /// Upper bound on the retry delay.
    pub max_delay: Duration,
}

impl Default for RelayRetryPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(5),
        }
    }
}

/// Lifecycle notifications of the persistent relay link, consumed by the node
/// runtime (forwarded into `StateEvent` for the state loop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayClientEvent {
    /// A connection attempt started (also emitted before each retry).
    Connecting {
        /// The relay address being dialed.
        relay_address: String,
    },
    /// The link is established: TLS authenticated, `Register` handed to the
    /// link (the relay answers refusal only by closing the link or an
    /// `Error` frame, both of which surface as `Disconnected`).
    Connected {
        /// The relay address the link is registered with.
        relay_address: String,
    },
    /// The link dropped; the client is backing off before reconnecting.
    Disconnected {
        /// The relay address that was dialed.
        relay_address: String,
        /// Why the link ended (read error, relay `Error` frame, protocol
        /// violation, dial/handshake failure).
        reason: String,
    },
    /// A watched node's presence transitioned on the relay. Emitted only for
    /// nodes registered as watch interests via [`RelayClientHandle::watch`]
    /// or an implicit `open_stream` watch.
    Presence {
        /// The watched node's certificate fingerprint (lowercase).
        node_id: String,
        /// `true` when the node came online, `false` when it went offline.
        online: bool,
    },
    /// The relay sent a connection-level `Error` frame on the link (register
    /// refusal, revocation, eviction). Emitted immediately before the
    /// `Disconnected` that tears the link down. The raw wire code is
    /// preserved — type it via
    /// [`crate::infra::relay_routing::error_code::RelayErrorCode::from_wire`].
    RelayError {
        /// The wire error code from `Frame::Error.code`.
        code: u16,
        /// The relay's human-readable explanation.
        message: String,
    },
}

/// Spawned relay client (issue #32). All connection state lives on the client
/// thread; interact with it only through [`RelayClient::spawn`] and
/// [`RelayClientHandle`].
pub struct RelayClient;

impl RelayClient {
    /// Starts the persistent relay link on a dedicated thread with its own
    /// multi-thread tokio runtime, reporting lifecycle through `event_tx`.
    /// The link runs until [`RelayClientHandle::cancel`] (or Drop) signals the
    /// stop watch; reconnects retry forever.
    pub fn spawn(
        config: RelayClientConfig,
        event_tx: mpsc::Sender<RelayClientEvent>,
    ) -> RelayClientHandle {
        let (stop_tx, stop_rx) = watch::channel(false);
        let (inbound_tx, inbound_rx) = mpsc::channel::<MuxStream>(ACCEPT_QUEUE);
        let link_state = Arc::new(ClientLinkState {
            opener_slot: Mutex::new(None),
            inbound_tx,
            watch_interests: Mutex::new(HashSet::new()),
        });
        let (runtime_tx, runtime_rx) = std::sync::mpsc::channel::<tokio::runtime::Handle>();
        let worker_state = link_state.clone();
        let worker = thread::Builder::new()
            .name("relay-client".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        ERROR_LOG.log_error(format!(
                            "[relay-client] failed to build tokio runtime: {error}"
                        ));
                        return;
                    }
                };
                let _ = runtime_tx.send(runtime.handle().clone());
                runtime.block_on(run_client(config, event_tx, stop_rx, worker_state));
            })
            .map_err(|error| {
                ERROR_LOG.log_error(format!("[relay-client] failed to spawn thread: {error}"));
            })
            .ok();
        // The thread sends its runtime handle before running; a spawn/build
        // failure leaves None and the stream APIs report NotConnected.
        let runtime = runtime_rx.recv().ok();
        RelayClientHandle {
            stop_tx,
            worker,
            link_state,
            inbound: Mutex::new(inbound_rx),
            runtime,
        }
    }
}

/// State shared between the client thread (which installs a fresh opener
/// after every successful register and clears it before reconnecting) and the
/// handle (which reads it from arbitrary runtime threads).
struct ClientLinkState {
    opener_slot: Mutex<Option<MuxOpener>>,
    /// Client-wide inbound stream queue; survives reconnects so a consumer
    /// never has to re-arm its accept loop.
    inbound_tx: mpsc::Sender<MuxStream>,
    /// Watch interests (lowercase node fingerprints). The relay forgets
    /// watches per connection, so `serve_registered` re-declares every
    /// interest after each `Register`.
    watch_interests: Mutex<HashSet<String>>,
}

/// Handle to a running [`RelayClient`] thread. Dropping it (or calling
/// [`RelayClientHandle::cancel`]) signals the stop watch and joins the thread
/// best-effort; every blocking phase of the run loop observes the watch, so
/// the join is prompt.
pub struct RelayClientHandle {
    stop_tx: watch::Sender<bool>,
    worker: Option<JoinHandle<()>>,
    // Consumed by `open_stream`/`accept_inbound`/`watch`: the relay
    // stream/presence integration tests exercise them end-to-end; the
    // runtime consumer lands with the relay session-sync wiring.
    #[allow(dead_code)]
    link_state: Arc<ClientLinkState>,
    #[allow(dead_code)]
    inbound: Mutex<mpsc::Receiver<MuxStream>>,
    #[allow(dead_code)]
    runtime: Option<tokio::runtime::Handle>,
}

impl RelayClientHandle {
    /// Opens a node-to-node stream toward `target_node_id` through the relay.
    ///
    /// Returns immediately with the stream handle; the relay routes the
    /// open asynchronously, so a refusal (unknown target, target offline)
    /// surfaces later as an error on stream use, while the link itself stays
    /// up.
    ///
    /// Opening a stream also establishes a presence watch for the target, so
    /// later presence transitions of the target are delivered as
    /// [`RelayClientEvent::Presence`]; the watch persists across reconnects.
    ///
    /// Returns [`RelayClientError::NotConnected`] when no link is established.
    /// Blocks the calling thread until the open is queued; must not be called
    /// from an asynchronous execution context.
    #[allow(dead_code)]
    // Consumed by the relay stream/presence integration tests; the runtime
    // consumer lands with the console wiring of issue #32.
    pub fn open_stream(
        &self,
        target_node_id: &str,
    ) -> Result<Box<dyn PeerConnection>, RelayClientError> {
        let target = target_node_id.to_lowercase();
        self.record_watch_interest(&target);
        self.declare_watch_now(&target);
        let opener = {
            let slot = self
                .link_state
                .opener_slot
                .lock()
                .map_err(|_| RelayClientError::NotConnected)?;
            slot.clone().ok_or(RelayClientError::NotConnected)?
        };
        let runtime = self
            .runtime
            .as_ref()
            .ok_or(RelayClientError::NotConnected)?;
        let stream = runtime.block_on(opener.open_stream_to(&target))?;
        Ok(Box::new(stream))
    }

    /// Watches a node's presence on the relay: records the interest and, when
    /// a link is current, declares the `Watch` on it. The interest persists
    /// across reconnects (re-declared after every `Register`), so calling
    /// `watch` while offline is fine. Repeated watches of the same target
    /// are no-ops. `send_control` is synchronous — no runtime is needed.
    ///
    /// Must not be called from an asynchronous execution context.
    #[allow(dead_code)]
    // Consumed by the relay presence integration tests; the runtime
    // caller lands with the relay session-sync wiring.
    pub fn watch(&self, target_node_id: &str) -> Result<(), RelayClientError> {
        let target = target_node_id.to_lowercase();
        let is_new = {
            let mut interests = self
                .link_state
                .watch_interests
                .lock()
                .map_err(|_| RelayClientError::NotConnected)?;
            interests.insert(target.clone())
        };
        if is_new {
            self.declare_watch_now(&target);
        }
        Ok(())
    }

    /// Records a watch interest (lowercase target) for re-declaration on
    /// reconnect. Lock is held only for the set insert.
    fn record_watch_interest(&self, target: &str) {
        if let Ok(mut interests) = self.link_state.watch_interests.lock() {
            interests.insert(target.to_string());
        }
    }

    /// Best-effort `Watch` on the current link; absence of a link is not an
    /// error (the interest is declared once a link registers).
    fn declare_watch_now(&self, target: &str) {
        let Ok(slot) = self.link_state.opener_slot.lock() else {
            return;
        };
        if let Some(opener) = slot.as_ref() {
            if let Err(error) = opener.send_control(Frame::Watch {
                node_id: target.to_string(),
            }) {
                ERROR_LOG.log_debug(format!(
                    "[relay-client] watch declaration for {target} not sent ({error})"
                ));
            }
        }
    }

    /// Blocks until the next relay-routed inbound stream arrives, returning it
    /// as a [`PeerConnection`]. Returns `None` once the client has stopped.
    ///
    /// The queue is client-wide and survives reconnects. Must not be called
    /// from an asynchronous execution context.
    #[allow(dead_code)]
    // Consumed by the relay stream/presence integration tests; the
    // runtime consumer lands with the relay session-sync wiring.
    pub fn accept_inbound(&self) -> Option<Box<dyn PeerConnection>> {
        let mut inbound = self.inbound.lock().ok()?;
        inbound
            .blocking_recv()
            .map(|stream| Box::new(stream) as Box<dyn PeerConnection>)
    }

    /// Stops the persistent link and waits for the client thread to exit.
    pub fn cancel(self) {
        // Drop signals the stop watch and joins the worker.
    }
}

impl Drop for RelayClientHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Internal result of one connection attempt.
enum RunOutcome {
    /// The stop watch fired; the run loop must exit without emitting
    /// `Disconnected`.
    Cancelled,
    /// The link ended (or never came up); carries whether registration
    /// succeeded before the drop and the human-readable reason.
    Disconnected { registered: bool, reason: String },
}

async fn run_client(
    config: RelayClientConfig,
    event_tx: mpsc::Sender<RelayClientEvent>,
    mut stop_rx: watch::Receiver<bool>,
    link_state: Arc<ClientLinkState>,
) {
    let relay_address = config.relay.address.clone();
    let mut delay = config.retry.initial_delay;
    loop {
        if *stop_rx.borrow_and_update() {
            return;
        }
        emit(
            &event_tx,
            RelayClientEvent::Connecting {
                relay_address: relay_address.clone(),
            },
        );
        match run_once(&config, &event_tx, &mut stop_rx, &link_state).await {
            RunOutcome::Cancelled => return,
            RunOutcome::Disconnected { registered, reason } => {
                if registered {
                    // A long-lived link dropping should restart at the
                    // initial delay, not at the capped backoff.
                    delay = config.retry.initial_delay;
                }
                emit(
                    &event_tx,
                    RelayClientEvent::Disconnected {
                        relay_address: relay_address.clone(),
                        reason,
                    },
                );
            }
        }
        if *stop_rx.borrow() {
            return;
        }
        tokio::select! {
            _ = stop_rx.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(config.retry.max_delay);
    }
}

/// Runs one dial → TLS → register → serve cycle. Cancellation wins over every
/// phase; every failure is converted into a disconnect reason so the caller
/// can report and retry.
async fn run_once(
    config: &RelayClientConfig,
    event_tx: &mpsc::Sender<RelayClientEvent>,
    stop_rx: &mut watch::Receiver<bool>,
    link_state: &Arc<ClientLinkState>,
) -> RunOutcome {
    if *stop_rx.borrow() {
        return RunOutcome::Cancelled;
    }
    let (host, port) = match parse_relay_address(&config.relay.address) {
        Ok(parsed) => parsed,
        Err(error) => {
            return RunOutcome::Disconnected {
                registered: false,
                reason: error.to_string(),
            };
        }
    };
    let identity = match prepare_identity(config) {
        Ok(identity) => identity,
        Err(error) => {
            return RunOutcome::Disconnected {
                registered: false,
                reason: format!("identity load failed: {error}"),
            };
        }
    };
    let ClientIdentity {
        own_fingerprint,
        certs,
        key,
    } = identity;
    let tls_config = match build_client_config(config, certs, key) {
        Ok(tls_config) => tls_config,
        Err(error) => {
            return RunOutcome::Disconnected {
                registered: false,
                reason: error.to_string(),
            };
        }
    };
    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = match rustls::pki_types::ServerName::try_from("waitagent") {
        Ok(server_name) => server_name,
        Err(error) => {
            return RunOutcome::Disconnected {
                registered: false,
                reason: format!("invalid relay server name: {error}"),
            };
        }
    };
    let tcp = tokio::select! {
        _ = stop_rx.changed() => return RunOutcome::Cancelled,
        dialed = dial_tcp_peer_connection(&host, port) => match dialed {
            Ok(tcp) => tcp,
            Err(error) => {
                return RunOutcome::Disconnected {
                    registered: false,
                    reason: format!("tcp dial to {host}:{port} failed: {error}"),
                };
            }
        },
    };
    let tls = tokio::select! {
        _ = stop_rx.changed() => return RunOutcome::Cancelled,
        handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp)) => {
            match handshake {
                Err(_) => {
                    return RunOutcome::Disconnected {
                        registered: false,
                        reason: "relay TLS handshake timed out".to_string(),
                    };
                }
                Ok(Err(error)) => {
                    return RunOutcome::Disconnected {
                        registered: false,
                        reason: format!("relay TLS handshake failed: {error}"),
                    };
                }
                Ok(Ok(tls)) => tls,
            }
        }
    };
    serve_registered(tls, config, event_tx, stop_rx, link_state, own_fingerprint).await
}

/// What the supervisor does with one mux control frame.
#[derive(Debug)]
enum ControlOutcome {
    /// Frame handled; keep supervising the link.
    Continue,
    /// The frame tore the link down; the string is the `Disconnected` reason.
    Break(String),
}

/// Handles one connection-level mux control frame, shared by the live select
/// arm and the connection-death drain: a connection-level `Error` emits
/// `RelayError` first and folds the reason; `Presence` is forwarded; anything
/// else is logged and ignored.
fn handle_control_frame(event_tx: &mpsc::Sender<RelayClientEvent>, frame: Frame) -> ControlOutcome {
    match frame {
        Frame::Error { code, message, .. } => {
            ERROR_LOG.log_error(format!(
                "[relay-client] relay error: code 0x{code:04x}: {message}"
            ));
            // Structured first: consumers see the code+message before
            // the disconnect reason folds them into a string.
            emit(
                event_tx,
                RelayClientEvent::RelayError {
                    code,
                    message: message.clone(),
                },
            );
            ControlOutcome::Break(format!("relay error 0x{code:04x}: {message}"))
        }
        Frame::Presence { node_id, online } => {
            // A watched node's presence transitioned; forward to the
            // consumer (the link itself stays up).
            emit(event_tx, RelayClientEvent::Presence { node_id, online });
            ControlOutcome::Continue
        }
        other => {
            ERROR_LOG.log_debug(format!(
                "[relay-client] ignoring unexpected control frame: {other:?}"
            ));
            ControlOutcome::Continue
        }
    }
}

/// Death path of the registered loop: surface the control frames the mux
/// queued before it died, then describe the death. An evicting relay sends
/// `Error` and closes in the same breath, so the mux queues the frame and
/// dies together; tearing down on connection death alone would drop the
/// frame and skip the `RelayError` event (issue #114). Pending control
/// frames must not be lost to that race.
fn pending_control_or_dead_reason(
    control_rx: &mut mpsc::Receiver<Frame>,
    conn: &MuxConnection,
    event_tx: &mpsc::Sender<RelayClientEvent>,
) -> String {
    while let Ok(frame) = control_rx.try_recv() {
        if let ControlOutcome::Break(reason) = handle_control_frame(event_tx, frame) {
            return reason;
        }
    }
    format!("relay link closed: {}", conn.dead_reason())
}

/// The registered phase: the TLS link runs as a relay-mode mux connection.
/// This task supervises: connection death (→ reconnect), connection-level
/// relay `Error` frames forwarded by the mux (→ log + reconnect), inbound
/// stream forwarding into the client-wide accept queue, and the stop watch.
/// The opener is installed in the handle's slot for the duration and cleared
/// before returning.
async fn serve_registered(
    tls: ClientLink,
    config: &RelayClientConfig,
    event_tx: &mpsc::Sender<RelayClientEvent>,
    stop_rx: &mut watch::Receiver<bool>,
    link_state: &Arc<ClientLinkState>,
    own_fingerprint: String,
) -> RunOutcome {
    let (control_tx, mut control_rx) = mpsc::channel::<Frame>(CONTROL_QUEUE);
    let mut conn = MuxConnection::spawn_relay_link(tls, control_tx);
    let opener = conn.opener();
    if let Err(error) = opener.send_control(Frame::Register {
        node_id: own_fingerprint,
    }) {
        return RunOutcome::Disconnected {
            registered: false,
            reason: format!("register send failed: {error}"),
        };
    }
    if let Ok(mut slot) = link_state.opener_slot.lock() {
        *slot = Some(opener.clone());
    }
    // The relay forgets watches per connection: re-declare every persisted
    // interest now that Register is queued (the queue is FIFO, so the
    // watches follow the register on the wire).
    let interests: Vec<String> = link_state
        .watch_interests
        .lock()
        .map(|interests| interests.iter().cloned().collect())
        .unwrap_or_default();
    for target in interests {
        if let Err(error) = opener.send_control(Frame::Watch { node_id: target }) {
            ERROR_LOG.log_debug(format!("[relay-client] watch re-declare failed: {error}"));
        }
    }
    emit(
        event_tx,
        RelayClientEvent::Connected {
            relay_address: config.relay.address.clone(),
        },
    );
    let heartbeat_task = tokio::spawn(heartbeat_loop(
        config.heartbeat_interval,
        opener,
        stop_rx.clone(),
    ));

    let mut forward_inbound = true;
    let outcome = loop {
        if *stop_rx.borrow() {
            break None;
        }
        if conn.is_closed() {
            break Some(pending_control_or_dead_reason(
                &mut control_rx,
                &conn,
                event_tx,
            ));
        }
        tokio::select! {
            _ = stop_rx.changed() => break None,
            frame = control_rx.recv() => match frame {
                Some(frame) => match handle_control_frame(event_tx, frame) {
                    ControlOutcome::Continue => {}
                    ControlOutcome::Break(reason) => break Some(reason),
                },
                None => break Some(pending_control_or_dead_reason(
                    &mut control_rx,
                    &conn,
                    event_tx,
                )),
            },
            accepted = conn.accept() => match accepted {
                Some(stream) => {
                    if !forward_inbound {
                        // The accept receiver is gone; drop further inbound
                        // streams (their connections are unusable anyway).
                        continue;
                    }
                    tokio::select! {
                        _ = stop_rx.changed() => break None,
                        sent = link_state.inbound_tx.send(stream) => {
                            if sent.is_err() {
                                forward_inbound = false;
                            }
                        }
                    }
                }
                None => break Some(pending_control_or_dead_reason(
                    &mut control_rx,
                    &conn,
                    event_tx,
                )),
            },
        }
    };

    if let Ok(mut slot) = link_state.opener_slot.lock() {
        *slot = None;
    }
    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    drop(conn);
    match outcome {
        Some(reason) => RunOutcome::Disconnected {
            registered: true,
            reason,
        },
        None => RunOutcome::Cancelled,
    }
}

/// Writes one `Frame::Heartbeat` per interval through the mux opener. Exits
/// on the stop watch or when the link dies (the next send fails).
async fn heartbeat_loop(interval: Duration, opener: MuxOpener, mut stop_rx: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(interval);
    loop {
        tokio::select! {
            _ = stop_rx.changed() => return,
            _ = tick.tick() => {
                if let Err(error) = opener.send_control(Frame::Heartbeat) {
                    ERROR_LOG.log_debug(format!(
                        "[relay-client] heartbeat not sent ({error}); link is going down"
                    ));
                    return;
                }
            }
        }
    }
}

fn emit(event_tx: &mpsc::Sender<RelayClientEvent>, event: RelayClientEvent) {
    if let Err(error) = event_tx.try_send(event) {
        ERROR_LOG.log_debug(format!(
            "[relay-client] event not delivered ({error}); link continues"
        ));
    }
}

/// Loads (generating when missing) this node's identity and its PEM key
/// material. Small local files, read once per connection attempt — the same
/// sync-read idiom as `relay_join` and the relay server startup.
fn prepare_identity(config: &RelayClientConfig) -> Result<ClientIdentity, RelayClientConnectError> {
    let own_fingerprint = node_credentials::ensure_credentials(&config.credentials)?;
    let cert_pem = fs::read_to_string(&config.credentials.cert_path)?;
    let key_pem = fs::read_to_string(&config.credentials.key_path)?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(|error| RelayClientConnectError::Tls(error.to_string()))?;
    if certs.is_empty() {
        return Err(RelayClientConnectError::Credentials(
            node_credentials::NodeCredentialsError::MissingEndEntityCertificate,
        ));
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|error| RelayClientConnectError::Tls(error.to_string()))?
        .ok_or_else(|| {
            RelayClientConnectError::Tls(format!(
                "no private key in {:?}",
                config.credentials.key_path
            ))
        })?;
    Ok(ClientIdentity {
        own_fingerprint,
        certs,
        key,
    })
}

struct ClientIdentity {
    own_fingerprint: String,
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
}

fn build_client_config(
    config: &RelayClientConfig,
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
) -> Result<rustls::ClientConfig, RelayClientConnectError> {
    let verifier = Arc::new(PinnedServerCertVerifier {
        expected_fingerprint: config.relay.relay_fingerprint.clone(),
    });
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
        .map_err(|error| RelayClientConnectError::Tls(error.to_string()))
}

/// Connect-phase failures, converted to disconnect reasons at the run loop.
#[derive(Debug, Error)]
enum RelayClientConnectError {
    #[error("invalid relay address {0:?}: {1}")]
    Address(String, String),
    #[error(transparent)]
    Credentials(#[from] node_credentials::NodeCredentialsError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("tls error: {0}")]
    Tls(String),
}

/// rustls server-cert verifier pinning the relay certificate fingerprint from
/// `relay.toml`: the end-entity SPKI SHA-256 must match the pin (learned via
/// the token-authenticated enrollment session), while TLS 1.2/1.3 signature
/// checks are delegated to ring so the relay proves possession of its private
/// key (same construction as `relay_join`'s enrollment verifier).
#[derive(Debug)]
struct PinnedServerCertVerifier {
    expected_fingerprint: String,
}

impl rustls::client::danger::ServerCertVerifier for PinnedServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fingerprint = node_credentials::cert_fingerprint_from_der(end_entity.as_ref())
            .map_err(|_| {
                rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
            })?;
        if fingerprint.eq_ignore_ascii_case(&self.expected_fingerprint) {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Splits `host[:port]`; a missing port defaults to
/// [`DEFAULT_RELAY_LISTEN_PORT`]. The data port is the base port (the
/// enrollment offset used by `relay join` does not apply here).
fn parse_relay_address(address: &str) -> Result<(String, u16), RelayClientConnectError> {
    let address = address.trim();
    if address.is_empty() {
        return Err(RelayClientConnectError::Address(
            address.to_string(),
            "empty address".to_string(),
        ));
    }
    match address.rsplit_once(':') {
        Some((host, port)) => {
            let host = host.trim();
            if host.is_empty() {
                return Err(RelayClientConnectError::Address(
                    address.to_string(),
                    "missing host".to_string(),
                ));
            }
            let port = port.trim().parse::<u16>().map_err(|_| {
                RelayClientConnectError::Address(
                    address.to_string(),
                    "port is not a number".to_string(),
                )
            })?;
            Ok((host.to_string(), port))
        }
        None => Ok((address.to_string(), DEFAULT_RELAY_LISTEN_PORT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;

    #[test]
    fn retry_policy_defaults_to_half_second_initial_and_five_second_cap() {
        let policy = RelayRetryPolicy::default();
        assert_eq!(policy.initial_delay, Duration::from_millis(500));
        assert_eq!(policy.max_delay, Duration::from_secs(5));
    }

    #[test]
    fn from_relay_toml_defaults_heartbeat_and_retry() {
        let config = RelayClientConfig::from_relay_toml(
            RelayTomlConfig {
                address: "relay.example:7475".to_string(),
                relay_fingerprint: "ab".to_string(),
                heartbeat_interval_secs: None,
            },
            NodeCredentialPaths {
                key_path: std::path::PathBuf::from("node.key"),
                cert_path: std::path::PathBuf::from("node.crt"),
            },
        );
        assert_eq!(config.heartbeat_interval, DEFAULT_RELAY_HEARTBEAT_INTERVAL);
        assert_eq!(config.retry, RelayRetryPolicy::default());
    }

    #[test]
    fn from_relay_toml_applies_the_configured_heartbeat() {
        let config = RelayClientConfig::from_relay_toml(
            RelayTomlConfig {
                address: "relay.example:7475".to_string(),
                relay_fingerprint: "ab".to_string(),
                heartbeat_interval_secs: Some(30),
            },
            NodeCredentialPaths {
                key_path: std::path::PathBuf::from("node.key"),
                cert_path: std::path::PathBuf::from("node.crt"),
            },
        );
        assert_eq!(config.heartbeat_interval, Duration::from_secs(30));
    }

    #[test]
    fn address_without_port_defaults_to_the_relay_port() {
        let (host, port) = parse_relay_address("relay.example").expect("parse");
        assert_eq!(host, "relay.example");
        assert_eq!(port, DEFAULT_RELAY_LISTEN_PORT);
    }

    #[test]
    fn address_with_explicit_port_wins() {
        let (host, port) = parse_relay_address("relay.example:9999").expect("parse");
        assert_eq!(host, "relay.example");
        assert_eq!(port, 9999);
    }

    #[test]
    fn address_rejects_garbage() {
        assert!(matches!(
            parse_relay_address(""),
            Err(RelayClientConnectError::Address(..))
        ));
        assert!(matches!(
            parse_relay_address(":9999"),
            Err(RelayClientConnectError::Address(..))
        ));
        assert!(matches!(
            parse_relay_address("host:notaport"),
            Err(RelayClientConnectError::Address(..))
        ));
    }

    #[test]
    fn control_frame_handler_folds_error_into_teardown_reason() {
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let outcome = handle_control_frame(
            &event_tx,
            Frame::Error {
                stream_id: 0,
                code: 0x000a,
                message: "relay heartbeat timed out; the link was evicted".to_string(),
            },
        );
        assert!(
            matches!(outcome, ControlOutcome::Break(ref reason) if reason == "relay error 0x000a: relay heartbeat timed out; the link was evicted"),
            "an error frame must tear the link down with the folded reason: {outcome:?}"
        );
        assert!(
            matches!(
                event_rx.try_recv(),
                Ok(RelayClientEvent::RelayError { code: 0x000a, .. })
            ),
            "the structured relay error must be emitted before the teardown"
        );
    }

    #[test]
    fn control_frame_handler_forwards_presence_and_keeps_supervising() {
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let outcome = handle_control_frame(
            &event_tx,
            Frame::Presence {
                node_id: "peer".to_string(),
                online: false,
            },
        );
        assert!(
            matches!(outcome, ControlOutcome::Continue),
            "presence keeps the link up: {outcome:?}"
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(RelayClientEvent::Presence { node_id, online: false }) if node_id == "peer"
        ));
    }

    #[test]
    fn control_frame_handler_ignores_unexpected_frames() {
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let outcome = handle_control_frame(&event_tx, Frame::Heartbeat);
        assert!(matches!(outcome, ControlOutcome::Continue));
        assert!(
            event_rx.try_recv().is_err(),
            "an unexpected control frame emits no event"
        );
    }

    #[tokio::test]
    async fn death_reason_surfaces_control_frames_queued_before_link_death() {
        // The eviction race (issue #114): the mux reader queues the relay's
        // `Error` frame and dies on the close that follows it, both before
        // the supervisor observes the death. The drain must emit the queued
        // frames and fold the reason instead of reporting a bare teardown.
        let (client_io, server_io) = tokio::io::duplex(1024);
        let (control_tx, mut control_rx) = mpsc::channel(16);
        let conn = MuxConnection::spawn_relay_link(client_io, control_tx.clone());
        drop(server_io);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !conn.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection should die when the peer half closes");

        control_tx
            .try_send(Frame::Presence {
                node_id: "peer".to_string(),
                online: true,
            })
            .expect("queue presence");
        control_tx
            .try_send(Frame::Error {
                stream_id: 0,
                code: 0x000a,
                message: "evicted".to_string(),
            })
            .expect("queue eviction error");

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let reason = pending_control_or_dead_reason(&mut control_rx, &conn, &event_tx);
        assert_eq!(reason, "relay error 0x000a: evicted");
        assert!(matches!(
            event_rx.try_recv(),
            Ok(RelayClientEvent::Presence { node_id, online: true }) if node_id == "peer"
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(RelayClientEvent::RelayError { code: 0x000a, .. })
        ));
    }

    #[test]
    fn pinned_verifier_accepts_matching_fingerprint_case_insensitively() {
        let cert = rcgen::Certificate::from_params(rcgen::CertificateParams::new(vec![
            "waitagent".to_string(),
        ]))
        .expect("cert should generate");
        let der = cert.serialize_der().expect("cert should serialize");
        let fingerprint = node_credentials::cert_fingerprint_from_der(&der).expect("fingerprint");
        let uppercased = fingerprint.to_uppercase();
        let verifier = PinnedServerCertVerifier {
            expected_fingerprint: uppercased,
        };
        let now = rustls::pki_types::UnixTime::now();
        let result = verifier.verify_server_cert(
            &rustls::pki_types::CertificateDer::from(der.clone()),
            &[],
            &rustls::pki_types::ServerName::try_from("waitagent").expect("server name"),
            &[],
            now,
        );
        assert!(result.is_ok(), "pin match must verify: {result:?}");
    }

    #[test]
    fn pinned_verifier_rejects_mismatched_fingerprint() {
        let cert = rcgen::Certificate::from_params(rcgen::CertificateParams::new(vec![
            "waitagent".to_string(),
        ]))
        .expect("cert should generate");
        let der = cert.serialize_der().expect("cert should serialize");
        let verifier = PinnedServerCertVerifier {
            expected_fingerprint: "deadbeef".to_string(),
        };
        let now = rustls::pki_types::UnixTime::now();
        let result = verifier.verify_server_cert(
            &rustls::pki_types::CertificateDer::from(der),
            &[],
            &rustls::pki_types::ServerName::try_from("waitagent").expect("server name"),
            &[],
            now,
        );
        assert!(result.is_err(), "pin mismatch must fail");
    }
}
