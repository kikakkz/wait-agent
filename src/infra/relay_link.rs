//! Per-link relay protocol loop: register/heartbeat lifecycle plus stream
//! routing dispatch. The link's TLS stream is split — this task owns the
//! read half and the frame dispatch; a writer task pumps a bounded outbound
//! queue into the write half. Cross-link forwarding awaits the peer's
//! bounded queue, which couples backpressure between the two legs of a
//! routed stream exactly like TCP (a peer that stops reading eventually
//! stalls the other side's writer).
//!
//! Relay links never carry raw mux `Open` frames (streams open via the
//! `OpenStream` control frame) and never carry node-sent `Error` frames.
//! Stream frames for unknown or closed routed streams are answered with a
//! structured `Error` and the link stays up — teardown races with in-flight
//! frames are expected, and failing a whole link for a dead peer's frames
//! would be wrong.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::mpsc;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::node_credentials;
use crate::infra::relay_connection_table::{
    RelayConnectionTable, RelayLifecycleConfig, RelayLifecycleEvent,
};
use crate::infra::relay_mux::frame::{read_frame, write_frame, Frame};
use crate::infra::relay_routing::{error_code, CloseOutcome, Lookup, RouteKey, RoutingTable};

/// Bounded outbound queue per link: frames waiting for the writer task.
pub(crate) const LINK_OUTBOUND_QUEUE: usize = 256;

pub(crate) type ServerTls = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

/// Runs one authenticated link to completion: registration deadline,
/// fingerprint-checked register, then heartbeat/unregister/routing dispatch
/// until either side ends it. All routing state touching this connection is
/// torn down on exit and surviving peers receive `CloseStream`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_link(
    tls: ServerTls,
    peer_addr: SocketAddr,
    table: Arc<RelayConnectionTable>,
    routing: Arc<RoutingTable>,
    events: mpsc::Sender<RelayLifecycleEvent>,
    lifecycle: RelayLifecycleConfig,
) {
    let peer_fingerprint = match peer_fingerprint(&tls, peer_addr) {
        Some(fingerprint) => fingerprint,
        None => return,
    };

    let (mut reader, writer) = tokio::io::split(tls);
    let (outbound_tx, outbound_rx) = mpsc::channel(LINK_OUTBOUND_QUEUE);
    let writer_task = tokio::spawn(link_writer_loop(writer, outbound_rx));

    let registered = match register_link(
        &mut reader,
        peer_addr,
        &peer_fingerprint,
        &table,
        &events,
        &outbound_tx,
        &lifecycle,
    )
    .await
    {
        Some(state) => state,
        None => {
            drop(outbound_tx);
            drop(reader);
            let _ = writer_task.await;
            return;
        }
    };

    dispatch_loop(
        &mut reader,
        peer_addr,
        &table,
        &routing,
        &events,
        &registered,
        &outbound_tx,
    )
    .await;

    // Epilogue: drop this link's table entry and every route touching it;
    // surviving peers get CloseStream for their side.
    table.remove_if_current(&registered.node_id, registered.connection_id);
    for (peer_outbound, peer_stream_id) in routing.teardown_connection(registered.connection_id) {
        let _ = peer_outbound.try_send(Frame::CloseStream {
            stream_id: peer_stream_id,
        });
    }
    drop(outbound_tx);
    drop(reader);
    let _ = writer_task.await;
}

struct RegisteredLink {
    node_id: String,
    connection_id: u64,
    retire_rx: tokio::sync::watch::Receiver<bool>,
}

fn peer_fingerprint(tls: &ServerTls, peer_addr: SocketAddr) -> Option<String> {
    let (_, server_conn) = tls.get_ref();
    match server_conn
        .peer_certificates()
        .and_then(|certs| certs.first())
    {
        Some(cert) => match node_credentials::cert_fingerprint_from_der(cert.as_ref()) {
            Ok(fingerprint) => Some(fingerprint),
            Err(error) => {
                ERROR_LOG.log_error(format!(
                    "[relay] {peer_addr}: peer certificate fingerprint failed: {error}"
                ));
                None
            }
        },
        // Mandatory client auth makes this an invariant; a missing peer
        // certificate means the rustls contract broke — close the link
        // rather than continue without identity.
        None => {
            ERROR_LOG.log_error(format!(
                "[relay] {peer_addr}: no peer certificate after mandatory client auth"
            ));
            None
        }
    }
}

/// Registration phase: first frame must be `Register` within the deadline,
/// and the node id must equal the mTLS fingerprint (self-proving identity,
/// docs/relay-design.md node_id 策略).
async fn register_link(
    reader: &mut ReadHalf<ServerTls>,
    peer_addr: SocketAddr,
    peer_fingerprint: &str,
    table: &Arc<RelayConnectionTable>,
    events: &mpsc::Sender<RelayLifecycleEvent>,
    outbound_tx: &mpsc::Sender<Frame>,
    lifecycle: &RelayLifecycleConfig,
) -> Option<RegisteredLink> {
    let first = tokio::time::timeout(lifecycle.register_timeout, read_frame(reader)).await;
    let node_id = match first {
        Err(_) => {
            ERROR_LOG.log_error(format!("[relay] {peer_addr}: register deadline exceeded"));
            return None;
        }
        Ok(Err(error)) => {
            ERROR_LOG.log_error(format!(
                "[relay] {peer_addr}: link closed before register: {error}"
            ));
            return None;
        }
        Ok(Ok(Frame::Register { node_id })) => node_id,
        Ok(Ok(other)) => {
            ERROR_LOG.log_error(format!(
                "[relay] {peer_addr}: first frame must be Register, got {other:?}"
            ));
            return None;
        }
    };

    if !node_id.eq_ignore_ascii_case(peer_fingerprint) {
        ERROR_LOG.log_error(format!(
            "[relay] {peer_addr}: register node id {node_id:?} does not match peer fingerprint {peer_fingerprint}"
        ));
        return None;
    }

    let registered = table.register(&node_id, outbound_tx.clone());
    match &registered.previous {
        Some(previous) => {
            let _ = previous.retire_tx.send(true);
            let _ = events.try_send(RelayLifecycleEvent::Replaced {
                node_id: node_id.clone(),
            });
        }
        None => {
            let _ = events.try_send(RelayLifecycleEvent::Registered {
                node_id: node_id.clone(),
            });
        }
    }
    Some(RegisteredLink {
        node_id,
        connection_id: registered.connection_id,
        retire_rx: registered.retire_rx,
    })
}

async fn dispatch_loop(
    reader: &mut ReadHalf<ServerTls>,
    peer_addr: SocketAddr,
    table: &Arc<RelayConnectionTable>,
    routing: &Arc<RoutingTable>,
    events: &mpsc::Sender<RelayLifecycleEvent>,
    registered: &RegisteredLink,
    outbound_tx: &mpsc::Sender<Frame>,
) {
    let node_id = &registered.node_id;
    let connection_id = registered.connection_id;
    let mut retire_rx = registered.retire_rx.clone();
    loop {
        tokio::select! {
            _ = retire_rx.changed() => {
                // Replaced or evicted: the table entry is already gone or
                // owned by a successor; the epilogue removes only if current.
                break;
            }
            read = read_frame(reader) => {
                match read {
                    Ok(Frame::Heartbeat) => table.touch(node_id, connection_id),
                    Ok(Frame::Unregister) => {
                        if table.remove_if_current(node_id, connection_id) {
                            let _ = events.try_send(RelayLifecycleEvent::Unregistered {
                                node_id: node_id.clone(),
                            });
                        }
                        break;
                    }
                    Ok(Frame::OpenStream { stream_id, target_node_id }) => {
                        if !handle_open_stream(
                            stream_id, target_node_id, node_id, connection_id,
                            table, routing, outbound_tx,
                        ).await {
                            break;
                        }
                    }
                    Ok(Frame::Data { stream_id, payload }) => {
                        forward_routed(routing, connection_id, stream_id, outbound_tx, |peer_stream_id| {
                            Frame::Data { stream_id: peer_stream_id, payload }
                        }).await;
                    }
                    Ok(Frame::Window { stream_id, credit }) => {
                        forward_routed(routing, connection_id, stream_id, outbound_tx, |peer_stream_id| {
                            Frame::Window { stream_id: peer_stream_id, credit }
                        }).await;
                    }
                    Ok(Frame::Close { stream_id }) => {
                        match routing.close_leg(RouteKey { connection_id, stream_id }) {
                            CloseOutcome::Forward(leg) => {
                                if leg.peer_outbound.send(Frame::Close { stream_id: leg.peer.stream_id }).await.is_err() {
                                    ERROR_LOG.log_error(format!(
                                        "[relay] {peer_addr} ({node_id}): peer link gone while forwarding close on stream {stream_id}"
                                    ));
                                }
                            }
                            CloseOutcome::Swallow => {
                                ERROR_LOG.log_error(format!(
                                    "[relay] {peer_addr} ({node_id}): duplicate close on stream {stream_id}"
                                ));
                            }
                            CloseOutcome::Unknown => {
                                send_stream_error(outbound_tx, stream_id, error_code::STREAM_UNKNOWN, "unknown stream").await;
                            }
                        }
                    }
                    Ok(Frame::CloseStream { stream_id }) => {
                        if let Some(leg) = routing.teardown_pair(RouteKey { connection_id, stream_id }) {
                            let _ = leg.peer_outbound.send(Frame::CloseStream { stream_id: leg.peer.stream_id }).await;
                        } else {
                            send_stream_error(outbound_tx, stream_id, error_code::STREAM_UNKNOWN, "unknown stream").await;
                        }
                    }
                    Ok(other) => {
                        ERROR_LOG.log_error(format!(
                            "[relay] {peer_addr} ({node_id}): unexpected frame on relay link: {other:?}"
                        ));
                        break;
                    }
                    Err(error) => {
                        ERROR_LOG.log_error(format!(
                            "[relay] {peer_addr} ({node_id}): link closed: {error}"
                        ));
                        break;
                    }
                }
            }
        }
    }
}

/// Handles a node-initiated `OpenStream`. Returns false when the link must
/// be torn down (violation); structured errors are sent inline otherwise.
async fn handle_open_stream(
    stream_id: u32,
    target_node_id: String,
    node_id: &str,
    connection_id: u64,
    table: &Arc<RelayConnectionTable>,
    routing: &Arc<RoutingTable>,
    outbound_tx: &mpsc::Sender<Frame>,
) -> bool {
    if stream_id % 2 == 0 {
        ERROR_LOG.log_error(format!(
            "[relay] ({node_id}): OpenStream with even (relay-initiated) stream id {stream_id}"
        ));
        return false;
    }
    let Some(target) = table.lookup(&target_node_id) else {
        send_stream_error(
            outbound_tx,
            stream_id,
            error_code::TARGET_UNKNOWN,
            "target node is unknown or offline",
        )
        .await;
        return true;
    };
    let relay_stream_id = target.next_relay_stream.fetch_add(2, Ordering::SeqCst);
    let forward = routing.open(
        RouteKey {
            connection_id,
            stream_id,
        },
        target.connection_id,
        relay_stream_id,
        target.outbound.clone(),
        outbound_tx.clone(),
    );
    if forward
        .peer_outbound
        .send(Frame::OpenStream {
            stream_id: relay_stream_id,
            target_node_id: node_id.to_string(),
        })
        .await
        .is_err()
    {
        // The target link is dying; its teardown removes the pair. Tell the
        // requester the target is unreachable now.
        ERROR_LOG.log_error(format!(
            "[relay] ({node_id}): target {target_node_id} link closed while opening stream {stream_id}"
        ));
        send_stream_error(
            outbound_tx,
            stream_id,
            error_code::TARGET_UNKNOWN,
            "target link closed during open",
        )
        .await;
    }
    true
}

/// Forwards a `Data`/`Window` frame along its route. Unknown or closed
/// streams get a structured error and the link stays up.
async fn forward_routed(
    routing: &Arc<RoutingTable>,
    connection_id: u64,
    stream_id: u32,
    outbound_tx: &mpsc::Sender<Frame>,
    build: impl FnOnce(u32) -> Frame,
) {
    match routing.lookup(RouteKey {
        connection_id,
        stream_id,
    }) {
        Lookup::Open(leg) => {
            if leg
                .peer_outbound
                .send(build(leg.peer.stream_id))
                .await
                .is_err()
            {
                // Peer link is dying; its teardown removes the pair and
                // notifies this side with CloseStream.
                send_stream_error(
                    outbound_tx,
                    stream_id,
                    error_code::STREAM_UNKNOWN,
                    "peer link closed",
                )
                .await;
            }
        }
        Lookup::Closed => {
            send_stream_error(
                outbound_tx,
                stream_id,
                error_code::STREAM_CLOSED,
                "stream is closed",
            )
            .await;
        }
        Lookup::Unknown => {
            send_stream_error(
                outbound_tx,
                stream_id,
                error_code::STREAM_UNKNOWN,
                "unknown stream",
            )
            .await;
        }
    }
}

async fn send_stream_error(
    outbound_tx: &mpsc::Sender<Frame>,
    stream_id: u32,
    code: u16,
    message: &str,
) {
    let _ = outbound_tx
        .send(Frame::Error {
            stream_id,
            code,
            message: message.to_string(),
        })
        .await;
}

/// Drains the outbound queue into the link's write half. Exits when every
/// sender dropped (link teardown) or the write fails (peer gone).
async fn link_writer_loop(mut writer: WriteHalf<ServerTls>, mut outbound: mpsc::Receiver<Frame>) {
    while let Some(frame) = outbound.recv().await {
        if let Err(error) = write_frame(&mut writer, &frame).await {
            ERROR_LOG.log_error(format!("[relay] link writer failed: {error}"));
            return;
        }
    }
}

#[cfg(test)]
mod tests;
