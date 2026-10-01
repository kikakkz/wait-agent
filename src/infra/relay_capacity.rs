//! Relay capacity model and admission control, per docs/relay-design.md
//! 容量评估与准入控制. The connection table and routing table are the live
//! usage; throughput is metered on the routing path; admission decisions
//! (`接入决策 = capacity − usage`) happen at register and open_stream. The
//! phase-2 cluster scheduler consumes exactly this shape.
//!
//! Defaults are conservative and provisional (压测标定后再修订); relay.toml
//! wiring lands with the config-surface slice.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Conservative provisional defaults, revised after calibration.
pub const DEFAULT_MAX_NODES: usize = 1024;
pub const DEFAULT_MAX_STREAMS: usize = 4096;
pub const DEFAULT_MAX_THROUGHPUT_BYTES_PER_SEC: u64 = 64 * 1024 * 1024;

/// Capacity knobs; every field is overridable (relay.toml with #35).
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct RelayCapacityConfig {
    /// Maximum concurrent registered node links.
    pub max_nodes: usize,
    /// Maximum concurrent routed streams.
    pub max_streams: usize,
    /// Forwarded-bytes/s protection threshold; new `open_stream` requests
    /// are refused while the rate is over it (conservative policy).
    pub max_throughput_bytes_per_sec: u64,
}

impl Default for RelayCapacityConfig {
    fn default() -> Self {
        Self {
            max_nodes: DEFAULT_MAX_NODES,
            max_streams: DEFAULT_MAX_STREAMS,
            max_throughput_bytes_per_sec: DEFAULT_MAX_THROUGHPUT_BYTES_PER_SEC,
        }
    }
}

/// Usage snapshot for the admin surface and admission checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RelayUsage {
    pub registered_nodes: usize,
    pub active_streams: usize,
    pub forwarded_bytes_per_sec: u64,
}

/// Rough forwarded-bytes rate meter: an atomic counter inside a 1-second
/// window. Deliberately simple — the threshold is a conservative protection
/// valve, not a billing meter, and the design leaves window tightening to
/// post-calibration revision.
pub struct UsageMeter {
    bytes: AtomicU64,
    window_started: std::sync::Mutex<Instant>,
}

impl Default for UsageMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageMeter {
    pub fn new() -> Self {
        Self {
            bytes: AtomicU64::new(0),
            window_started: std::sync::Mutex::new(Instant::now()),
        }
    }

    /// Records forwarded payload bytes on the routing path.
    pub fn record(&self, bytes: usize) {
        self.refresh_window();
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Current window's byte count (the meter's rate granularity is 1s).
    pub fn bytes_this_window(&self) -> u64 {
        self.refresh_window();
        self.bytes.load(Ordering::Relaxed)
    }

    fn refresh_window(&self) {
        let Ok(mut started) = self.window_started.lock() else {
            return;
        };
        if started.elapsed() >= Duration::from_secs(1) {
            *started = Instant::now();
            self.bytes.store(0, Ordering::Relaxed);
        }
    }
}

/// Shared meter handed to every link task.
pub type SharedUsageMeter = Arc<UsageMeter>;

/// Admission decision for one `open_stream` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAdmission {
    Allow,
    StreamsFull,
    ThroughputExceeded,
}

impl RelayCapacityConfig {
    /// Decides a new routed stream against current usage.
    pub(crate) fn admit_open(&self, active_streams: usize, meter: &UsageMeter) -> OpenAdmission {
        if active_streams >= self.max_streams {
            return OpenAdmission::StreamsFull;
        }
        if meter.bytes_this_window() >= self.max_throughput_bytes_per_sec {
            return OpenAdmission::ThroughputExceeded;
        }
        OpenAdmission::Allow
    }

    /// Decides a registration for a NEW node (replacements never grow the
    /// table and are always allowed — reconnect re-admits).
    pub(crate) fn admit_register(&self, registered_nodes: usize) -> bool {
        registered_nodes < self.max_nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative_provisional_values() {
        let config = RelayCapacityConfig::default();
        assert_eq!(config.max_nodes, 1024);
        assert_eq!(config.max_streams, 4096);
        assert_eq!(config.max_throughput_bytes_per_sec, 64 * 1024 * 1024);
    }

    #[test]
    fn meter_aggregates_within_a_window() {
        let meter = UsageMeter::new();
        assert_eq!(meter.bytes_this_window(), 0);
        meter.record(100);
        meter.record(250);
        assert_eq!(meter.bytes_this_window(), 350);
    }

    #[test]
    fn meter_resets_after_one_second() {
        let meter = UsageMeter::new();
        meter.record(100);
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(meter.bytes_this_window(), 0, "window should roll over");
    }

    #[test]
    fn register_admission_allows_replacement_at_cap() {
        let config = RelayCapacityConfig {
            max_nodes: 2,
            ..RelayCapacityConfig::default()
        };
        assert!(config.admit_register(0));
        assert!(config.admit_register(1));
        assert!(!config.admit_register(2), "at cap: new node refused");
        // Replacement does not grow the table — the caller checks len only
        // for NEW nodes, so re-registration stays legal.
    }

    #[test]
    fn open_admission_checks_streams_then_throughput() {
        let config = RelayCapacityConfig {
            max_streams: 1,
            max_throughput_bytes_per_sec: 100,
            ..RelayCapacityConfig::default()
        };
        let meter = UsageMeter::new();
        assert_eq!(config.admit_open(0, &meter), OpenAdmission::Allow);
        assert_eq!(
            config.admit_open(1, &meter),
            OpenAdmission::StreamsFull,
            "at stream cap"
        );
        meter.record(100);
        assert_eq!(
            config.admit_open(0, &meter),
            OpenAdmission::ThroughputExceeded,
            "over the throughput threshold"
        );
    }
}
