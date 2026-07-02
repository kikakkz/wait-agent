use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TRAILING_REFRESH_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_SETTLED_REFRESH_DELAY: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputQuietRefreshKind {
    Trailing,
    Settled,
}

impl OutputQuietRefreshKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Trailing => "trailing",
            Self::Settled => "settled",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OutputQuietRefreshConfig {
    trailing_delay: Duration,
    settled_delay: Duration,
}

impl OutputQuietRefreshConfig {
    #[cfg(test)]
    pub(crate) fn new(trailing_delay: Duration, settled_delay: Duration) -> Self {
        Self {
            trailing_delay,
            settled_delay,
        }
    }
}

impl Default for OutputQuietRefreshConfig {
    fn default() -> Self {
        Self {
            trailing_delay: DEFAULT_TRAILING_REFRESH_DELAY,
            settled_delay: DEFAULT_SETTLED_REFRESH_DELAY,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ScheduledRefresh {
    generation: u64,
    due_at: Instant,
    kind: OutputQuietRefreshKind,
}

struct SchedulerState {
    generation: u64,
    trailing: Option<ScheduledRefresh>,
    settled: Option<ScheduledRefresh>,
    shutdown: bool,
}

struct SchedulerShared {
    state: Mutex<SchedulerState>,
    changed: Condvar,
}

pub(crate) struct OutputQuietRefreshScheduler {
    shared: Arc<SchedulerShared>,
    config: OutputQuietRefreshConfig,
    worker: Option<thread::JoinHandle<()>>,
}

impl OutputQuietRefreshScheduler {
    pub(crate) fn new<F>(config: OutputQuietRefreshConfig, refresh: F) -> Self
    where
        F: Fn(OutputQuietRefreshKind) + Send + 'static,
    {
        let shared = Arc::new(SchedulerShared {
            state: Mutex::new(SchedulerState {
                generation: 0,
                trailing: None,
                settled: None,
                shutdown: false,
            }),
            changed: Condvar::new(),
        });
        let worker_shared = shared.clone();
        let worker = thread::spawn(move || run_scheduler_worker(worker_shared, refresh));
        Self {
            shared,
            config,
            worker: Some(worker),
        }
    }

    pub(crate) fn on_output(&self) {
        let now = Instant::now();
        let mut state = self
            .shared
            .state
            .lock()
            .expect("output quiet scheduler mutex should not be poisoned");
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.trailing = Some(ScheduledRefresh {
            generation,
            due_at: now + self.config.trailing_delay,
            kind: OutputQuietRefreshKind::Trailing,
        });
        state.settled = Some(ScheduledRefresh {
            generation,
            due_at: now + self.config.settled_delay,
            kind: OutputQuietRefreshKind::Settled,
        });
        self.shared.changed.notify_one();
    }
}

impl Drop for OutputQuietRefreshScheduler {
    fn drop(&mut self) {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("output quiet scheduler mutex should not be poisoned");
            state.shutdown = true;
            state.trailing = None;
            state.settled = None;
        }
        self.shared.changed.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_scheduler_worker<F>(shared: Arc<SchedulerShared>, refresh: F)
where
    F: Fn(OutputQuietRefreshKind),
{
    let mut state = shared
        .state
        .lock()
        .expect("output quiet scheduler mutex should not be poisoned");
    loop {
        if state.shutdown {
            return;
        }
        let Some(next) = next_refresh(&state) else {
            state = shared
                .changed
                .wait(state)
                .expect("output quiet scheduler mutex should not be poisoned");
            continue;
        };
        let now = Instant::now();
        if next.due_at > now {
            let wait = next.due_at.saturating_duration_since(now);
            let (next_state, _) = shared
                .changed
                .wait_timeout(state, wait)
                .expect("output quiet scheduler mutex should not be poisoned");
            state = next_state;
            continue;
        }
        clear_refresh(&mut state, next);
        drop(state);
        refresh(next.kind);
        state = shared
            .state
            .lock()
            .expect("output quiet scheduler mutex should not be poisoned");
    }
}

fn next_refresh(state: &SchedulerState) -> Option<ScheduledRefresh> {
    [state.trailing, state.settled]
        .into_iter()
        .flatten()
        .min_by_key(|refresh| refresh.due_at)
}

fn clear_refresh(state: &mut SchedulerState, refresh: ScheduledRefresh) {
    let slot = match refresh.kind {
        OutputQuietRefreshKind::Trailing => &mut state.trailing,
        OutputQuietRefreshKind::Settled => &mut state.settled,
    };
    if slot
        .map(|scheduled| scheduled.generation == refresh.generation)
        .unwrap_or(false)
    {
        *slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputQuietRefreshConfig, OutputQuietRefreshKind, OutputQuietRefreshScheduler};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn output_quiet_refresh_scheduler_keeps_only_latest_output_burst() {
        let (tx, rx) = mpsc::channel();
        let scheduler = OutputQuietRefreshScheduler::new(
            OutputQuietRefreshConfig::new(Duration::from_millis(30), Duration::from_millis(70)),
            move |kind| {
                let _ = tx.send(kind);
            },
        );

        scheduler.on_output();
        thread::sleep(Duration::from_millis(10));
        scheduler.on_output();

        assert_eq!(
            rx.recv_timeout(Duration::from_millis(150)),
            Ok(OutputQuietRefreshKind::Trailing)
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(150)),
            Ok(OutputQuietRefreshKind::Settled)
        );
        assert!(rx.recv_timeout(Duration::from_millis(80)).is_err());
    }
}
