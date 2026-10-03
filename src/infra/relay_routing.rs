//! Relay stream routing: pairs of stream legs across two registered node
//! links, with header-only forwarding — node-to-node payloads stay inner-TLS
//! ciphertext end to end (docs/relay-design.md 协议分层: the relay decodes
//! outer frame headers only).
//!
//! A routed stream is a pair of [`RouteLeg`]s, one per direction:
//!
//! ```text
//! node A (stream X, odd)                node B (stream Y, even)
//!   | write Data{X}                       ^ read Data{Y}
//!   v                                     |
//! relay link A -- leg (A,X) --> leg (B,Y) -- relay link B
//! ```
//!
//! The relay remaps ids and forwards `Data` / `Window` / half-close `Close`
//! frames between the legs; `CloseStream` tears the whole pair down. Legs
//! carry a `closed` flag so a one-direction half-close keeps the reverse
//! leg alive until it also closes (TCP semantics end to end).
//!
//! Lock discipline: a single mutex guards the leg map; every method takes
//! short critical sections and never awaits while holding it. Senders are
//! cloned out and sent to afterwards by callers.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::infra::relay_scheduler::SchedulerIngress;

/// Structured relay error semantics carried by `Frame::Error` (issue #36):
/// typed codes with stable wire values plus a canonical human-readable
/// message per code. The wire format is unchanged — `Frame::Error.code`
/// stays a raw `u16`; [`error_code::RelayErrorCode::from_wire`] decodes known
/// values and rejects anything outside the assigned range.
pub mod error_code {
    /// Typed relay error codes; the discriminant is the wire value carried
    /// by `Frame::Error.code`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u16)]
    pub enum RelayErrorCode {
        /// The target node id is not in the connection table (unknown or
        /// offline — the table evicts offline nodes).
        TargetUnknown = 0x0001,
        /// The stream id has no routing entry on this link.
        StreamUnknown = 0x0002,
        /// The stream id is routed but this direction is already closed.
        StreamClosed = 0x0003,
        /// Register refused: the connection table is at max_nodes.
        NodeCapacity = 0x0004,
        /// OpenStream refused: the routing table is at max_streams.
        StreamCapacity = 0x0005,
        /// OpenStream refused: the forwarded-bytes/s threshold is exceeded.
        ThroughputExceeded = 0x0006,
        /// Enrollment refused: the token is unknown or already consumed.
        TokenInvalid = 0x0007,
        /// Enrollment refused: the token is past its expiry.
        TokenExpired = 0x0008,
        /// The relay operator revoked the node; its live link was dropped.
        NodeRevoked = 0x0009,
        /// Heartbeat silence past the offline deadline; the link was evicted.
        HeartbeatLost = 0x000a,
        /// Quota exhausted; reserved for quota enforcement — no emitter
        /// assigns it yet.
        #[allow(dead_code)]
        QuotaExceeded = 0x000b,
    }

    impl RelayErrorCode {
        /// Every assigned code, for table-driven checks.
        const ALL: [RelayErrorCode; 11] = [
            RelayErrorCode::TargetUnknown,
            RelayErrorCode::StreamUnknown,
            RelayErrorCode::StreamClosed,
            RelayErrorCode::NodeCapacity,
            RelayErrorCode::StreamCapacity,
            RelayErrorCode::ThroughputExceeded,
            RelayErrorCode::TokenInvalid,
            RelayErrorCode::TokenExpired,
            RelayErrorCode::NodeRevoked,
            RelayErrorCode::HeartbeatLost,
            RelayErrorCode::QuotaExceeded,
        ];

        /// Decodes a wire value from `Frame::Error.code`; `None` when the
        /// value has no assigned meaning.
        pub fn from_wire(code: u16) -> Option<Self> {
            Self::ALL
                .into_iter()
                .find(|assigned| assigned.wire_value() == code)
        }

        /// The wire value carried by `Frame::Error.code`.
        pub fn wire_value(self) -> u16 {
            self as u16
        }

        /// The canonical human-readable explanation of this error.
        pub fn message(self) -> &'static str {
            match self {
                RelayErrorCode::TargetUnknown => "target node is unknown or offline",
                RelayErrorCode::StreamUnknown => "stream is unknown on this link",
                RelayErrorCode::StreamClosed => "stream direction is already closed",
                RelayErrorCode::NodeCapacity => "relay node capacity reached",
                RelayErrorCode::StreamCapacity => "relay stream capacity reached",
                RelayErrorCode::ThroughputExceeded => "relay throughput limit exceeded",
                RelayErrorCode::TokenInvalid => "enrollment token is invalid or already used",
                RelayErrorCode::TokenExpired => "enrollment token has expired",
                RelayErrorCode::NodeRevoked => "node access revoked by the relay operator",
                RelayErrorCode::HeartbeatLost => "relay heartbeat timed out; the link was evicted",
                RelayErrorCode::QuotaExceeded => "quota exceeded",
            }
        }
    }
}

/// One endpoint of a routed stream: a link (by connection id) plus the
/// stream id as seen on that link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RouteKey {
    pub(crate) connection_id: u64,
    pub(crate) stream_id: u32,
}

/// Directional routing state.
pub(crate) struct RouteLeg {
    /// The opposite endpoint.
    pub(crate) peer: RouteKey,
    /// The peer link's outbound queue: frames for the peer are queued here
    /// (remapped to `peer.stream_id`) by the caller.
    pub(crate) peer_outbound: SchedulerIngress,
    /// This direction's write leg is closed (a half-close `Close` arrived
    /// from this side and was forwarded).
    pub(crate) closed: bool,
}

/// What [`RoutingTable::lookup`] found for an incoming stream frame.
pub(crate) enum Lookup {
    Open(RouteLeg),
    Closed,
    Unknown,
}

pub(crate) enum CloseOutcome {
    /// Forward a half-close `Close` to the peer with the remapped id.
    Forward(RouteLeg),
    /// The close was already recorded for this leg; nothing to forward.
    Swallow,
    Unknown,
}

/// The routing table. Two legs per routed stream. Entries are unbounded
/// until the capacity model lands (docs/relay-design.md 容量评估).
#[derive(Default)]
pub(crate) struct RoutingTable {
    legs: Mutex<HashMap<RouteKey, RouteLeg>>,
}

impl RoutingTable {
    /// Opens a routed stream from `from` toward the target link: allocates
    /// the relay-side (even) stream id on the target, inserts both legs,
    /// and returns the leg facing the target (for forwarding the
    /// `OpenStream`) plus the allocated id.
    pub(crate) fn open(
        &self,
        from: RouteKey,
        target_connection_id: u64,
        target_stream_id: u32,
        target_outbound: SchedulerIngress,
        from_outbound: SchedulerIngress,
    ) -> RouteLeg {
        let to = RouteKey {
            connection_id: target_connection_id,
            stream_id: target_stream_id,
        };
        let forward = RouteLeg {
            peer: to,
            peer_outbound: target_outbound,
            closed: false,
        };
        let backward = RouteLeg {
            peer: from,
            peer_outbound: from_outbound,
            closed: false,
        };
        let mut legs = self.legs.lock().expect("relay routing table lock poisoned");
        legs.insert(from, forward.clone_leg());
        legs.insert(to, backward);
        forward
    }

    /// Looks up the leg for an incoming frame.
    pub(crate) fn lookup(&self, key: RouteKey) -> Lookup {
        let legs = self.legs.lock().expect("relay routing table lock poisoned");
        match legs.get(&key) {
            Some(leg) if leg.closed => Lookup::Closed,
            Some(leg) => Lookup::Open(leg.clone_leg()),
            None => Lookup::Unknown,
        }
    }

    /// Records a half-close from `key`'s side: marks the leg closed,
    /// removes the pair once both legs are closed, and says whether the
    /// peer should receive the forwarded `Close`.
    pub(crate) fn close_leg(&self, key: RouteKey) -> CloseOutcome {
        let mut legs = self.legs.lock().expect("relay routing table lock poisoned");
        let Some(leg) = legs.get_mut(&key) else {
            return CloseOutcome::Unknown;
        };
        if leg.closed {
            // Duplicate close on an already-closed direction: benign in
            // flight, nothing more to forward.
            return CloseOutcome::Swallow;
        }
        leg.closed = true;
        let peer_key = leg.peer;
        let forward = leg.clone_leg();
        let peer_closed = legs.get(&peer_key).map(|peer| peer.closed).unwrap_or(true);
        if peer_closed {
            legs.remove(&key);
            legs.remove(&peer_key);
        }
        CloseOutcome::Forward(forward)
    }

    /// Tears down a single routed stream from either side: removes both
    /// legs and returns the leg facing the peer (for a `CloseStream` with
    /// the peer's stream id).
    pub(crate) fn teardown_pair(&self, key: RouteKey) -> Option<RouteLeg> {
        let mut legs = self.legs.lock().expect("relay routing table lock poisoned");
        let leg = legs.remove(&key)?;
        legs.remove(&leg.peer);
        Some(leg)
    }

    /// Returns the number of routed streams (two legs per stream).
    pub(crate) fn stream_count(&self) -> usize {
        self.legs.lock().map(|legs| legs.len() / 2).unwrap_or(0)
    }

    /// Tears down every route touching `connection_id`. Returns one entry
    /// per affected peer link: the peer's outbound queue and the stream id
    /// (as the peer knows it) for a `CloseStream`.
    pub(crate) fn teardown_connection(&self, connection_id: u64) -> Vec<(SchedulerIngress, u32)> {
        let mut legs = self.legs.lock().expect("relay routing table lock poisoned");
        let keys: Vec<RouteKey> = legs
            .keys()
            .copied()
            .filter(|key| key.connection_id == connection_id)
            .collect();
        let mut peers = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(leg) = legs.remove(&key) {
                // The mirror leg is keyed by the peer; drop it too.
                legs.remove(&leg.peer);
                peers.push((leg.peer_outbound, leg.peer.stream_id));
            }
        }
        peers
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.legs.lock().map(|legs| legs.len()).unwrap_or(0)
    }
}

impl RouteLeg {
    fn clone_leg(&self) -> RouteLeg {
        RouteLeg {
            peer: self.peer,
            peer_outbound: self.peer_outbound.clone(),
            closed: self.closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use crate::infra::relay_mux::frame::Frame;

    fn outbound() -> (
        SchedulerIngress,
        mpsc::Receiver<Frame>,
        mpsc::Receiver<Frame>,
    ) {
        SchedulerIngress::test_channels(8)
    }

    fn key(connection_id: u64, stream_id: u32) -> RouteKey {
        RouteKey {
            connection_id,
            stream_id,
        }
    }

    #[test]
    fn open_inserts_two_legs_and_routes_both_ways() {
        let table = RoutingTable::default();
        let (a_tx, _, _) = outbound();
        let (b_tx, _, _) = outbound();
        let from = key(1, 1);
        let forward = table.open(from, 2, 2, b_tx, a_tx);
        assert_eq!(forward.peer, key(2, 2));
        assert_eq!(table.len(), 2);
        assert!(matches!(table.lookup(from), Lookup::Open(_)));
        assert!(matches!(table.lookup(key(2, 2)), Lookup::Open(_)));
    }

    #[test]
    fn close_leg_forwards_then_drops_pair_after_both_close() {
        let table = RoutingTable::default();
        let (a_tx, _, _) = outbound();
        let (b_tx, _, _) = outbound();
        let a = key(1, 1);
        let b = key(2, 2);
        table.open(a, b.connection_id, b.stream_id, b_tx, a_tx);

        assert!(matches!(table.close_leg(a), CloseOutcome::Forward(_)));
        assert!(matches!(table.lookup(a), Lookup::Closed));
        assert!(
            matches!(table.lookup(b), Lookup::Open(_)),
            "reverse leg stays open"
        );
        assert_eq!(table.len(), 2, "pair survives a one-direction close");

        assert!(matches!(table.close_leg(b), CloseOutcome::Forward(_)));
        assert_eq!(table.len(), 0, "pair drops after both directions close");
        assert!(matches!(table.lookup(a), Lookup::Unknown));
    }

    #[test]
    fn duplicate_close_is_swallowed() {
        let table = RoutingTable::default();
        let (a_tx, _, _) = outbound();
        let (b_tx, _, _) = outbound();
        let a = key(1, 1);
        table.open(a, 2, 2, b_tx, a_tx);
        assert!(matches!(table.close_leg(a), CloseOutcome::Forward(_)));
        assert!(matches!(table.close_leg(a), CloseOutcome::Swallow));
    }

    #[test]
    fn teardown_connection_notifies_each_peer_once() {
        let table = RoutingTable::default();
        let (a_tx, _, _) = outbound();
        let (b_tx, _, _) = outbound();
        let (c_tx, _, _) = outbound();
        // A talks to B and C; B also talks to C (untouched by A's death).
        table.open(key(1, 1), 2, 2, b_tx.clone(), a_tx.clone());
        table.open(key(1, 3), 3, 2, c_tx.clone(), a_tx);
        table.open(key(2, 3), 3, 4, c_tx, b_tx);

        let mut peers = table.teardown_connection(1);
        peers.sort_by_key(|(_, stream_id)| *stream_id);
        let notified: Vec<u32> = peers.iter().map(|(_, stream_id)| *stream_id).collect();
        assert_eq!(notified, vec![2, 2], "B and C each hear about one stream");
        assert_eq!(
            table.len(),
            2,
            "the B<->C route survives an unrelated teardown"
        );
    }

    #[test]
    fn teardown_of_unknown_connection_is_empty() {
        let table = RoutingTable::default();
        assert!(table.teardown_connection(99).is_empty());
    }

    mod error_code {
        use super::super::error_code::RelayErrorCode;

        /// (code, wire value, canonical message) — the table pins both the
        /// assigned wire numbers and the canonical wording.
        const TABLE: &[(RelayErrorCode, u16, &str)] = &[
            (
                RelayErrorCode::TargetUnknown,
                0x0001,
                "target node is unknown or offline",
            ),
            (
                RelayErrorCode::StreamUnknown,
                0x0002,
                "stream is unknown on this link",
            ),
            (
                RelayErrorCode::StreamClosed,
                0x0003,
                "stream direction is already closed",
            ),
            (
                RelayErrorCode::NodeCapacity,
                0x0004,
                "relay node capacity reached",
            ),
            (
                RelayErrorCode::StreamCapacity,
                0x0005,
                "relay stream capacity reached",
            ),
            (
                RelayErrorCode::ThroughputExceeded,
                0x0006,
                "relay throughput limit exceeded",
            ),
            (
                RelayErrorCode::TokenInvalid,
                0x0007,
                "enrollment token is invalid or already used",
            ),
            (
                RelayErrorCode::TokenExpired,
                0x0008,
                "enrollment token has expired",
            ),
            (
                RelayErrorCode::NodeRevoked,
                0x0009,
                "node access revoked by the relay operator",
            ),
            (
                RelayErrorCode::HeartbeatLost,
                0x000a,
                "relay heartbeat timed out; the link was evicted",
            ),
            (RelayErrorCode::QuotaExceeded, 0x000b, "quota exceeded"),
        ];

        #[test]
        fn from_wire_roundtrips_every_assigned_code() {
            for (code, wire, _) in TABLE {
                assert_eq!(
                    code.wire_value(),
                    *wire,
                    "{code:?} keeps its assigned wire value"
                );
                assert_eq!(
                    RelayErrorCode::from_wire(*wire),
                    Some(*code),
                    "wire {wire:#06x} must decode to {code:?}"
                );
            }
        }

        #[test]
        fn from_wire_rejects_unassigned_values() {
            assert_eq!(
                RelayErrorCode::from_wire(0x0000),
                None,
                "zero is unassigned"
            );
            assert_eq!(
                RelayErrorCode::from_wire(0x000c),
                None,
                "the value above the assigned range is unknown"
            );
        }

        #[test]
        fn every_code_has_a_canonical_message() {
            for (code, _, message) in TABLE {
                assert!(
                    !message.is_empty(),
                    "{code:?} must carry a non-empty canonical message"
                );
                assert_eq!(code.message(), *message);
            }
        }
    }
}
