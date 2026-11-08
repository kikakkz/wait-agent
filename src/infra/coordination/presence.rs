//! Online-presence tracking for the coordination service (issue #253).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use iroh::{EndpointAddr, TransportAddr};

/// Default presence timeout: three missed heartbeats at the default 15 s interval.
pub const DEFAULT_PRESENCE_TIMEOUT_SECS: u64 = 45;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Last known state of one node.
pub struct PresenceEntry {
    pub node_id: String,
    pub addr: EndpointAddr,
    pub last_seen: u64,
}

#[derive(Debug, Default)]
/// In-memory presence table; presence is ephemeral and rebuilds from heartbeats
/// after a server restart.
pub struct PresenceStore {
    nodes: Mutex<BTreeMap<String, PresenceEntry>>,
}

impl PresenceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a heartbeat, merging the connection-observed direct address.
    pub fn record(
        &self,
        node_id: String,
        mut addr: EndpointAddr,
        observed: Option<SocketAddr>,
        now: u64,
    ) {
        if let Some(observed) = observed {
            addr.addrs.insert(TransportAddr::Ip(observed));
        }
        let mut nodes = self.nodes.lock().expect("poisoned");
        nodes.insert(
            node_id.clone(),
            PresenceEntry {
                node_id,
                addr,
                last_seen: now,
            },
        );
    }

    /// The last known entry, online or not.
    pub fn get(&self, node_id: &str) -> Option<PresenceEntry> {
        self.nodes.lock().expect("poisoned").get(node_id).cloned()
    }

    /// Drops a node (on revoke).
    pub fn remove(&self, node_id: &str) {
        self.nodes.lock().expect("poisoned").remove(node_id);
    }

    /// Entries whose last heartbeat is within `timeout` of `now`, by node id.
    pub fn online(&self, now: u64, timeout: Duration) -> Vec<PresenceEntry> {
        self.nodes
            .lock()
            .expect("poisoned")
            .values()
            .filter(|entry| entry.last_seen.saturating_add(timeout.as_secs()) > now)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::mesh_probe::parse_node_id;

    fn addr(node_id: &str) -> EndpointAddr {
        EndpointAddr::new(parse_node_id(node_id).unwrap())
    }

    #[test]
    fn record_merges_the_observed_connection_address() {
        let store = PresenceStore::new();
        let node_id = iroh::SecretKey::generate().public().to_string();
        store.record(
            node_id.clone(),
            addr(&node_id),
            Some("192.168.1.10:7000".parse().unwrap()),
            1000,
        );
        let entry = store.get(&node_id).unwrap();
        assert!(entry.addr.addrs.iter().any(
            |a| matches!(a, TransportAddr::Ip(ip) if *ip == "192.168.1.10:7000".parse().unwrap())
        ));
    }

    #[test]
    fn online_filters_by_timeout_and_sorts_by_node_id() {
        let store = PresenceStore::new();
        let node_b = iroh::SecretKey::generate().public().to_string();
        let node_a = iroh::SecretKey::generate().public().to_string();
        store.record(node_b.clone(), addr(&node_b), None, 1000);
        store.record(node_a.clone(), addr(&node_a), None, 1010);
        store.record(
            "stale".repeat(8),
            addr(&iroh::SecretKey::generate().public().to_string()),
            None,
            100,
        );

        let online = store.online(1010, Duration::from_secs(45));
        let ids: Vec<String> = online.iter().map(|entry| entry.node_id.clone()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        assert_eq!(online.len(), 2);
        assert!(!online
            .iter()
            .any(|entry| entry.node_id.starts_with("stale")));
        assert_eq!(store.online(2000, Duration::from_secs(45)).len(), 0);
    }

    #[test]
    fn get_returns_the_last_known_entry_even_when_expired() {
        let store = PresenceStore::new();
        let node_id = iroh::SecretKey::generate().public().to_string();
        store.record(node_id.clone(), addr(&node_id), None, 100);
        let entry = store.get(&node_id).unwrap();
        assert_eq!(entry.last_seen, 100);
        assert!(store.online(1000, Duration::from_secs(45)).is_empty());
    }
}
