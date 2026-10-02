//! Per-link egress scheduler: stream isolation (per-stream byte bounds) and
//! fair scheduling (deficit round robin) between the routed streams of one
//! relay link (issue #33).
//!
//! Today every frame bound for a link — routed stream data from any number of
//! sources plus control frames — lands in one FIFO (`mpsc::Sender<Frame>`),
//! and a writer task pumps it in arrival order. One stream with a slow or
//! absent receiver fills that FIFO and stalls every other stream to the same
//! target (head-of-line blocking), with no bound on how much memory a single
//! stream can occupy.
//!
//! `LinkScheduler` keeps the producers' ingress type unchanged — it IS the
//! `mpsc::Sender<Frame>` cloned into connection entries and routing legs —
//! and interposes two tasks between that FIFO and the socket:
//!
//! - an ingress consumer that classifies frames: `Data`/`Window` go to a
//!   per-stream bucket keyed by the frame's stream id (already the
//!   target-side id from the route rewrite); everything else (heartbeat,
//!   register, errors, `CloseStream`, presence, ...) goes to a priority
//!   queue;
//! - a writer task that drains the priority queue fully first, then serves
//!   live buckets with one deficit-round-robin pass per loop iteration.
//!
//! Backpressure is end-to-end: a bucket at its byte cap, or a link at its
//! total budget, parks the ingress consumer (await on a `Notify`); the
//! ingress FIFO (256 frames) fills next; and the source link's
//! `forward_routed` await parks at the same point it does today — the relay
//! adds its own bounds between end-to-end stream credit, it never replaces
//! it.
//!
//! # Fairness guarantee
//!
//! While any bucket holds backlog, every live bucket transmits at least one
//! frame per RR visit (deficit flooring), and per round up to
//! `round_budget_bytes` plus one frame: no single stream can emit more than
//! `round_budget_bytes` ahead of another stream that holds backlog. Priority
//! frames always precede bulk data: the writer drains the priority queue
//! fully before every RR pass.
//!
//! # Lock order
//!
//! One mutex, `Shared`, and it is a leaf: it is never held across an await,
//! across a channel send that can park, or across any I/O. The ingress
//! consumer takes it to classify and enqueue (waiting for budget happens
//! outside it, via `space_notify`); the writer takes it to take batches out
//! (priority drain, RR selection, accounting) and writes to the socket only
//! after releasing it. Wake-ups (`Notify`) are issued after the lock is
//! dropped. No other lock is ever acquired while holding it, so there is no
//! lock ordering beyond this leaf.
//!
//! # Idle behavior and shutdown
//!
//! When no queue holds anything and the ingress is open, the writer parks on
//! a `Notify` signaled by the consumer — no busy poll. Buckets emptied and
//! not re-fed for `IDLE_REAP_PASSES` rounds are reaped (the stream ended).
//! When the ingress channel closes (every producer dropped, i.e. link
//! teardown), the consumer sets `ingress_open = false`; the writer finishes
//! draining the priority queue and the buckets, then exits — the flush
//! semantics the raw FIFO writer had.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::relay_link::LINK_OUTBOUND_QUEUE;
use crate::infra::relay_mux::frame::{write_frame, Frame};
use crate::infra::relay_mux::MAX_FRAME_PAYLOAD;

/// A bucket emptied for this many consecutive RR rounds is reaped: the
/// stream it served ended (both legs closed) and nobody re-fed it.
const IDLE_REAP_PASSES: u32 = 8;

/// Scheduling and bounds for one link's egress.
pub(crate) struct SchedulerConfig {
    /// Per-stream queued-byte bound. At least one max-size frame always fits
    /// an empty bucket (floored to [`MAX_FRAME_PAYLOAD`] at spawn).
    pub(crate) bucket_capacity_bytes: usize,
    /// Total queued bytes across all buckets of this link.
    pub(crate) link_budget_bytes: usize,
    /// Deficit added to every live bucket per RR round. Carried across
    /// rounds as credit, so an idle stream's burst is bounded by one round,
    /// not by how long it was silent.
    pub(crate) round_budget_bytes: usize,
    /// Capacity of the shared ingress FIFO — the producers' backpressure
    /// horizon (`LINK_OUTBOUND_QUEUE` at the call site).
    pub(crate) ingress_capacity: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            bucket_capacity_bytes: 1024 * 1024,
            link_budget_bytes: 64 * 1024 * 1024,
            round_budget_bytes: 64 * 1024,
            ingress_capacity: LINK_OUTBOUND_QUEUE,
        }
    }
}

struct Bucket {
    queue: VecDeque<Frame>,
    queued_bytes: usize,
    /// Deficit-round-robin credit, carried across rounds.
    deficit: i64,
    /// Consecutive RR rounds seen empty; reaped past [`IDLE_REAP_PASSES`].
    idle_passes: u32,
}

impl Bucket {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            queued_bytes: 0,
            deficit: 0,
            idle_passes: 0,
        }
    }
}

struct Shared {
    priority: VecDeque<Frame>,
    buckets: HashMap<u32, Bucket>,
    total_bucket_bytes: usize,
    /// Cumulative count of classified frames (never decremented); the test
    /// hooks use it to wait for classification without racing the writer.
    classified_frames: u64,
    /// False once the ingress channel closed (all producers dropped).
    ingress_open: bool,
    /// Set when the writer exits (socket error): the consumer drops frames.
    writer_done: bool,
}

/// Scheduler state: the data mutex plus the two wake signals. The notifies
/// live OUTSIDE the mutex so a signal is never issued while holding it.
struct SchedulerState {
    shared: Mutex<Shared>,
    /// Consumer → writer: work is available.
    work_available: Notify,
    /// Writer → consumer: budget freed, or the writer is gone.
    space_freed: Notify,
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|error| error.into_inner())
}

/// Join handle for the scheduler's two tasks. `run_link` awaits it where it
/// previously awaited the raw writer task.
pub(crate) struct SchedulerHandle {
    // Read by the test-only introspection accessor below.
    #[allow(dead_code)]
    state: Arc<SchedulerState>,
    consumer: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl SchedulerHandle {
    /// Waits for both tasks. After the ingress sender is dropped this
    /// returns once the queues are drained (flush semantics on teardown).
    pub(crate) async fn shutdown(self) {
        let _ = self.consumer.await;
        let _ = self.writer.await;
    }

    /// Test-only introspection: cumulative classified frame count.
    #[cfg(test)]
    async fn debug_classified_frames(&self) -> u64 {
        lock(&self.state.shared).classified_frames
    }
}

pub(crate) struct LinkScheduler;

impl LinkScheduler {
    /// Spawns the ingress consumer and the writer task over `writer`.
    /// Returns the ingress sender (clone it into connection entries and
    /// routing legs exactly like the raw channel before) and the handle.
    pub(crate) fn spawn<W: AsyncWrite + Unpin + Send + 'static>(
        config: SchedulerConfig,
        writer: W,
    ) -> (mpsc::Sender<Frame>, SchedulerHandle) {
        // Floors keep invariants: an empty bucket always accepts one
        // max-size frame, and a round always sends at least one frame.
        let bucket_capacity = config.bucket_capacity_bytes.max(MAX_FRAME_PAYLOAD as usize);
        let link_budget = config.link_budget_bytes.max(bucket_capacity);
        let round_budget = config.round_budget_bytes.max(1);
        let (ingress_tx, ingress_rx) = mpsc::channel(config.ingress_capacity.max(1));
        let state = Arc::new(SchedulerState {
            shared: Mutex::new(Shared {
                priority: VecDeque::new(),
                buckets: HashMap::new(),
                total_bucket_bytes: 0,
                classified_frames: 0,
                ingress_open: true,
                writer_done: false,
            }),
            work_available: Notify::new(),
            space_freed: Notify::new(),
        });
        let consumer = tokio::spawn(ingress_consumer_loop(
            bucket_capacity,
            link_budget,
            ingress_rx,
            state.clone(),
        ));
        let writer = tokio::spawn(writer_loop(round_budget, writer, state.clone()));
        (
            ingress_tx,
            SchedulerHandle {
                state,
                consumer,
                writer,
            },
        )
    }
}

async fn ingress_consumer_loop(
    bucket_capacity: usize,
    link_budget: usize,
    mut ingress: mpsc::Receiver<Frame>,
    state: Arc<SchedulerState>,
) {
    while let Some(frame) = ingress.recv().await {
        match frame {
            Frame::Data { .. } | Frame::Window { .. } => {
                let stream_id = frame.stream_id();
                let len = frame.payload_len();
                loop {
                    enum Outcome {
                        Fits,
                        Park,
                        Drop,
                    }
                    // Capacity check and enqueue are separate lock sections;
                    // this is the only task that enqueues, so no race.
                    let outcome = {
                        let mut guard = lock(&state.shared);
                        if guard.writer_done {
                            // The writer is gone (socket error): the link is
                            // dying, drop the frame.
                            Outcome::Drop
                        } else {
                            let total = guard.total_bucket_bytes;
                            let bucket = guard.buckets.entry(stream_id).or_insert_with(Bucket::new);
                            if bucket.queued_bytes + len <= bucket_capacity
                                && total + len <= link_budget
                            {
                                Outcome::Fits
                            } else {
                                Outcome::Park
                            }
                        }
                    };
                    match outcome {
                        Outcome::Fits => {
                            {
                                let mut guard = lock(&state.shared);
                                if let Some(bucket) = guard.buckets.get_mut(&stream_id) {
                                    bucket.queue.push_back(frame);
                                    bucket.queued_bytes += len;
                                    bucket.idle_passes = 0;
                                    guard.total_bucket_bytes += len;
                                    guard.classified_frames += 1;
                                }
                            }
                            state.work_available.notify_one();
                            break;
                        }
                        Outcome::Park => {
                            // Bucket at cap or link budget exhausted: park
                            // until the writer frees budget or dies. The FIFO
                            // upstream fills next and `forward_routed` parks
                            // behind it — the end-to-end backpressure point.
                            state.space_freed.notified().await;
                        }
                        Outcome::Drop => break,
                    }
                }
            }
            priority_frame => {
                {
                    let mut guard = lock(&state.shared);
                    guard.priority.push_back(priority_frame);
                    guard.classified_frames += 1;
                }
                state.work_available.notify_one();
            }
        }
    }
    {
        let mut guard = lock(&state.shared);
        guard.ingress_open = false;
    }
    state.work_available.notify_one();
}

async fn writer_loop<W: AsyncWrite + Unpin>(
    round_budget: usize,
    mut writer: W,
    state: Arc<SchedulerState>,
) {
    'write: loop {
        // Priority first: control frames always precede bulk data.
        let priority_batch: Vec<Frame> = {
            let mut guard = lock(&state.shared);
            guard.priority.drain(..).collect()
        };
        for frame in &priority_batch {
            if let Err(error) = write_frame(&mut writer, frame).await {
                ERROR_LOG.log_error(format!("[relay] link writer failed: {error}"));
                break 'write;
            }
        }

        // One deficit-round-robin pass over every bucket (live and idle):
        // batches are taken under the lock, written after it is released.
        let mut batch: Vec<Frame> = Vec::new();
        let mut freed_budget = false;
        let more_work = {
            let mut guard = lock(&state.shared);
            let mut total_freed = 0usize;
            for bucket in guard.buckets.values_mut() {
                if bucket.queue.is_empty() {
                    bucket.idle_passes += 1;
                    continue;
                }
                bucket.idle_passes = 0;
                bucket.deficit += round_budget as i64;
                let mut served = false;
                loop {
                    let Some(len) = bucket.queue.front().map(|f| f.payload_len()) else {
                        break;
                    };
                    if len as i64 > bucket.deficit {
                        break;
                    }
                    let Some(frame) = bucket.queue.pop_front() else {
                        break;
                    };
                    bucket.deficit -= len as i64;
                    bucket.queued_bytes -= len;
                    total_freed += len;
                    batch.push(frame);
                    served = true;
                    freed_budget = true;
                }
                if !served {
                    // Deficit flooring: a live bucket always transmits at
                    // least one frame per visit, so a frame larger than the
                    // round budget still progresses (deficit goes negative
                    // and is carried).
                    if let Some(frame) = bucket.queue.pop_front() {
                        let len = frame.payload_len();
                        bucket.deficit -= len as i64;
                        bucket.queued_bytes -= len;
                        total_freed += len;
                        batch.push(frame);
                        freed_budget = true;
                    }
                }
            }
            guard.total_bucket_bytes -= total_freed;
            guard.buckets.retain(|_, bucket| {
                !(bucket.queue.is_empty() && bucket.idle_passes >= IDLE_REAP_PASSES)
            });
            let pending = !guard.priority.is_empty()
                || guard
                    .buckets
                    .values()
                    .any(|bucket| !bucket.queue.is_empty());
            pending || guard.ingress_open
        };
        // Signal freed budget BEFORE writing the batch: the socket write can
        // block indefinitely (stalled target), and the consumer must not wait
        // on it to make queue room. The stored permit covers the consumer's
        // next `notified().await` even if it has not parked yet.
        if freed_budget {
            state.space_freed.notify_one();
        }
        for frame in &batch {
            if let Err(error) = write_frame(&mut writer, frame).await {
                ERROR_LOG.log_error(format!("[relay] link writer failed: {error}"));
                break 'write;
            }
        }
        if !more_work {
            // Everything drained and the ingress closed: flush is complete.
            if batch.is_empty() {
                break 'write;
            }
        } else if batch.is_empty() && priority_batch.is_empty() {
            // Nothing served and more may come: park until the consumer
            // signals (never a busy poll).
            state.work_available.notified().await;
        }
    }
    // Unblock a consumer parked on budget so it can observe `writer_done`.
    {
        let mut guard = lock(&state.shared);
        guard.writer_done = true;
    }
    state.space_freed.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::relay_mux::frame::read_frame;
    use std::time::Duration;
    use tokio::io::DuplexStream;
    use tokio::time::timeout;

    const NO_DEADLOCK: Duration = Duration::from_secs(10);

    fn tiny_config() -> SchedulerConfig {
        SchedulerConfig {
            bucket_capacity_bytes: 1024,
            link_budget_bytes: 4096,
            round_budget_bytes: 8,
            ingress_capacity: 4,
        }
    }

    fn spawn_test(config: SchedulerConfig) -> (mpsc::Sender<Frame>, DuplexStream, SchedulerHandle) {
        let (read_half, write_half) = tokio::io::duplex(1024);
        let (tx, handle) = LinkScheduler::spawn(config, write_half);
        (tx, read_half, handle)
    }

    async fn read_one(reader: &mut DuplexStream) -> Frame {
        timeout(NO_DEADLOCK, read_frame(reader))
            .await
            .expect("frame within deadline")
            .expect("frame decodes")
    }

    #[tokio::test]
    async fn priority_frames_overtake_queued_data() {
        let (tx, mut rx, handle) = spawn_test(tiny_config());
        // One max-size frame fills the duplex buffer and parks the writer
        // mid-frame; everything sent below is queued, not on the wire.
        tx.send(Frame::Data {
            stream_id: 1,
            payload: vec![0u8; MAX_FRAME_PAYLOAD as usize],
        })
        .await
        .expect("big frame");
        for _ in 0..20 {
            tx.send(Frame::Data {
                stream_id: 1,
                payload: vec![1],
            })
            .await
            .expect("small frame");
        }
        // Let the consumer classify the backlog before the priority frame.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(Frame::CloseStream { stream_id: 2 })
            .await
            .expect("priority frame");

        let first = read_one(&mut rx).await;
        assert!(
            matches!(first, Frame::Data { stream_id: 1, .. }),
            "the blocked socket write finishes first: {first:?}"
        );
        let second = read_one(&mut rx).await;
        assert!(
            matches!(second, Frame::CloseStream { stream_id: 2 }),
            "priority frames precede queued bulk data: {second:?}"
        );

        drop(tx);
        drop(rx);
        timeout(NO_DEADLOCK, handle.shutdown())
            .await
            .expect("shutdown within deadline");
    }

    #[tokio::test]
    async fn round_robin_interleaves_two_backlogged_streams() {
        let (tx, mut rx, handle) = spawn_test(tiny_config());
        let producer = tokio::spawn(async move {
            for _ in 0..300 {
                tx.send(Frame::Data {
                    stream_id: 1,
                    payload: vec![1],
                })
                .await
                .expect("stream 1 frame");
                tx.send(Frame::Data {
                    stream_id: 3,
                    payload: vec![3],
                })
                .await
                .expect("stream 3 frame");
            }
            tx
        });

        let mut count1 = 0usize;
        let mut count3 = 0usize;
        for _ in 0..200 {
            let frame = read_one(&mut rx).await;
            match frame {
                Frame::Data { stream_id: 1, .. } => count1 += 1,
                Frame::Data { stream_id: 3, .. } => count3 += 1,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        assert!(count1 > 0 && count3 > 0, "both streams must progress");
        assert!(
            count1.abs_diff(count3) <= 9,
            "the per-round budget bounds the skew: stream1={count1} stream3={count3}"
        );

        drop(producer.await.expect("producer"));
        drop(rx);
        timeout(NO_DEADLOCK, handle.shutdown())
            .await
            .expect("shutdown within deadline");
    }

    #[tokio::test]
    async fn bucket_cap_backpressures_the_producer_then_unblocks() {
        let (tx, mut rx, handle) = spawn_test(tiny_config());
        // The bucket floor is MAX_FRAME_PAYLOAD: one max-size frame fills it.
        tx.send(Frame::Data {
            stream_id: 1,
            payload: vec![0u8; MAX_FRAME_PAYLOAD as usize],
        })
        .await
        .expect("frame that fills the bucket");
        tx.send(Frame::Data {
            stream_id: 1,
            payload: vec![0u8; MAX_FRAME_PAYLOAD as usize],
        })
        .await
        .expect("second frame");
        tx.send(Frame::Data {
            stream_id: 1,
            payload: vec![1],
        })
        .await
        .expect("third frame");
        tx.send(Frame::Data {
            stream_id: 1,
            payload: vec![1],
        })
        .await
        .expect("fourth frame");

        // Fill the ingress FIFO until it reports full (the exact count
        // depends on whether the parked consumer already dequeued its frame);
        // then the producer must park — the end-to-end backpressure point.
        // Steady state before probing: the first frame is in the bucket,
        // the second classified the full bucket and the consumer is parked;
        // the writer is blocked writing the first frame to the unread
        // duplex. Only then is the FIFO guaranteed to fill.
        while handle.debug_classified_frames().await < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut tries = 0;
        loop {
            match tx.try_send(Frame::Data {
                stream_id: 1,
                payload: vec![1],
            }) {
                Ok(()) => tries += 1,
                Err(mpsc::error::TrySendError::Full(_)) => break,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    panic!("ingress should stay open")
                }
            }
            assert!(
                tries <= 64,
                "try_send never filled: the consumer is draining"
            );
        }
        let parked = timeout(
            Duration::from_millis(200),
            tx.send(Frame::Data {
                stream_id: 1,
                payload: vec![1],
            }),
        )
        .await;
        assert!(
            parked.is_err(),
            "a full bucket must propagate backpressure into the shared FIFO"
        );

        // Draining the written frame frees the bucket and un-parks the
        // pipeline: the same send succeeds afterwards.
        let frame = read_one(&mut rx).await;
        assert!(matches!(frame, Frame::Data { stream_id: 1, .. }));
        tx.send(Frame::Data {
            stream_id: 1,
            payload: vec![1],
        })
        .await
        .expect("send succeeds once budget frees");

        drop(tx);
        drop(rx);
        timeout(NO_DEADLOCK, handle.shutdown())
            .await
            .expect("shutdown within deadline");
    }

    #[tokio::test]
    async fn ingress_close_flushes_pending_data_then_exits() {
        let (tx, mut rx, handle) = spawn_test(tiny_config());
        let payload = vec![7u8; 32];
        for _ in 0..5 {
            tx.send(Frame::Data {
                stream_id: 1,
                payload: payload.clone(),
            })
            .await
            .expect("queued frame");
        }
        // Wait until the consumer classified everything before closing the
        // ingress (a queued-count check would race the writer's drain).
        while handle.debug_classified_frames().await < 5 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(tx);

        for _ in 0..5 {
            let frame = read_one(&mut rx).await;
            assert!(
                matches!(frame, Frame::Data { stream_id: 1, .. }),
                "pending data is flushed before the writer exits: {frame:?}"
            );
        }
        timeout(NO_DEADLOCK, handle.shutdown())
            .await
            .expect("writer exits after the flush");
    }
}
