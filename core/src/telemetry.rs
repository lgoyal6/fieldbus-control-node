//! Allocation-free wake-jitter telemetry.
//!
//! # Why a histogram and not a sample buffer
//!
//! A 10 ms loop run for any useful length produces more samples than a
//! microcontroller can hold, and keeping only min, max and mean hides exactly
//! the thing worth knowing, which is the shape of the tail. A fixed-width
//! histogram costs a constant [`NUM_BUCKETS`] + 1 counters regardless of run
//! length and still answers tail questions.
//!
//! # Resolution, stated plainly
//!
//! Buckets are [`BUCKET_WIDTH_US`] microseconds wide. Sample `v` lands in
//! bucket `v / 10`, and a percentile is reported as the **upper edge** of the
//! bucket that contains it. So a reported percentile `r` means the true value
//! lies in `(r - 10, r]`: the report is never optimistic, and it is never
//! wrong by more than one bucket width. [`JitterStats::min_us`] and
//! [`JitterStats::max_us`] are exact, unbucketed values.
//!
//! Samples at or above [`OVERFLOW_FLOOR_US`] all land in one overflow bucket.
//! A percentile that falls there is reported as [`OVERFLOW_FLOOR_US`] and is a
//! **lower bound** only; [`JitterStats::max_us`] still gives the exact worst
//! case. At a 10 ms period, a 40 ms jitter sample is four whole periods late,
//! so any run whose percentiles reach the overflow bucket has already failed
//! every deadline gate this repository states.

/// Width of one histogram bucket, in microseconds.
pub const BUCKET_WIDTH_US: u64 = 10;
/// Number of exact buckets, covering 0 through 39_999 us.
pub const NUM_BUCKETS: usize = 4_000;
/// Samples at or above this value land in the single overflow bucket.
pub const OVERFLOW_FLOOR_US: u64 = BUCKET_WIDTH_US * NUM_BUCKETS as u64;

/// Fixed-size jitter histogram plus exact scalar statistics.
///
/// This struct is roughly 16 KiB of counters. It is deliberately not `Copy`
/// and is meant to be constructed once and held, not passed by value: on a
/// Cortex-M target a by-value move of it would be a meaningful fraction of
/// the stack.
#[derive(Clone, Debug)]
pub struct JitterStats {
    buckets: [u32; NUM_BUCKETS + 1],
    count: u64,
    sum_us: u64,
    min_us: u64,
    max_us: u64,
    missed_deadlines: u64,
}

impl Default for JitterStats {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterStats {
    /// An empty histogram.
    pub const fn new() -> Self {
        JitterStats {
            buckets: [0; NUM_BUCKETS + 1],
            count: 0,
            sum_us: 0,
            min_us: u64::MAX,
            max_us: 0,
            missed_deadlines: 0,
        }
    }

    /// Records one wake-jitter sample in microseconds.
    pub fn record(&mut self, jitter_us: u64) {
        let idx = (jitter_us / BUCKET_WIDTH_US) as usize;
        let idx = if idx >= NUM_BUCKETS { NUM_BUCKETS } else { idx };
        self.buckets[idx] += 1;
        self.count += 1;
        self.sum_us = self.sum_us.saturating_add(jitter_us);
        if jitter_us < self.min_us {
            self.min_us = jitter_us;
        }
        if jitter_us > self.max_us {
            self.max_us = jitter_us;
        }
    }

    /// Records a missed deadline. Counted separately from jitter because a
    /// miss is a statement about when work *finished*, and jitter is a
    /// statement about when it *started*.
    pub fn record_miss(&mut self) {
        self.missed_deadlines += 1;
    }

    /// Number of jitter samples recorded.
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Exact smallest sample, or 0 if nothing has been recorded.
    pub const fn min_us(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.min_us
        }
    }

    /// Exact largest sample, or 0 if nothing has been recorded.
    pub const fn max_us(&self) -> u64 {
        self.max_us
    }

    /// Sum of all samples, saturating.
    pub const fn sum_us(&self) -> u64 {
        self.sum_us
    }

    /// Missed deadlines recorded via [`JitterStats::record_miss`].
    pub const fn missed_deadlines(&self) -> u64 {
        self.missed_deadlines
    }

    /// Arithmetic mean of the samples in microseconds, truncated.
    pub const fn mean_us(&self) -> u64 {
        match self.count {
            0 => 0,
            n => self.sum_us / n,
        }
    }

    /// Nearest-rank percentile, expressed in permille so the caller needs no
    /// floating point: 500 is p50, 950 is p95, 990 is p99.
    ///
    /// Returns the upper edge of the bucket holding the rank-th smallest
    /// sample, where `rank = ceil(count * permille / 1000)`, clamped to at
    /// least 1. See the module documentation for what that bound means.
    pub fn percentile_permille(&self, permille: u32) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let scaled = self.count * permille as u64;
        let mut rank = scaled / 1000;
        if !scaled.is_multiple_of(1000) {
            rank += 1;
        }
        if rank == 0 {
            rank = 1;
        }
        let mut cumulative: u64 = 0;
        for (i, &c) in self.buckets.iter().enumerate() {
            cumulative += c as u64;
            if cumulative >= rank {
                if i >= NUM_BUCKETS {
                    return OVERFLOW_FLOOR_US;
                }
                return (i as u64 + 1) * BUCKET_WIDTH_US;
            }
        }
        OVERFLOW_FLOOR_US
    }

    /// p50, as an upper-edge bound.
    pub fn p50_us(&self) -> u64 {
        self.percentile_permille(500)
    }

    /// p95, as an upper-edge bound.
    pub fn p95_us(&self) -> u64 {
        self.percentile_permille(950)
    }

    /// p99, as an upper-edge bound.
    pub fn p99_us(&self) -> u64 {
        self.percentile_permille(990)
    }

    /// Whether any sample landed in the overflow bucket, which makes
    /// percentiles at or above that point lower bounds.
    pub const fn overflowed(&self) -> bool {
        self.buckets[NUM_BUCKETS] > 0
    }
}
