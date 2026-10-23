//! Zero-allocation latency metrics (Phase 6).
//!
//! # Design
//! - **Nothing allocates on the recording path.** A [`LatencyHistogram`] is a
//!   fixed array of atomics; recording is a handful of relaxed atomic operations
//!   and never takes a lock, so it can run on the hot path and from any thread.
//! - **Microsecond resolution.** Durations come from [`std::time::Instant`]
//!   (monotonic) and are stored as whole microseconds. Buckets are powers of
//!   two: bucket `k >= 1` holds `[2^(k-1), 2^k)` µs and bucket `0` holds `0` µs,
//!   so a percentile is exact to within a factor of two and is never reported
//!   above the largest sample seen.
//! - **No sensitive data.** Metrics are counts and durations only: no keys,
//!   signatures, amounts or balances can reach them or their log summary.
//!
//! # Pipeline timing
//! [`PipelineMetrics`] follows an order through the exit pipeline:
//!
//! ```text
//!  Geyser update dequeued ──▶ order issued ──▶ transaction assembled
//!         (update received)       (mark_order)       (mark_assembled)
//!          └──── ingest_to_order ────┘└─ order_to_assembled ─┘
//!          └──────────────── ingest_to_assembled ────────────┘
//! ```
//!
//! The *ingest* instant is taken when the exit manager receives the stream
//! update, before it is decoded, so decoding and decision time are included.
//! Only orders issued **in reaction to an update** (a stop-loss, or a tranche
//! that was waiting for its first observation) have an ingest instant: orders
//! fired by the schedule's own timer are not a reaction to any event, and are
//! measured from issue to assembly only.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::info;

/// Number of histogram buckets. The last one is open-ended and starts at about 3.2 days.
pub const BUCKETS: usize = 40;

/// Number of order sequences tracked (`SellOrder::sequence` is a `u8`).
pub const TRACKED_ORDERS: usize = 256;

/// Whole microseconds in `elapsed`, saturating at `u64::MAX`.
#[must_use]
pub fn duration_micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

/// Whole microseconds from `from` to `to` (zero if `to` is earlier).
#[must_use]
pub fn micros_between(from: Instant, to: Instant) -> u64 {
    duration_micros(to.saturating_duration_since(from))
}

/// The bucket a sample of `micros` falls in.
fn bucket_index(micros: u64) -> usize {
    if micros == 0 {
        return 0;
    }
    let bits = u64::BITS.saturating_sub(micros.leading_zeros());
    usize::try_from(bits)
        .unwrap_or(BUCKETS)
        .min(BUCKETS.saturating_sub(1))
}

/// The largest value bucket `index` can hold. The last bucket is open-ended.
fn bucket_upper_bound(index: usize) -> u64 {
    if index == 0 {
        return 0;
    }
    if index >= BUCKETS.saturating_sub(1) {
        return u64::MAX;
    }
    match u32::try_from(index) {
        Ok(bits) if bits < u64::BITS => (1u64 << bits).saturating_sub(1),
        _ => u64::MAX,
    }
}

/// A started stopwatch: a thin, `Copy` wrapper over [`Instant`].
#[derive(Debug, Clone, Copy)]
pub struct Stopwatch(Instant);

impl Stopwatch {
    /// Starts a stopwatch now.
    #[must_use]
    pub fn start() -> Self {
        Self(Instant::now())
    }

    /// Microseconds elapsed since the stopwatch started.
    #[must_use]
    pub fn elapsed_micros(&self) -> u64 {
        duration_micros(self.0.elapsed())
    }

    /// Records the elapsed time into `histogram` and returns it in microseconds.
    pub fn stop(&self, histogram: &LatencyHistogram) -> u64 {
        let micros = self.elapsed_micros();
        histogram.record_micros(micros);
        micros
    }
}

/// A lock-free, allocation-free latency histogram.
#[derive(Debug)]
pub struct LatencyHistogram {
    count: AtomicU64,
    sum_us: AtomicU64,
    min_us: AtomicU64,
    max_us: AtomicU64,
    buckets: [AtomicU64; BUCKETS],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    /// An empty histogram.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            min_us: AtomicU64::new(u64::MAX),
            max_us: AtomicU64::new(0),
            buckets: [const { AtomicU64::new(0) }; BUCKETS],
        }
    }

    /// Records a duration.
    pub fn record(&self, elapsed: Duration) {
        self.record_micros(duration_micros(elapsed));
    }

    /// Records a sample of `micros` microseconds.
    pub fn record_micros(&self, micros: u64) {
        if let Some(bucket) = self.buckets.get(bucket_index(micros)) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        // A saturating sum: wrapping would silently corrupt the mean.
        let _ = self
            .sum_us
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |sum| {
                Some(sum.saturating_add(micros))
            });
        self.min_us.fetch_min(micros, Ordering::Relaxed);
        self.max_us.fetch_max(micros, Ordering::Relaxed);
    }

    /// Samples recorded so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// A point-in-time copy. Concurrent recordings may or may not be included,
    /// but a snapshot never shows a sample in one field and not its bucket for
    /// longer than the few instructions between the two stores.
    #[must_use]
    pub fn snapshot(&self) -> LatencySnapshot {
        let mut buckets = [0u64; BUCKETS];
        for (slot, bucket) in buckets.iter_mut().zip(self.buckets.iter()) {
            *slot = bucket.load(Ordering::Relaxed);
        }
        let count = self.count.load(Ordering::Relaxed);
        let min = self.min_us.load(Ordering::Relaxed);
        LatencySnapshot {
            count,
            sum_us: self.sum_us.load(Ordering::Relaxed),
            min_us: if count == 0 || min == u64::MAX {
                0
            } else {
                min
            },
            max_us: self.max_us.load(Ordering::Relaxed),
            buckets,
        }
    }
}

/// An immutable copy of a [`LatencyHistogram`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySnapshot {
    /// Samples recorded.
    pub count: u64,
    /// Sum of all samples, in microseconds (saturating).
    pub sum_us: u64,
    /// Smallest sample, in microseconds (`0` when empty).
    pub min_us: u64,
    /// Largest sample, in microseconds.
    pub max_us: u64,
    /// Samples per power-of-two bucket.
    pub buckets: [u64; BUCKETS],
}

impl Default for LatencySnapshot {
    fn default() -> Self {
        Self {
            count: 0,
            sum_us: 0,
            min_us: 0,
            max_us: 0,
            buckets: [0; BUCKETS],
        }
    }
}

impl LatencySnapshot {
    /// Mean sample in microseconds, rounded down (`0` when empty).
    #[must_use]
    pub fn mean_us(&self) -> u64 {
        self.sum_us.checked_div(self.count).unwrap_or(0)
    }

    /// The `percent`-th percentile (`0..=100`, larger values are clamped), in
    /// microseconds. It is the upper bound of the bucket holding that rank,
    /// capped at the largest sample, so it never understates by more than a
    /// factor of two and never exceeds [`LatencySnapshot::max_us`]. `0` when empty.
    #[must_use]
    pub fn percentile_us(&self, percent: u8) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let percent = u128::from(percent.min(100));
        // rank = ceil(count * percent / 100), at least 1.
        let rank = u128::from(self.count)
            .saturating_mul(percent)
            .div_ceil(100)
            .max(1);
        let mut seen: u128 = 0;
        for (index, bucket) in self.buckets.iter().enumerate() {
            seen = seen.saturating_add(u128::from(*bucket));
            if seen >= rank {
                return bucket_upper_bound(index).clamp(self.min_us, self.max_us);
            }
        }
        self.max_us
    }

    /// Median, in microseconds.
    #[must_use]
    pub fn p50_us(&self) -> u64 {
        self.percentile_us(50)
    }

    /// 95th percentile, in microseconds.
    #[must_use]
    pub fn p95_us(&self) -> u64 {
        self.percentile_us(95)
    }

    /// 99th percentile, in microseconds.
    #[must_use]
    pub fn p99_us(&self) -> u64 {
        self.percentile_us(99)
    }
}

/// Nanoseconds from `base` to `at`, plus one so that `0` can mean "unset".
fn stamp_of(base: Instant, at: Instant) -> u64 {
    u64::try_from(at.saturating_duration_since(base).as_nanos())
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

/// Microseconds between two stamps produced by [`stamp_of`].
fn stamp_delta_us(from: u64, to: u64) -> u64 {
    to.saturating_sub(from) / 1_000
}

/// Per-order timing of the exit pipeline. Share it with an `Arc`.
#[derive(Debug)]
pub struct PipelineMetrics {
    base: Instant,
    ingest_stamp: [AtomicU64; TRACKED_ORDERS],
    order_stamp: [AtomicU64; TRACKED_ORDERS],
    /// Update received → order issued (reactive orders only).
    pub ingest_to_order: LatencyHistogram,
    /// Order issued → transaction assembled and signed (every order).
    pub order_to_assembled: LatencyHistogram,
    /// Update received → transaction assembled (reactive orders only): the
    /// end-to-end reaction latency.
    pub ingest_to_assembled: LatencyHistogram,
    observations: AtomicU64,
    orders: AtomicU64,
    reactive_orders: AtomicU64,
    assembled: AtomicU64,
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineMetrics {
    /// Fresh metrics; the clock origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            ingest_stamp: [const { AtomicU64::new(0) }; TRACKED_ORDERS],
            order_stamp: [const { AtomicU64::new(0) }; TRACKED_ORDERS],
            ingest_to_order: LatencyHistogram::new(),
            order_to_assembled: LatencyHistogram::new(),
            ingest_to_assembled: LatencyHistogram::new(),
            observations: AtomicU64::new(0),
            orders: AtomicU64::new(0),
            reactive_orders: AtomicU64::new(0),
            assembled: AtomicU64::new(0),
        }
    }

    /// Counts one applied curve observation.
    pub fn count_observation(&self) {
        self.observations.fetch_add(1, Ordering::Relaxed);
    }

    /// Marks an order as issued now. `ingest` is the instant of the update that
    /// triggered it, or `None` when the schedule's timer did.
    pub fn mark_order(&self, sequence: u8, ingest: Option<Instant>) {
        let now = Instant::now();
        let index = usize::from(sequence);
        let now_stamp = stamp_of(self.base, now);
        self.orders.fetch_add(1, Ordering::Relaxed);
        let ingest_stamp = ingest.map_or(0, |at| stamp_of(self.base, at));
        if let Some(slot) = self.ingest_stamp.get(index) {
            slot.store(ingest_stamp, Ordering::Relaxed);
        }
        if let Some(slot) = self.order_stamp.get(index) {
            slot.store(now_stamp, Ordering::Relaxed);
        }
        if ingest.is_some() {
            self.reactive_orders.fetch_add(1, Ordering::Relaxed);
            self.ingest_to_order
                .record_micros(stamp_delta_us(ingest_stamp, now_stamp));
        }
    }

    /// Marks the transaction of order `sequence` as assembled and signed now.
    /// Orders that were never marked as issued are counted but not timed.
    pub fn mark_assembled(&self, sequence: u8) {
        let now_stamp = stamp_of(self.base, Instant::now());
        let index = usize::from(sequence);
        self.assembled.fetch_add(1, Ordering::Relaxed);
        let issued = self
            .order_stamp
            .get(index)
            .map_or(0, |slot| slot.load(Ordering::Relaxed));
        if issued != 0 {
            self.order_to_assembled
                .record_micros(stamp_delta_us(issued, now_stamp));
        }
        let ingested = self
            .ingest_stamp
            .get(index)
            .map_or(0, |slot| slot.load(Ordering::Relaxed));
        if ingested != 0 {
            self.ingest_to_assembled
                .record_micros(stamp_delta_us(ingested, now_stamp));
        }
    }

    /// A point-in-time copy of every counter and histogram.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            ingest_to_order: self.ingest_to_order.snapshot(),
            order_to_assembled: self.order_to_assembled.snapshot(),
            ingest_to_assembled: self.ingest_to_assembled.snapshot(),
            observations: self.observations.load(Ordering::Relaxed),
            orders: self.orders.load(Ordering::Relaxed),
            reactive_orders: self.reactive_orders.load(Ordering::Relaxed),
            assembled: self.assembled.load(Ordering::Relaxed),
        }
    }

    /// Logs a one-line summary at `info`: counts and microsecond percentiles
    /// only, nothing sensitive.
    pub fn log_summary(&self) {
        let snapshot = self.snapshot();
        info!(
            observations = snapshot.observations,
            orders = snapshot.orders,
            reactive_orders = snapshot.reactive_orders,
            assembled = snapshot.assembled,
            reaction_p50_us = snapshot.ingest_to_assembled.p50_us(),
            reaction_p99_us = snapshot.ingest_to_assembled.p99_us(),
            assembly_p50_us = snapshot.order_to_assembled.p50_us(),
            assembly_p99_us = snapshot.order_to_assembled.p99_us(),
            "pipeline latency summary"
        );
    }
}

/// The name the roadmap uses for the end-to-end latency tracker: an alias of
/// [`PipelineMetrics`] (histograms of stream-update-received to
/// transaction-assembled time, in microseconds).
pub type LatencyMetrics = PipelineMetrics;

/// An immutable copy of [`PipelineMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    /// Update received → order issued (reactive orders only).
    pub ingest_to_order: LatencySnapshot,
    /// Order issued → transaction assembled (every order).
    pub order_to_assembled: LatencySnapshot,
    /// Update received → transaction assembled (reactive orders only).
    pub ingest_to_assembled: LatencySnapshot,
    /// Curve observations applied.
    pub observations: u64,
    /// Orders issued.
    pub orders: u64,
    /// Orders issued in reaction to an update.
    pub reactive_orders: u64,
    /// Transactions assembled.
    pub assembled: u64,
}
