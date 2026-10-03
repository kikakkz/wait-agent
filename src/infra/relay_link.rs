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

use tokio::io::ReadHalf;
use tokio::sync::mpsc;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::node_credentials;
use crate::infra::relay_capacity::{OpenAdmission, RelayCapacityConfig, SharedUsageMeter};
use crate::infra::relay_connection_table::{
    RelayConnectionTable, RelayLifecycleConfig, RelayLifecycleEvent,
};
use crate::infra::relay_mux::frame::{read_frame, Frame};
use crate::infra::relay_presence::PresenceHub;
use crate::infra::relay_routing::{error_code, CloseOutcome, Lookup, RouteKey, RoutingTable};
use crate::infra::relay_scheduler::{LinkScheduler, SchedulerConfig, SchedulerIngress};

/// Bounded outbound queue per link: frames waiting for the writer task.
pub(crate) const LINK_OUTBOUND_QUEUE: usize = 256;

pub(crate) type ServerTls = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

/// Runs one authenticated link to completion: registration deadline,
/// fingerprint-checked register, then heartbeat/unregister/routing dispatch
/// until either side ends it. All routing state touching this connection is
/// torn down on exit and surviving peers receive `CloseStream`; the link's
/// presence watch interests are purged and, when the link owned the table
/// entry (loss, unregister), watchers are told it went offline.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_link(
    tls: ServerTls,
    peer_addr: SocketAddr,
    table: Arc<RelayConnectionTable>,
    routing: Arc<RoutingTable>,
    events: mpsc::Sender<RelayLifecycleEvent>,
    lifecycle: RelayLifecycleConfig,
    capacity: RelayCapacityConfig,
    meter: SharedUsageMeter,
    presence: Arc<PresenceHub>,
) {
    let peer_fingerprint = match peer_fingerprint(&tls, peer_addr) {
        Some(fingerprint) => fingerprint,
        None => return,
    };

    let (mut reader, writer) = tokio::io::split(tls);
    // Egress scheduling: every producer (connection entry, routing legs,
    // presence hub) clones this handle; the scheduler classifies frames
    // onto its bulk/control ingress channels and adds per-stream bounds and
    // fairness between streams.
    let (outbound_tx, scheduler) = LinkScheduler::spawn(
        SchedulerConfig {
            ingress_capacity: LINK_OUTBOUND_QUEUE,
            ..SchedulerConfig::default()
        },
        writer,
    );

    let registered = match register_link(
        &mut reader,
        peer_addr,
        &peer_fingerprint,
        &table,
        &events,
        &outbound_tx,
        &lifecycle,
        &capacity,
        &presence,
    )
    .await
    {
        Some(state) => state,
        None => {
            drop(outbound_tx);
            drop(reader);
            scheduler.shutdown().await;
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
        &capacity,
        &meter,
        &presence,
    )
    .await;

    // Epilogue: drop this link's table entry and every route touching it;
    // surviving peers get CloseStream for their side. When this link still
    // owned the table entry (link loss; unregister removes it in the dispatch
    // arm) watchers learn the node went offline. Eviction and admin remove
    // publish at their own sites; replacement publishes nothing (no churn).
    // The watch purge is unconditional: every exit cause ends the interests.
    if table.remove_if_current(&registered.node_id, registered.connection_id) {
        presence.publish(&registered.node_id, false);
    }
    presence.unwatch_connection(registered.connection_id);
    for (peer_outbound, peer_stream_id) in routing.teardown_connection(registered.connection_id) {
        let _ = peer_outbound.try_send(Frame::CloseStream {
            stream_id: peer_stream_id,
        });
    }
    drop(outbound_tx);
    drop(reader);
    scheduler.shutdown().await;
}

struct RegisteredLink {
    node_id: String,
    connection_id: u64,
    retire_rx: tokio::sync::watch::Receiver<bool>,
}

pub(crate) fn peer_fingerprint(tls: &ServerTls, peer_addr: SocketAddr) -> Option<String> {
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
#[allow(clippy::too_many_arguments)]
async fn register_link(
    reader: &mut ReadHalf<ServerTls>,
    peer_addr: SocketAddr,
    peer_fingerprint: &str,
    table: &Arc<RelayConnectionTable>,
    events: &mpsc::Sender<RelayLifecycleEvent>,
    outbound_tx: &SchedulerIngress,
    lifecycle: &RelayLifecycleConfig,
    capacity: &RelayCapacityConfig,
    presence: &Arc<PresenceHub>,
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

    // Admission: a NEW node counts against max_nodes; replacing an existing
    // entry never grows the table, so reconnects re-admit even at the cap.
    if table.lookup(&node_id).is_none() && !capacity.admit_register(table.len()) {
        ERROR_LOG.log_error(format!(
            "[relay] {peer_addr} ({node_id}): register refused, max_nodes reached"
        ));
        send_stream_error(
            outbound_tx,
            0,
            error_code::NODE_CAPACITY,
            "relay is at max_nodes capacity",
        )
        .await;
        return None;
    }

    let registered = table.register(&node_id, outbound_tx.clone());
    match &registered.previous {
        Some(previous) => {
            // Reconnect/re-register: the table entry moves to the new link
            // without an offline/online churn — watchers keep their state.
            let _ = previous.retire_tx.send(true);
            let _ = events.try_send(RelayLifecycleEvent::Replaced {
                node_id: node_id.clone(),
            });
        }
        None => {
            let _ = events.try_send(RelayLifecycleEvent::Registered {
                node_id: node_id.clone(),
            });
            presence.publish(&node_id, true);
        }
    }
    Some(RegisteredLink {
        node_id,
        connection_id: registered.connection_id,
        retire_rx: registered.retire_rx,
    })
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_loop(
    reader: &mut ReadHalf<ServerTls>,
    peer_addr: SocketAddr,
    table: &Arc<RelayConnectionTable>,
    routing: &Arc<RoutingTable>,
    events: &mpsc::Sender<RelayLifecycleEvent>,
    registered: &RegisteredLink,
    outbound_tx: &SchedulerIngress,
    capacity: &RelayCapacityConfig,
    meter: &SharedUsageMeter,
    presence: &Arc<PresenceHub>,
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
                            // Unregister removes the entry here, so the
                            // epilogue's removal sees nothing; publish the
                            // offline transition at the cause.
                            presence.publish(node_id, false);
                        }
                        break;
                    }
                    Ok(Frame::Watch { node_id: target }) => {
                        // Presence subscription: the hub answers with an
                        // immediate replay on this link's queue. Watching an
                        // unknown or offline target simply replays offline.
                        let currently_online = table.lookup(&target).is_some();
                        presence.watch(
                            connection_id,
                            &target,
                            outbound_tx.clone(),
                            currently_online,
                        );
                    }
                    Ok(Frame::OpenStream { stream_id, target_node_id }) => {
                        if !handle_open_stream(
                            stream_id, target_node_id, node_id, connection_id,
                            table, routing, outbound_tx, capacity, meter,
                        ).await {
                            break;
                        }
                    }
                    Ok(Frame::Data { stream_id, payload }) => {
                        forward_routed(routing, connection_id, stream_id, outbound_tx, meter, payload.len(), |peer_stream_id| {
                            Frame::Data { stream_id: peer_stream_id, payload }
                        }).await;
                    }
                    Ok(Frame::Window { stream_id, credit }) => {
                        forward_routed(routing, connection_id, stream_id, outbound_tx, meter, 0, |peer_stream_id| {
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
#[allow(clippy::too_many_arguments)]
async fn handle_open_stream(
    stream_id: u32,
    target_node_id: String,
    node_id: &str,
    connection_id: u64,
    table: &Arc<RelayConnectionTable>,
    routing: &Arc<RoutingTable>,
    outbound_tx: &SchedulerIngress,
    capacity: &RelayCapacityConfig,
    meter: &SharedUsageMeter,
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
    // Capacity admission (docs/relay-design.md 容量评估与准入控制): refuse
    // new streams past max_streams or while the forwarded rate is over the
    // threshold — structured rejection, no route state created.
    match capacity.admit_open(routing.stream_count(), meter) {
        OpenAdmission::Allow => {}
        OpenAdmission::StreamsFull => {
            send_stream_error(
                outbound_tx,
                stream_id,
                error_code::STREAM_CAPACITY,
                "relay is at max_streams capacity",
            )
            .await;
            return true;
        }
        OpenAdmission::ThroughputExceeded => {
            send_stream_error(
                outbound_tx,
                stream_id,
                error_code::THROUGHPUT_EXCEEDED,
                "relay forwarded-throughput threshold exceeded",
            )
            .await;
            return true;
        }
    }
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
/// streams get a structured error and the link stays up. `forwarded_bytes`
/// feeds the throughput meter after a successful enqueue (zero for
/// non-Data frames).
async fn forward_routed(
    routing: &Arc<RoutingTable>,
    connection_id: u64,
    stream_id: u32,
    outbound_tx: &SchedulerIngress,
    meter: &SharedUsageMeter,
    forwarded_bytes: usize,
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
            } else if forwarded_bytes > 0 {
                meter.record(forwarded_bytes);
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
    outbound_tx: &SchedulerIngress,
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

#[cfg(test)]
mod tests;
