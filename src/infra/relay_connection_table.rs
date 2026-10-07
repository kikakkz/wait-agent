//! Relay connection table: the in-memory `node_id -> connection` registry
//! and its liveness bookkeeping.
//!
//! Governing design: docs/relay-design.md (node 生命周期, 数据策略). The
//! table is memory-only — the relay persists no session or traffic data;
//! on-disk state stays limited to relay.toml, the whitelist directory, and
//! token parameters. Entries appear on `Register` and disappear on
//! `Unregister`, link loss, replacement by a fresher register (reconnect /
//! re-register), or heartbeat-timeout eviction (3 missed beats, default
//! 30s).
//!
//! Lock order (extends the relay module documentation): the table mutex is
//! the outer lock; per-entry `last_seen` mutexes are inner locks. No lock
//! is held across an `.await`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::infra::relay_mux::frame::Frame;
use crate::infra::relay_routing::error_code::RelayErrorCode;
use crate::infra::relay_scheduler::SchedulerIngress;

/// Default eviction deadline: three missed 10s beats = 30s of silence.
pub const DEFAULT_OFFLINE_AFTER: Duration = Duration::from_secs(30);

/// Default sweep cadence of the eviction task.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Default deadline for the first frame on a fresh link to be `Register`.
pub const DEFAULT_REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

/// Timing knobs for the connection-table lifecycle. Defaults follow the
/// design; tests shrink them via [`RelayLifecycleConfig::fast_for_tests`].
#[derive(Debug, Clone)]
pub struct RelayLifecycleConfig {
    /// Silence longer than this marks a node offline and evicts it.
    pub offline_after: Duration,
    /// How often the sweeper checks for idle entries.
    pub sweep_interval: Duration,
    /// How long a fresh link may take to send its first `Register`.
    pub register_timeout: Duration,
}

impl Default for RelayLifecycleConfig {
    fn default() -> Self {
        Self {
            offline_after: DEFAULT_OFFLINE_AFTER,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            register_timeout: DEFAULT_REGISTER_TIMEOUT,
        }
    }
}

impl RelayLifecycleConfig {
    /// Returns aggressively short timing for tests.
    #[cfg(test)]
    pub fn fast_for_tests() -> Self {
        Self {
            offline_after: Duration::from_millis(200),
            sweep_interval: Duration::from_millis(25),
            register_timeout: Duration::from_secs(2),
        }
    }
}

/// Observable lifecycle transitions (the test hook named by the issue).
/// Events are ephemeral notifications, not data: a full or absent receiver
/// drops them, which never affects connection handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayLifecycleEvent {
    Registered {
        node_id: String,
    },
    /// A fresher register replaced the previous live entry for this node.
    Replaced {
        node_id: String,
    },
    Unregistered {
        node_id: String,
    },
    /// Evicted after `offline_after` of silence (three missed heartbeats).
    EvictedOffline {
        node_id: String,
    },
}

/// One registered link. Dropping the entry drops the `retire_tx` sender,
/// which the link task observes as its shutdown signal.
pub(crate) struct ConnectionEntry {
    pub(crate) connection_id: u64,
    pub(crate) retire_tx: watch::Sender<bool>,
    pub(crate) last_seen: Arc<Mutex<Instant>>,
    /// The link's outbound frame queue (its writer task drains this).
    pub(crate) outbound: SchedulerIngress,
    /// Allocator for relay-initiated (even) stream ids on this link.
    pub(crate) next_relay_stream: Arc<AtomicU32>,
    /// Host labels the node announced for itself over the node-channel
    /// `announce` admin request (issue #156 slice 3: fingerprint
    /// auto-discovery). Written once per (re)register, read by
    /// `resolve_label`; both happen under the table lock.
    pub(crate) labels: Vec<String>,
}

/// The outcome of matching a host label against the registered nodes'
/// announced labels (issue #156 slice 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LabelResolve {
    /// Exactly one registered node matches; carries its fingerprint (node
    /// id) and its announced labels for the admin response.
    Unique {
        node_id: String,
        labels: Vec<String>,
    },
    /// More than one registered node matches; the caller must not guess.
    Ambiguous(usize),
    /// No registered node matches.
    NoMatch,
}

/// A resolved routing target: everything needed to open a routed stream
/// toward a registered node.
pub(crate) struct RoutingTarget {
    pub(crate) connection_id: u64,
    pub(crate) outbound: SchedulerIngress,
    pub(crate) next_relay_stream: Arc<AtomicU32>,
}

/// The registry. Every method takes short critical sections; the sweeper
/// and the link tasks never hold the table lock across an await.
#[derive(Default)]
pub(crate) struct RelayConnectionTable {
    entries: Mutex<HashMap<String, ConnectionEntry>>,
    next_connection_id: AtomicU64,
}

impl RelayConnectionTable {
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().map(|table| table.len()).unwrap_or(0)
    }

    /// Inserts (or replaces) the node. Returns the fresh entry's handle
    /// plus the previous entry when this was a replacement — the caller
    /// retires the stale link and emits `Replaced`.
    pub(crate) fn register(&self, node_id: &str, outbound: SchedulerIngress) -> RegisteredEntry {
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::SeqCst);
        let (retire_tx, retire_rx) = watch::channel(false);
        let entry = ConnectionEntry {
            connection_id,
            retire_tx: retire_tx.clone(),
            last_seen: Arc::new(Mutex::new(Instant::now())),
            outbound,
            next_relay_stream: Arc::new(AtomicU32::new(2)),
            labels: Vec::new(),
        };
        let previous = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned")
            .insert(node_id.to_string(), entry);
        RegisteredEntry {
            connection_id,
            retire_rx,
            previous,
        }
    }

    /// Resolves a routing target by node id.
    pub(crate) fn lookup(&self, node_id: &str) -> Option<RoutingTarget> {
        self.entries
            .lock()
            .expect("relay connection table lock poisoned")
            .get(node_id)
            .map(|entry| RoutingTarget {
                connection_id: entry.connection_id,
                outbound: entry.outbound.clone(),
                next_relay_stream: entry.next_relay_stream.clone(),
            })
    }

    /// Records activity from a link, guarding against a replaced link
    /// touching its successor's entry.
    pub(crate) fn touch(&self, node_id: &str, connection_id: u64) {
        let entry = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned")
            .get(node_id)
            .map(|entry| (entry.connection_id, entry.last_seen.clone()));
        let Some((current_id, last_seen)) = entry else {
            return;
        };
        if current_id != connection_id {
            return;
        }
        if let Ok(mut last_seen) = last_seen.lock() {
            *last_seen = Instant::now();
        };
    }

    /// Removes the entry only if `connection_id` still owns it, so a stale
    /// link cannot delete its replacement. Returns whether an entry was
    /// removed.
    pub(crate) fn remove_if_current(&self, node_id: &str, connection_id: u64) -> bool {
        let mut table = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned");
        match table.get(node_id) {
            Some(entry) if entry.connection_id == connection_id => {
                table.remove(node_id);
                true
            }
            _ => false,
        }
    }

    /// Removes the entry for `node_id` and signals its link task to retire,
    /// first queueing a `Frame::Error` carrying `code` on the removed link's
    /// outbound queue, so the node learns why the link is going away (issue
    /// #36). The Error frame is a best-effort notification: a full control
    /// queue must not block or prevent teardown, so the enqueue is
    /// `try_send`-based and its failure is ignored before the retire signal
    /// fires. (Register replacement retires the stale link via the handle
    /// returned from [`RelayConnectionTable::register`], not through here.)
    pub(crate) fn retire_notifying(&self, node_id: &str, code: RelayErrorCode) -> bool {
        let entry = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned")
            .remove(node_id);
        match entry {
            Some(entry) => {
                let _ = entry.outbound.try_send(Frame::Error {
                    stream_id: 0,
                    code: code.wire_value(),
                    message: code.message().to_string(),
                });
                let _ = entry.retire_tx.send(true);
                true
            }
            None => false,
        }
    }

    /// Removes and returns every entry silent for longer than `max_idle`.
    /// Callers retire the returned links and emit `EvictedOffline`.
    pub(crate) fn evict_idle(&self, max_idle: Duration) -> Vec<(String, ConnectionEntry)> {
        let now = Instant::now();
        let mut table = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned");
        let idle: Vec<String> = table
            .iter()
            .filter(|(_, entry)| {
                entry
                    .last_seen
                    .lock()
                    .map(|last_seen| now.duration_since(*last_seen) >= max_idle)
                    .unwrap_or(false)
            })
            .map(|(node_id, _)| node_id.clone())
            .collect();
        idle.into_iter()
            .filter_map(|node_id| table.remove(&node_id).map(|entry| (node_id, entry)))
            .collect()
    }

    /// Records the host labels a link announced for itself (issue #156
    /// slice 3). The write lands only when `connection_id` still owns the
    /// entry, so a stale link cannot label its successor (same guard as
    /// `touch`/`remove_if_current`). Labels are stored verbatim; matching
    /// normalizes case and trailing dots.
    pub(crate) fn set_labels(&self, node_id: &str, connection_id: u64, labels: Vec<String>) {
        let mut table = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned");
        if let Some(entry) = table.get_mut(node_id) {
            if entry.connection_id == connection_id {
                entry.labels = labels;
            }
        }
    }

    /// Matches a host label (what the operator typed as the profile's host)
    /// against every registered node's announced labels. Hostname labels
    /// match case-insensitively, ignoring one trailing dot, and in either
    /// dotted-suffix direction (`nas` matches `nas.local` and vice versa);
    /// IP-shaped labels must match exactly. One table lock covers the whole
    /// scan so the answer is a consistent point-in-time snapshot.
    pub(crate) fn resolve_label(&self, label: &str) -> LabelResolve {
        let table = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned");
        let mut matches: Vec<(String, Vec<String>)> = Vec::new();
        for (node_id, entry) in table.iter() {
            if entry
                .labels
                .iter()
                .any(|announced| labels_match(announced, label))
            {
                matches.push((node_id.clone(), entry.labels.clone()));
            }
        }
        match matches.len() {
            0 => LabelResolve::NoMatch,
            1 => {
                let (node_id, labels) = matches.pop().expect("one match present");
                LabelResolve::Unique { node_id, labels }
            }
            count => LabelResolve::Ambiguous(count),
        }
    }

    /// Returns a point-in-time view of the registered nodes for the admin
    /// socket: `(node_id, connection_id, idle milliseconds)`. The table
    /// lock is released before the per-entry `last_seen` reads (lock order:
    /// table, then entry).
    pub(crate) fn snapshot(&self) -> Vec<(String, u64, u128)> {
        let now = Instant::now();
        let entries = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned")
            .iter()
            .map(|(node_id, entry)| {
                (
                    node_id.clone(),
                    entry.connection_id,
                    entry.last_seen.clone(),
                )
            })
            .collect::<Vec<_>>();
        entries
            .into_iter()
            .map(|(node_id, connection_id, last_seen)| {
                let idle_ms = last_seen
                    .lock()
                    .map(|last_seen| now.duration_since(*last_seen).as_millis())
                    .unwrap_or(0);
                (node_id, connection_id, idle_ms)
            })
            .collect()
    }
}

/// Handle handed to the registering link task.
pub(crate) struct RegisteredEntry {
    pub(crate) connection_id: u64,
    pub(crate) retire_rx: watch::Receiver<bool>,
    pub(crate) previous: Option<ConnectionEntry>,
}

/// Normalizes a label for matching: lowercase, one trailing dot stripped.
fn normalize_label(label: &str) -> String {
    let mut normalized = label.trim().to_lowercase();
    if normalized.len() > 1 && normalized.ends_with('.') {
        normalized.pop();
    }
    normalized
}

/// Whether an announced label answers the operator-typed query. IPs match
/// exactly; hostnames match exactly or as dotted-suffix extensions in
/// either direction (`nas` ↔ `nas.local`).
fn labels_match(announced: &str, query: &str) -> bool {
    let announced = normalize_label(announced);
    let query = normalize_label(query);
    if announced.is_empty() || query.is_empty() {
        return false;
    }
    if announced == query {
        return true;
    }
    if announced.parse::<std::net::IpAddr>().is_ok() || query.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    // Hostname extension in either direction: `nas` answers `nas.local` and
    // `nas.local` answers `nas`.
    announced.starts_with(&format!("{query}.")) || query.starts_with(&format!("{announced}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_outbound() -> SchedulerIngress {
        let (ingress, _bulk_rx, _control_rx) = SchedulerIngress::test_channels(8);
        ingress
    }

    fn node_id(name: &str) -> String {
        name.to_string()
    }

    #[test]
    fn register_inserts_and_len_tracks_entries() {
        let table = RelayConnectionTable::default();
        assert_eq!(table.len(), 0);
        table.register(&node_id("node-a"), dummy_outbound());
        table.register(&node_id("node-b"), dummy_outbound());
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn duplicate_register_reports_previous_entry() {
        let table = RelayConnectionTable::default();
        let first = table.register(&node_id("node-a"), dummy_outbound());
        let second = table.register(&node_id("node-a"), dummy_outbound());
        assert!(first.previous.is_none());
        let previous = second
            .previous
            .expect("replacement returns the stale entry");
        assert_eq!(previous.connection_id, first.connection_id);
        assert_ne!(second.connection_id, first.connection_id);
        assert_eq!(table.len(), 1, "re-register replaces, not duplicates");
    }

    #[test]
    fn touch_ignores_replaced_link() {
        let table = RelayConnectionTable::default();
        let first = table.register(&node_id("node-a"), dummy_outbound());
        let second = table.register(&node_id("node-a"), dummy_outbound());
        // Age the successor past the eviction deadline; a stale link's
        // touch must NOT refresh it (age stays past the deadline).
        std::thread::sleep(Duration::from_millis(20));
        table.touch(&node_id("node-a"), first.connection_id);
        let evicted = table.evict_idle(Duration::from_millis(10));
        assert_eq!(
            evicted.len(),
            1,
            "stale touch must not refresh the successor"
        );
        assert_eq!(evicted[0].0, node_id("node-a"));
        assert_eq!(evicted[0].1.connection_id, second.connection_id);
    }

    #[test]
    fn remove_if_current_guards_against_stale_links() {
        let table = RelayConnectionTable::default();
        let first = table.register(&node_id("node-a"), dummy_outbound());
        let second = table.register(&node_id("node-a"), dummy_outbound());
        assert!(
            !table.remove_if_current(&node_id("node-a"), first.connection_id),
            "the replaced link must not remove its successor"
        );
        assert!(
            table.remove_if_current(&node_id("node-a"), second.connection_id),
            "the current link removes its own entry"
        );
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn evict_idle_only_takes_silent_entries() {
        let table = RelayConnectionTable::default();
        let quiet = table.register(&node_id("quiet"), dummy_outbound());
        let active = table.register(&node_id("active"), dummy_outbound());
        table.touch(&node_id("active"), active.connection_id);
        // Backdate only the quiet entry.
        if let Ok(mut last_seen) = table
            .entries
            .lock()
            .expect("table lock")
            .get(&node_id("quiet"))
            .map(|entry| entry.last_seen.lock())
            .expect("entry present")
        {
            *last_seen = Instant::now() - Duration::from_secs(60);
        }
        let evicted = table.evict_idle(Duration::from_secs(30));
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0, node_id("quiet"));
        assert_eq!(table.len(), 1);
        assert!(
            !table.remove_if_current(&node_id("active"), quiet.connection_id),
            "a different connection id must not remove the current entry"
        );
    }

    #[test]
    fn lookup_resolves_routing_target_with_even_stream_allocator() {
        let table = RelayConnectionTable::default();
        let registered = table.register(&node_id("node-a"), dummy_outbound());
        let target = table
            .lookup(&node_id("node-a"))
            .expect("registered node should resolve");
        assert_eq!(target.connection_id, registered.connection_id);
        assert_eq!(
            target.next_relay_stream.fetch_add(2, Ordering::SeqCst),
            2,
            "relay-initiated stream ids start even"
        );
        assert!(table.lookup(&node_id("missing")).is_none());
    }

    #[test]
    fn retire_notifying_removes_entry_signals_the_link_and_queues_the_code() {
        let table = RelayConnectionTable::default();
        let (ingress, _bulk_rx, mut control_rx) = SchedulerIngress::test_channels(8);
        let registered = table.register(&node_id("node-a"), ingress);
        let retire_rx = registered.retire_rx;

        assert!(table.retire_notifying(&node_id("node-a"), RelayErrorCode::NodeRevoked));
        assert_eq!(table.len(), 0);
        assert!(
            *retire_rx.borrow(),
            "retire must send true on the entry's watch before dropping it"
        );
        let frame = control_rx
            .try_recv()
            .expect("the notice frame is queued before the entry drops");
        match frame {
            Frame::Error {
                stream_id,
                code,
                message,
            } => {
                assert_eq!(stream_id, 0);
                assert_eq!(
                    RelayErrorCode::from_wire(code),
                    Some(RelayErrorCode::NodeRevoked)
                );
                assert_eq!(message, RelayErrorCode::NodeRevoked.message());
            }
            other => panic!("expected an Error notice frame, got {other:?}"),
        }
        assert!(
            !table.retire_notifying(&node_id("node-a"), RelayErrorCode::NodeRevoked),
            "a second retire finds no live entry"
        );
    }

    #[test]
    fn set_labels_records_labels_and_resolve_label_matches_hostname() {
        let table = RelayConnectionTable::default();
        let registered = table.register(&node_id("node-a"), dummy_outbound());
        table.set_labels(
            &node_id("node-a"),
            registered.connection_id,
            vec!["nas".to_string(), "10.0.1.5".to_string()],
        );

        // Exact (case-insensitive, trailing-dot-insensitive) hostname match.
        assert_eq!(
            table.resolve_label("NAS."),
            LabelResolve::Unique {
                node_id: node_id("node-a"),
                labels: vec!["nas".to_string(), "10.0.1.5".to_string()],
            }
        );
        // Dotted suffix extension matches in both directions.
        assert_eq!(
            table.resolve_label("nas.local"),
            LabelResolve::Unique {
                node_id: node_id("node-a"),
                labels: vec!["nas".to_string(), "10.0.1.5".to_string()],
            }
        );
        // IP labels match exactly only.
        assert_eq!(
            table.resolve_label("10.0.1.5"),
            LabelResolve::Unique {
                node_id: node_id("node-a"),
                labels: vec!["nas".to_string(), "10.0.1.5".to_string()],
            }
        );
        assert_eq!(table.resolve_label("10.0.1"), LabelResolve::NoMatch);
        assert_eq!(table.resolve_label("other"), LabelResolve::NoMatch);
    }

    #[test]
    fn resolve_label_reports_ambiguous_matches() {
        let table = RelayConnectionTable::default();
        table.register(&node_id("node-a"), dummy_outbound());
        table.register(&node_id("node-b"), dummy_outbound());
        let a = table.register(&node_id("node-a"), dummy_outbound());
        let b = table.register(&node_id("node-b"), dummy_outbound());
        table.set_labels(&node_id("node-a"), a.connection_id, vec!["nas".to_string()]);
        table.set_labels(&node_id("node-b"), b.connection_id, vec!["nas".to_string()]);

        assert_eq!(table.resolve_label("nas"), LabelResolve::Ambiguous(2));
    }

    #[test]
    fn set_labels_ignored_by_a_replaced_link() {
        let table = RelayConnectionTable::default();
        let first = table.register(&node_id("node-a"), dummy_outbound());
        let second = table.register(&node_id("node-a"), dummy_outbound());
        // A stale link cannot label its successor's entry.
        table.set_labels(
            &node_id("node-a"),
            first.connection_id,
            vec!["stale".to_string()],
        );
        assert_eq!(table.resolve_label("stale"), LabelResolve::NoMatch);
        table.set_labels(
            &node_id("node-a"),
            second.connection_id,
            vec!["fresh".to_string()],
        );
        assert_eq!(
            table.resolve_label("fresh"),
            LabelResolve::Unique {
                node_id: node_id("node-a"),
                labels: vec!["fresh".to_string()],
            }
        );
    }

    #[test]
    fn set_labels_for_an_unknown_node_is_a_noop() {
        let table = RelayConnectionTable::default();
        table.register(&node_id("node-a"), dummy_outbound());
        table.set_labels(&node_id("missing"), 999, vec!["ghost".to_string()]);
        assert_eq!(table.resolve_label("ghost"), LabelResolve::NoMatch);
    }

    #[test]
    fn snapshot_lists_registered_nodes_with_idle_time() {
        let table = RelayConnectionTable::default();
        table.register(&node_id("node-a"), dummy_outbound());
        table.register(&node_id("node-b"), dummy_outbound());
        std::thread::sleep(Duration::from_millis(5));
        let mut snapshot = table.snapshot();
        snapshot.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].0, node_id("node-a"));
        assert_eq!(snapshot[1].0, node_id("node-b"));
        assert!(
            snapshot.iter().all(|(_, _, idle_ms)| *idle_ms < 500),
            "fresh entries should report small idle times"
        );
    }
}
