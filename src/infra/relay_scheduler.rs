//! Per-link egress scheduler: stream isolation (per-stream byte bounds) and
//! fair scheduling (deficit round robin) between the routed streams of one
//! relay link (issue #33), with dual-channel ingress so control frames
//! bypass a backpressured bulk queue (issue #105).
//!
//! Frames bound for a link — routed stream data from any number of sources
//! plus control frames — arrive over two bounded FIFOs, each sized
//! `SchedulerConfig.ingress_capacity`:
//!
//! - bulk: `Data`, `Window`, and `Close`. These must keep per-stream FIFO
//!   order: a `Close` must never overtake its own stream's `Data`, because
//!   the mux consumer fails the whole connection on `Data` after `peer_fin`
//!   (`relay_mux/connection.rs`) and the wire format promises EOF only once
//!   buffered data drains (`Frame::is_bulk_ordered`).
//! - control: every other variant — register/unregister/heartbeat,
//!   `OpenStream`/`Error`/`CloseStream`, presence, enrollment. These are
//!   resets and lifecycle frames; a stream parked at its byte cap must not
//!   delay them.
//!
//! Producers hold a [`SchedulerIngress`], which routes each frame to one of
//! the two FIFOs by kind. Two tasks sit between the FIFOs and the socket:
//!
//! - an ingress consumer that moves frames into the scheduling queues:
//!   `Data`/`Window`/`Close` go to a per-stream bucket keyed by the frame's
//!   stream id (already the target-side id from the route rewrite);
//!   everything else goes to a priority queue. A frame whose bucket is full
//!   parks the consumer's bulk arm only; the control arm keeps dequeuing, so
//!   a teardown behind a stalled stream still classifies (issue #105). The
//!   consumer is the only bucket enqueuer — that invariant is what makes the
//!   check-then-enqueue race-free.
//! - a writer task that drains the priority queue fully first, then serves
//!   live buckets with one deficit-round-robin pass per loop iteration.
//!
//! Backpressure is end-to-end: a bucket at its byte cap, or a link at its
//! total budget, parks the consumer's bulk arm (await on a `Notify`); the
//! bulk FIFO (256 frames) fills next; and the source link's
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
//! consumer takes it to enqueue (waiting for budget happens outside it, via
//! `space_freed`); the writer takes it to take batches out (priority drain,
//! RR selection, accounting) and writes to the socket only after releasing
//! it. Wake-ups (`Notify`) are issued after the lock is dropped. No other
//! lock is ever acquired while holding it, so there is no lock ordering
//! beyond this leaf. The consumer's parked-frame slot lives in its own task
//! state rather than in the mutex, so retrying a parked enqueue needs no
//! extra lock.
//!
//! # Idle behavior and shutdown
//!
//! When no queue holds anything and the ingress is open, the writer parks on
//! a `Notify` signaled by the consumer — no busy poll. Buckets emptied and
//! not re-fed for `IDLE_REAP_PASSES` rounds are reaped (the stream ended).
//! When both ingress channels close (every producer dropped, i.e. link
//! teardown) and no bulk frame is parked, the consumer sets
//! `ingress_open = false`; the writer finishes draining the priority queue
//! and the buckets, then exits — the flush semantics the raw FIFO writer
//! had.

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
    /// Capacity of each ingress FIFO (bulk and control) — the producers'
    /// backpressure horizon (`LINK_OUTBOUND_QUEUE` at the call site).
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
pub(crate) struct SchedulerState {
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

/// Test-only registry publishing each link's egress scheduler under the
/// link's node id. Unit tests hold the [`SchedulerHandle`] directly; relay
/// integration tests drive a real relay and need to await classification
/// milestones without racing the writer (issue #122).
#[cfg(test)]
pub(crate) mod test_registry {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    use super::{lock, SchedulerState};

    fn registry() -> &'static Mutex<HashMap<String, Arc<SchedulerState>>> {
        static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<SchedulerState>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn register(node_id: &str, state: Arc<SchedulerState>) {
        if let Ok(mut registry) = registry().lock() {
            registry.insert(node_id.to_string(), state);
        }
    }

    pub(crate) fn unregister(node_id: &str) {
        if let Ok(mut registry) = registry().lock() {
            registry.remove(node_id);
        }
    }

    /// Cumulative classified-frame count of `node_id`'s link, if registered.
    pub(crate) async fn classified_frames(node_id: &str) -> Option<u64> {
        let state = registry().lock().ok()?.get(node_id)?.clone();
        let frames = lock(&state.shared).classified_frames;
        Some(frames)
    }
}

#[cfg(test)]
impl SchedulerHandle {
    /// Publishes this scheduler under the link's node id for integration tests.
    pub(crate) fn publish_for_tests(&self, node_id: &str) {
        test_registry::register(node_id, self.state.clone());
    }

    /// Removes the registration installed by [`Self::publish_for_tests`].
    pub(crate) fn unpublish_for_tests(&self, node_id: &str) {
        test_registry::unregister(node_id);
    }
}

pub(crate) struct LinkScheduler;

/// Producers' handle into a link's egress scheduler. It owns the two ingress
/// FIFOs (bulk and control, see the module doc) and routes each frame to the
/// right one by [`Frame::is_bulk_ordered`], so a frame that must bypass a
/// backpressured bulk queue never queues behind bulk data in the first place
/// (issue #105). Call sites keep the raw-sender syntax: `send`, `try_send`,
/// and `clone`.
#[derive(Clone)]
pub(crate) struct SchedulerIngress {
    bulk: mpsc::Sender<Frame>,
    control: mpsc::Sender<Frame>,
}

impl SchedulerIngress {
    /// Queues `frame` on its classified channel, parking when that channel
    /// is full. An error means every scheduler handle is gone (link
    /// teardown).
    pub(crate) async fn send(&self, frame: Frame) -> Result<(), mpsc::error::SendError<Frame>> {
        if frame.is_bulk_ordered() {
            self.bulk.send(frame).await
        } else {
            self.control.send(frame).await
        }
    }

    /// Non-blocking variant of [`SchedulerIngress::send`]: `Full` carries
    /// the frame back on a full channel.
    pub(crate) fn try_send(&self, frame: Frame) -> Result<(), mpsc::error::TrySendError<Frame>> {
        if frame.is_bulk_ordered() {
            self.bulk.try_send(frame)
        } else {
            self.control.try_send(frame)
        }
    }

    /// Test-only constructor over fresh channels; returns both receivers so
    /// assertions can observe what was sent (bulk frames arrive on the
    /// first, control frames on the second).
    #[cfg(test)]
    pub(crate) fn test_channels(
        capacity: usize,
    ) -> (Self, mpsc::Receiver<Frame>, mpsc::Receiver<Frame>) {
        let (bulk_tx, bulk_rx) = mpsc::channel(capacity);
        let (control_tx, control_rx) = mpsc::channel(capacity);
        (
            Self {
                bulk: bulk_tx,
                control: control_tx,
            },
            bulk_rx,
            control_rx,
        )
    }
}

impl LinkScheduler {
    /// Spawns the ingress consumer and the writer task over `writer`.
    /// Returns the ingress handle (clone it into connection entries and
    /// routing legs exactly like the raw channel before) and the handle.
    pub(crate) fn spawn<W: AsyncWrite + Unpin + Send + 'static>(
        config: SchedulerConfig,
        writer: W,
    ) -> (SchedulerIngress, SchedulerHandle) {
        // Floors keep invariants: an empty bucket always accepts one
        // max-size frame, and a round always sends at least one frame.
        let bucket_capacity = config.bucket_capacity_bytes.max(MAX_FRAME_PAYLOAD as usize);
        let link_budget = config.link_budget_bytes.max(bucket_capacity);
        let round_budget = config.round_budget_bytes.max(1);
        let capacity = config.ingress_capacity.max(1);
        let (bulk_tx, bulk_rx) = mpsc::channel(capacity);
        let (control_tx, control_rx) = mpsc::channel(capacity);
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
            bulk_rx,
            control_rx,
            state.clone(),
        ));
        let writer = tokio::spawn(writer_loop(round_budget, writer, state.clone()));
        (
            SchedulerIngress {
                bulk: bulk_tx,
                control: control_tx,
            },
            SchedulerHandle {
                state,
                consumer,
                writer,
            },
        )
    }
}

/// Outcome of fitting one bulk-ordered frame into its per-stream bucket.
/// `Park` hands the frame back: the caller parks it and retries once the
/// writer frees budget.
enum BucketEnqueue {
    Fits,
    Park(Frame),
    Drop,
}

/// Capacity check and enqueue are one critical section: the consumer is the
/// only bucket enqueuer, and the writer only removes frames and sets
/// `writer_done`, so nothing can interleave between the check and the push.
/// `Shared` stays a leaf: no await inside the lock.
fn enqueue_bucket(
    state: &SchedulerState,
    bucket_capacity: usize,
    link_budget: usize,
    frame: Frame,
) -> BucketEnqueue {
    let stream_id = frame.stream_id();
    let len = frame.payload_len();
    let mut guard = lock(&state.shared);
    if guard.writer_done {
        // The writer is gone (socket error): the link is dying, drop the
        // frame.
        return BucketEnqueue::Drop;
    }
    let total = guard.total_bucket_bytes;
    let bucket = guard.buckets.entry(stream_id).or_insert_with(Bucket::new);
    if bucket.queued_bytes + len <= bucket_capacity && total + len <= link_budget {
        bucket.queue.push_back(frame);
        bucket.queued_bytes += len;
        bucket.idle_passes = 0;
        guard.total_bucket_bytes += len;
        guard.classified_frames += 1;
        BucketEnqueue::Fits
    } else {
        BucketEnqueue::Park(frame)
    }
}

/// Moves every ingress frame into the scheduling queues (per-stream buckets
/// for bulk, the priority queue for control). Single consumer task: it is
/// the only bucket enqueuer, which is what makes `enqueue_bucket` race-free.
///
/// The bulk and control FIFOs are consumed by separate `select!` arms so a
/// control frame classifies even while a bulk frame parks the bulk arm on a
/// full bucket (issue #105). The parked frame lives in `parked` (task state,
/// not the mutex) and retries its enqueue on every `space_freed` signal.
async fn ingress_consumer_loop(
    bucket_capacity: usize,
    link_budget: usize,
    mut bulk_rx: mpsc::Receiver<Frame>,
    mut control_rx: mpsc::Receiver<Frame>,
    state: Arc<SchedulerState>,
) {
    let mut parked: Option<Frame> = None;
    let mut bulk_closed = false;
    let mut control_closed = false;
    loop {
        if bulk_closed && control_closed && parked.is_none() {
            break;
        }
        tokio::select! {
            biased;
            // A parked bulk frame retries its enqueue first: a fresh budget
            // permit must not wait behind new arrivals.
            _ = state.space_freed.notified(), if parked.is_some() => {
                let Some(frame) = parked.take() else {
                    continue;
                };
                match enqueue_bucket(&state, bucket_capacity, link_budget, frame) {
                    BucketEnqueue::Fits => state.work_available.notify_one(),
                    // Still at cap: stay parked; the control arm above keeps
                    // this task responsive to control frames regardless.
                    BucketEnqueue::Park(frame) => parked = Some(frame),
                    BucketEnqueue::Drop => {}
                }
            }
            frame = control_rx.recv(), if !control_closed => match frame {
                Some(frame) => {
                    {
                        let mut guard = lock(&state.shared);
                        guard.priority.push_back(frame);
                        guard.classified_frames += 1;
                    }
                    state.work_available.notify_one();
                }
                None => control_closed = true,
            },
            frame = bulk_rx.recv(), if !bulk_closed && parked.is_none() => match frame {
                Some(frame) => {
                    if frame.is_bulk_ordered() {
                        match enqueue_bucket(&state, bucket_capacity, link_budget, frame) {
                            BucketEnqueue::Fits => state.work_available.notify_one(),
                            // Bucket at cap or link budget exhausted: park
                            // the bulk arm until the writer frees budget or
                            // dies. The bulk FIFO fills next and
                            // `forward_routed` parks behind it — the
                            // end-to-end backpressure point. Control frames
                            // keep flowing past this park (issue #105).
                            BucketEnqueue::Park(frame) => parked = Some(frame),
                            BucketEnqueue::Drop => {}
                        }
                    } else {
                        // Defense in depth: `SchedulerIngress` already routes
                        // non-bulk frames to the control channel.
                        let mut guard = lock(&state.shared);
                        guard.priority.push_back(frame);
                        guard.classified_frames += 1;
                        drop(guard);
                        state.work_available.notify_one();
                    }
                }
                None => bulk_closed = true,
            },
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

    fn spawn_test(config: SchedulerConfig) -> (SchedulerIngress, DuplexStream, SchedulerHandle) {
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
    async fn control_frames_bypass_a_parked_bulk_consumer() {
        let (ingress, mut rx, handle) = spawn_test(tiny_config());
        // Park the consumer's bulk arm exactly like
        // `bucket_cap_backpressures_the_producer_then_unblocks`: the first
        // max-size frame fills the bucket, the second frame classified
        // against the full bucket parks the bulk arm, and the writer blocks
        // mid-frame on the unread duplex.
        ingress
            .send(Frame::Data {
                stream_id: 1,
                payload: vec![0u8; MAX_FRAME_PAYLOAD as usize],
            })
            .await
            .expect("big frame");
        ingress
            .send(Frame::Data {
                stream_id: 1,
                payload: vec![0u8; MAX_FRAME_PAYLOAD as usize],
            })
            .await
            .expect("second frame");
        while handle.debug_classified_frames().await < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // One more bulk frame lands on the full bucket: the bulk arm parks
        // on it (it cannot classify until the writer frees budget). Settle
        // so the park — not merely classification of the backlog — is the
        // state under test.
        ingress
            .send(Frame::Data {
                stream_id: 1,
                payload: vec![1],
            })
            .await
            .expect("frame behind the full bucket");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A control frame must still classify promptly instead of queueing
        // behind the stalled bulk stream — without reading the socket.
        ingress
            .send(Frame::CloseStream { stream_id: 2 })
            .await
            .expect("control frame");
        timeout(Duration::from_millis(500), async {
            while handle.debug_classified_frames().await < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("control frame classified despite the parked bulk arm");

        // Wire order: the socket write blocked mid-frame finishes first,
        // then the teardown egresses before the parked stream's backlog.
        let first = read_one(&mut rx).await;
        assert!(
            matches!(first, Frame::Data { stream_id: 1, .. }),
            "the blocked socket write finishes first: {first:?}"
        );
        let second = read_one(&mut rx).await;
        assert!(
            matches!(second, Frame::CloseStream { stream_id: 2 }),
            "the control frame egresses before the parked stream's backlog: {second:?}"
        );

        drop(ingress);
        drop(rx);
        timeout(NO_DEADLOCK, handle.shutdown())
            .await
            .expect("shutdown within deadline");
    }

    #[tokio::test]
    async fn same_stream_close_stays_behind_its_data() {
        let (ingress, mut rx, handle) = spawn_test(tiny_config());
        // Payloads sized so the second frame's write blocks on the 1024-byte
        // duplex: until the test reads, wire order is frozen and the only
        // ordering source is the scheduler's queues.
        const FRAMES: usize = 4;
        for seq in 0..FRAMES {
            ingress
                .send(Frame::Data {
                    stream_id: 5,
                    payload: vec![seq as u8; 400],
                })
                .await
                .expect("data frame");
        }
        ingress
            .send(Frame::Close { stream_id: 5 })
            .await
            .expect("half-close");
        while handle.debug_classified_frames().await < FRAMES as u64 + 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        for seq in 0..FRAMES {
            let frame = read_one(&mut rx).await;
            match frame {
                Frame::Data { stream_id, payload } => {
                    assert_eq!(stream_id, 5, "every data frame belongs to the stream");
                    assert_eq!(
                        payload,
                        vec![seq as u8; 400],
                        "per-stream FIFO order is preserved"
                    );
                }
                other => panic!("data frame {seq} must precede the half-close: {other:?}"),
            }
        }
        let last = read_one(&mut rx).await;
        assert!(
            matches!(last, Frame::Close { stream_id: 5 }),
            "the half-close follows its own stream's data: {last:?}"
        );

        drop(ingress);
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
