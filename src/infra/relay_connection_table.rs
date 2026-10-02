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

use tokio::sync::{mpsc, watch};

use crate::infra::relay_mux::frame::Frame;

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
    pub(crate) outbound: mpsc::Sender<Frame>,
    /// Allocator for relay-initiated (even) stream ids on this link.
    pub(crate) next_relay_stream: Arc<AtomicU32>,
}

/// A resolved routing target: everything needed to open a routed stream
/// toward a registered node.
pub(crate) struct RoutingTarget {
    pub(crate) connection_id: u64,
    pub(crate) outbound: mpsc::Sender<Frame>,
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
    pub(crate) fn register(&self, node_id: &str, outbound: mpsc::Sender<Frame>) -> RegisteredEntry {
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::SeqCst);
        let (retire_tx, retire_rx) = watch::channel(false);
        let entry = ConnectionEntry {
            connection_id,
            retire_tx: retire_tx.clone(),
            last_seen: Arc::new(Mutex::new(Instant::now())),
            outbound,
            next_relay_stream: Arc::new(AtomicU32::new(2)),
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

    /// Removes the entry for `node_id` and signals its link task to retire —
    /// the `relay remove` revocation path (docs/relay-design.md node 生命周
    /// 期). Mirrors the replace-retire logic in `register`. Returns whether a
    /// live entry was removed.
    pub(crate) fn retire(&self, node_id: &str) -> bool {
        let entry = self
            .entries
            .lock()
            .expect("relay connection table lock poisoned")
            .remove(node_id);
        match entry {
            Some(entry) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_outbound() -> mpsc::Sender<Frame> {
        let (tx, _rx) = mpsc::channel(8);
        tx
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
    fn retire_removes_entry_and_signals_the_link() {
        let table = RelayConnectionTable::default();
        let registered = table.register(&node_id("node-a"), dummy_outbound());
        let retire_rx = registered.retire_rx;

        assert!(table.retire(&node_id("node-a")));
        assert_eq!(table.len(), 0);
        assert!(
            *retire_rx.borrow(),
            "retire must send true on the entry's watch before dropping it"
        );
        assert!(
            !table.retire(&node_id("node-a")),
            "a second retire finds no live entry"
        );
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
