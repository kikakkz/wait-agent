//! Presence hub: who watches whom on the relay (docs/relay-design.md node
//! 生命周期 presence channel). Registered links publish online/offline
//! transitions; nodes subscribe with `Frame::Watch` and receive an immediate
//! `Frame::Presence` replay plus every later transition for the target.
//!
//! The hub is relay-side only. Watch interests are per link connection and
//! die with it: the link epilogue purges the connection uniformly (no
//! per-cause bookkeeping for unregister, eviction, replacement, or loss).
//!
//! Locking: the watchers map mutex is a leaf, held only for map operations.
//! Presence frames are sent (via `try_send` — a full watcher queue drops the
//! transition, the lifecycle doctrine: presence is best-effort) only after
//! the lock is released.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::infra::relay_mux::frame::Frame;

/// Target node id → the connections watching it, each with its link's
/// outbound queue. Node ids are stored lowercase (fingerprints compare
/// case-insensitively across the relay protocol).
pub struct PresenceHub {
    watchers: Mutex<HashMap<String, HashMap<u64, mpsc::Sender<Frame>>>>,
}

impl PresenceHub {
    pub fn new() -> Self {
        Self {
            watchers: Mutex::new(HashMap::new()),
        }
    }

    /// Records `connection_id` as watching `target` and immediately replays
    /// the target's current presence on the link (a `Watch` always answers
    /// with one `Presence`, online or offline, so late watchers converge).
    pub fn watch(
        &self,
        connection_id: u64,
        target: &str,
        outbound: mpsc::Sender<Frame>,
        currently_online: bool,
    ) {
        let target = target.to_lowercase();
        {
            let Ok(mut watchers) = self.watchers.lock() else {
                return;
            };
            watchers
                .entry(target.clone())
                .or_default()
                .insert(connection_id, outbound.clone());
        }
        // Sent after unlocking; a full queue drops the replay (best-effort).
        let _ = outbound.try_send(Frame::Presence {
            node_id: target,
            online: currently_online,
        });
    }

    /// Purges every watch interest of `connection_id` — the link epilogue
    /// calls this for every exit cause (unregister, eviction, replacement,
    /// loss) so a dead link's queue is never touched again.
    pub fn unwatch_connection(&self, connection_id: u64) {
        let Ok(mut watchers) = self.watchers.lock() else {
            return;
        };
        watchers.retain(|_, interests| {
            interests.remove(&connection_id);
            !interests.is_empty()
        });
    }

    /// Fans a presence transition out to every watcher of `node_id`.
    /// Transitions are best-effort: a full watcher queue drops the frame.
    pub fn publish(&self, node_id: &str, online: bool) {
        let senders: Vec<mpsc::Sender<Frame>> = {
            let Ok(watchers) = self.watchers.lock() else {
                return;
            };
            watchers
                .get(&node_id.to_lowercase())
                .map(|interests| interests.values().cloned().collect())
                .unwrap_or_default()
        };
        for sender in senders {
            let _ = sender.try_send(Frame::Presence {
                node_id: node_id.to_lowercase(),
                online,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> (mpsc::Sender<Frame>, mpsc::Receiver<Frame>) {
        mpsc::channel(8)
    }

    #[test]
    fn watch_replays_current_presence_online_and_offline() {
        let hub = PresenceHub::new();
        let (tx, mut rx) = channel();
        hub.watch(1, "Node-A", tx, true);
        assert_eq!(
            rx.try_recv().expect("replay should arrive"),
            Frame::Presence {
                node_id: "node-a".to_string(),
                online: true
            }
        );
        assert!(rx.try_recv().is_err(), "exactly one replay frame");

        let (tx, mut rx) = channel();
        hub.watch(2, "node-a", tx, false);
        assert_eq!(
            rx.try_recv().expect("offline replay should arrive"),
            Frame::Presence {
                node_id: "node-a".to_string(),
                online: false
            }
        );
    }

    #[test]
    fn publish_fans_out_to_every_watcher_case_insensitively() {
        let hub = PresenceHub::new();
        let (tx_one, mut rx_one) = channel();
        let (tx_two, mut rx_two) = channel();
        hub.watch(1, "node-a", tx_one, true);
        hub.watch(2, "NODE-A", tx_two, false);
        let _ = rx_one.try_recv();
        let _ = rx_two.try_recv();

        hub.publish("Node-A", false);
        for rx in [&mut rx_one, &mut rx_two] {
            assert_eq!(
                rx.try_recv().expect("fan-out should reach every watcher"),
                Frame::Presence {
                    node_id: "node-a".to_string(),
                    online: false
                }
            );
        }
    }

    #[test]
    fn unwatch_connection_purges_all_interests_of_the_link() {
        let hub = PresenceHub::new();
        let (tx, mut rx) = channel();
        let (other_tx, mut other_rx) = channel();
        hub.watch(1, "node-a", tx.clone(), true);
        hub.watch(1, "node-b", tx, true);
        hub.watch(2, "node-a", other_tx, true);
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = other_rx.try_recv();

        hub.unwatch_connection(1);
        hub.publish("node-a", false);
        hub.publish("node-b", false);
        assert!(
            rx.try_recv().is_err(),
            "the purged link must receive nothing"
        );
        assert!(
            other_rx.try_recv().is_ok(),
            "other watchers keep their subscription"
        );
    }

    #[test]
    fn publish_without_watchers_is_a_no_op() {
        let hub = PresenceHub::new();
        hub.publish("nobody", true);
        hub.publish("nobody", false);
    }

    #[test]
    fn rewatched_connection_replaces_its_sender() {
        let hub = PresenceHub::new();
        let (tx_old, mut rx_old) = channel();
        let (tx_new, mut rx_new) = channel();
        hub.watch(1, "node-a", tx_old, true);
        hub.watch(1, "node-a", tx_new, true);
        let _ = rx_old.try_recv();
        let _ = rx_new.try_recv();

        hub.publish("node-a", false);
        assert!(
            rx_old.try_recv().is_err(),
            "the replaced sender must no longer receive transitions"
        );
        assert!(rx_new.try_recv().is_ok());
    }
}
