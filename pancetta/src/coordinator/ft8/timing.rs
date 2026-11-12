//! PAN-318 Phase 1: bounded per-window decode-timing histograms.
//!
//! `ft8lib_ms`, `waterfall_ms` and `pre_native_ms` were already computed
//! and logged per window (the `decode.budget` `window timing` line,
//! PAN-165); this module adds bounded distributions over the same three
//! values so overlap effects are visible without an unbounded sample
//! vector or a new metrics dependency.
//!
//! Storage is constant: one fixed bucket array plus a total per value, no
//! matter how many windows record. Samples are recorded in MICROseconds so
//! sub-millisecond work stays meaningful; buckets are labelled in
//! milliseconds. A summary line is emitted on the `decode.budget` target
//! every [`EMIT_EVERY_WINDOWS`] completed windows (debug level — internal
//! diagnostics only, no TUI/health-panel change).

/// Fixed bucket upper bounds in milliseconds; a sample above the last
/// bound lands in the overflow bucket.
pub(super) const BUCKET_BOUNDS_MS: [u64; 11] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2000];

/// Emit the distribution line every N completed windows.
pub(super) const EMIT_EVERY_WINDOWS: u64 = 32;

/// One bounded histogram over microsecond samples with millisecond
/// bucket bounds.
#[derive(Debug)]
pub(super) struct TimingHistogram {
    counts: [u64; BUCKET_BOUNDS_MS.len() + 1],
    total: u64,
}

impl TimingHistogram {
    pub(super) const fn new() -> Self {
        Self {
            counts: [0; BUCKET_BOUNDS_MS.len() + 1],
            total: 0,
        }
    }

    /// Record one sample, in microseconds.
    pub(super) fn record_us(&mut self, micros: u64) {
        let bucket = BUCKET_BOUNDS_MS
            .iter()
            .position(|&bound_ms| micros <= bound_ms * 1_000)
            .unwrap_or(BUCKET_BOUNDS_MS.len());
        self.counts[bucket] += 1;
        self.total += 1;
    }

    /// Total samples recorded (must always equal the bucket sum).
    pub(super) fn total(&self) -> u64 {
        self.total
    }

    /// Samples in bucket `i` (the last is overflow).
    pub(super) fn bucket(&self, i: usize) -> u64 {
        self.counts[i.min(self.counts.len() - 1)]
    }

    /// `<=1ms|<=2ms|...|>2000ms` counts, e.g. `<= 1:3 <= 2:1 ... >2000:0`.
    pub(super) fn summary(&self) -> String {
        let mut s = String::new();
        for (i, &bound) in BUCKET_BOUNDS_MS.iter().enumerate() {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&format!("<= {bound}:{}", self.counts[i]));
        }
        s.push_str(&format!(
            " >{}:{}",
            BUCKET_BOUNDS_MS[BUCKET_BOUNDS_MS.len() - 1],
            self.counts[BUCKET_BOUNDS_MS.len()]
        ));
        s
    }
}

/// The three PAN-318 per-window values, each in one bounded histogram.
#[derive(Debug)]
pub(super) struct WindowTimingHistograms {
    pub(super) ft8lib_ms: TimingHistogram,
    pub(super) waterfall_ms: TimingHistogram,
    pub(super) pre_native_ms: TimingHistogram,
    windows: u64,
}

impl WindowTimingHistograms {
    pub(super) const fn new() -> Self {
        Self {
            ft8lib_ms: TimingHistogram::new(),
            waterfall_ms: TimingHistogram::new(),
            pre_native_ms: TimingHistogram::new(),
            windows: 0,
        }
    }

    /// Record one completed window (microsecond samples; overlap wall
    /// times, never sums of concurrent work).
    pub(super) fn record_window_us(
        &mut self,
        ft8lib_us: u64,
        waterfall_us: u64,
        pre_native_us: u64,
    ) {
        self.ft8lib_ms.record_us(ft8lib_us);
        self.waterfall_ms.record_us(waterfall_us);
        self.pre_native_ms.record_us(pre_native_us);
        self.windows += 1;
    }

    /// True on the window count that should emit the debug distribution
    /// line (every [`EMIT_EVERY_WINDOWS`] windows).
    pub(super) fn should_emit(&self) -> bool {
        self.windows > 0 && self.windows % EMIT_EVERY_WINDOWS == 0
    }

    /// One debug line covering all three distributions.
    pub(super) fn summary_line(&self) -> String {
        format!(
            "ft8lib_ms[{}] waterfall_ms[{}] pre_native_ms[{}]",
            self.ft8lib_ms.summary(),
            self.waterfall_ms.summary(),
            self.pre_native_ms.summary()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC7: zero and sub-millisecond samples are meaningful (first
    /// bucket), each boundary lands inclusive of its bound, the value
    /// just above a bound lands in the next bucket, and oversized
    /// samples land in the overflow bucket.
    #[test]
    fn timing_histogram_boundaries() {
        let mut h = TimingHistogram::new();
        let cases: [(u64, usize); 13] = [
            (0, 0),
            (999, 0),
            (1_000, 0), // exactly 1 ms -> <=1ms bucket
            (1_001, 1),
            (2_000, 1),
            (2_001, 2),
            (5_000, 2),
            (10_001, 4),
            (50_000, 5),
            (250_000, 7),
            (1_000_000, 9),
            (2_000_000, 10), // exactly 2 s -> last bounded bucket
            (2_000_001, 11), // above -> overflow
        ];
        for (sample, expected_bucket) in cases {
            let mut probe = TimingHistogram::new();
            probe.record_us(sample);
            assert_eq!(
                probe.bucket(expected_bucket),
                1,
                "sample {sample}us must land in bucket {expected_bucket}"
            );
            assert_eq!(probe.total(), 1);
        }
        h.record_us(u64::MAX / 2);
        assert_eq!(h.bucket(BUCKET_BOUNDS_MS.len()), 1, "overflow bucket");
    }

    /// AC7: every completed window records each value exactly once, the
    /// cumulative count equals the bucket sum, and storage stays constant
    /// after many windows (fixed arrays — the sum is computed from the
    /// same fixed slots after 10k windows as after one).
    #[test]
    fn timing_histograms_record_once_and_bound_storage() {
        let mut h = WindowTimingHistograms::new();
        for i in 0..10_000u64 {
            h.record_window_us(i % 3_000, (i % 7) * 250, i % 2_500);
        }
        for hist in [&h.ft8lib_ms, &h.waterfall_ms, &h.pre_native_ms] {
            let sum: u64 = (0..=BUCKET_BOUNDS_MS.len()).map(|i| hist.bucket(i)).sum();
            assert_eq!(sum, 10_000, "each window recorded exactly once");
            assert_eq!(sum, hist.total());
        }
        // Constant storage: the struct's size does not depend on the
        // sample count (fixed arrays; no Vec anywhere in the type).
        assert_eq!(
            std::mem::size_of::<WindowTimingHistograms>(),
            3 * std::mem::size_of::<TimingHistogram>() + std::mem::size_of::<u64>()
        );
    }

    /// The debug distribution line fires every 32 windows and never on a
    /// zero-window recorder.
    #[test]
    fn distribution_line_every_32_windows() {
        let mut h = WindowTimingHistograms::new();
        assert!(!h.should_emit());
        for i in 1..=64u64 {
            h.record_window_us(100, 100, 100);
            assert_eq!(h.should_emit(), i % EMIT_EVERY_WINDOWS == 0);
        }
        let line = h.summary_line();
        assert!(line.contains("ft8lib_ms"));
        assert!(line.contains("waterfall_ms"));
        assert!(line.contains("pre_native_ms"));
        assert!(line.contains("<= 1:"));
        assert!(line.contains(">2000:"));
    }
}
