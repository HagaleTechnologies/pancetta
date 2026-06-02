//! hb-057 V1/V2: median-DT-per-callsign history + lookup trait.
//!
//! See `docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md`.
//!
//! This module defines `DtPrior` and the `DtPriorLookup` trait that the
//! decoder consumes during the residual sync pass. A reference in-memory
//! implementation (`InMemoryDtHistory`) is provided for the eval harness;
//! the production pancetta coordinator wraps `pancetta_qso::CrossTimeState`
//! and provides its own adapter (kept out of this crate to avoid a
//! pancetta-ft8 → pancetta-qso dep edge).
//!
//! # Mechanism (V1)
//!
//! For each successful decode produced by pass 1, the caller records the
//! `(callsign, time_offset, wall_clock)` triple. When the multipass
//! residual `costas_sync_search` returns candidates, those candidates can
//! be narrowed in the t0 axis to the union of per-callsign DT-prior
//! windows (window = max(`floor_s`, IQR * `iqr_scale`) around median).
//! If no priors are available, the narrowing is a no-op (full t0 sweep
//! preserved).
//!
//! # Mechanism (V2 — per-candidate callsign-keyed sync)
//!
//! V1 (Session 2 SHELVED) keyed the prior lookup by "callsigns decoded in
//! THIS WAV's pass 1" — the WRONG key. The right key is "callsigns whose
//! prior sightings predict THIS candidate's frequency." V2 (Session 3)
//! also records the per-decode frequency_offset and exposes
//! `priors_near_freq(freq_hz, window_hz)` so the decoder can look up
//! candidate-specific priors at residual-sync time: for each candidate
//! at (cand_freq, time_step), the union of nearby-callsigns' DT windows
//! is the gate. Cold-start (no priors near the candidate) preserves
//! the candidate.
//!
//! See the spec for the kill-switch diagnostic (38.6% coverage on top-20
//! hard-200) and Phase A graduation criteria. Prior art for the
//! per-callsign median-DT statistic: JTDX commit "use median filter in
//! average DT calculation" (Feb 2022).

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Default minimum sightings before a DT prior is returned.
pub const DEFAULT_MIN_SIGHTINGS: usize = 2;

/// Default per-callsign capacity (ring buffer length).
pub const DEFAULT_PER_CALLSIGN_CAPACITY: usize = 10;

/// Default max age for a sighting (30 min). Matches the callsign-continuity
/// rolling window pancetta uses elsewhere.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// Median-DT prior for a single callsign.
///
/// Returned by [`DtPriorLookup::prior`] when at least `min_sightings`
/// non-expired sightings exist. Used by the decoder to narrow the t0 axis
/// of the residual Costas sync search.
#[derive(Clone, Copy, Debug)]
pub struct DtPrior {
    /// Median DT across current sightings (seconds, slot-relative).
    pub median_dt: f64,
    /// Inter-quartile range (P75 - P25). Used to widen the prior gate
    /// when within-callsign variance is higher.
    pub iqr: f64,
    /// Number of sightings the median was computed from (≥ `min_sightings`).
    pub sighting_count: usize,
}

/// Lookup interface for per-callsign DT priors.
///
/// The decoder takes an `Arc<dyn DtPriorLookup + Send + Sync>` so a
/// downstream coordinator can plug in any backing store (the in-memory
/// implementation in this crate, or a wrapper around
/// `pancetta_qso::CrossTimeState`).
pub trait DtPriorLookup: Send + Sync {
    /// Return the DT prior for `callsign`, or `None` if no prior is
    /// available (insufficient sightings, expired, unknown callsign).
    fn prior(&self, callsign: &str) -> Option<DtPrior>;

    /// V2 (Session 3): return the union of DT priors for all callsigns
    /// whose recent sightings were within `freq_window_hz` of
    /// `target_freq_hz`. The decoder uses this at residual-sync time to
    /// narrow each candidate's t0 axis by the priors of callsigns
    /// "plausibly at this frequency" — implementing the per-candidate
    /// callsign-keyed narrowing the V1 hook missed.
    ///
    /// Default impl returns an empty `Vec` (preserves backward
    /// compatibility for impls that pre-date V2; the decoder treats
    /// "empty" as "no narrowing" by construction).
    fn priors_near_freq(&self, _target_freq_hz: f64, _freq_window_hz: f64) -> Vec<DtPrior> {
        Vec::new()
    }
}

/// In-memory reference implementation of `DtPriorLookup`.
///
/// Thread-safe via `Mutex`. Capacity-bounded per callsign and per-sighting
/// age-bounded. Designed for eval-harness use; production wraps
/// `pancetta_qso::CrossTimeState` instead.
pub struct InMemoryDtHistory {
    inner: Mutex<DtHistoryInner>,
}

#[derive(Debug)]
struct DtHistoryInner {
    entries: std::collections::HashMap<String, std::collections::VecDeque<DtSighting>>,
    capacity: usize,
    max_age: Duration,
    min_sightings: usize,
}

#[derive(Clone, Copy, Debug)]
struct DtSighting {
    at: SystemTime,
    dt_s: f64,
    /// V2 (Session 3): per-sighting frequency in Hz. Used by
    /// `priors_near_freq` to gate which callsigns participate in the
    /// per-candidate narrowing. `f64::NAN` for legacy sightings recorded
    /// via `record(...)` (without freq), which excludes them from
    /// `priors_near_freq` queries (they remain returned by `prior(...)`).
    freq_hz: f64,
}

impl Default for InMemoryDtHistory {
    fn default() -> Self {
        Self::new(
            DEFAULT_PER_CALLSIGN_CAPACITY,
            DEFAULT_MAX_AGE,
            DEFAULT_MIN_SIGHTINGS,
        )
    }
}

impl InMemoryDtHistory {
    /// Build a new in-memory DT history store.
    pub fn new(capacity: usize, max_age: Duration, min_sightings: usize) -> Self {
        Self {
            inner: Mutex::new(DtHistoryInner {
                entries: std::collections::HashMap::new(),
                capacity,
                max_age,
                min_sightings,
            }),
        }
    }

    /// Record a sighting (legacy, V1). Evicts expired sightings for this
    /// callsign before inserting. The recorded sighting's freq is set to
    /// NaN, which excludes it from `priors_near_freq` queries.
    pub fn record(&self, callsign: &str, dt_s: f64, at: SystemTime) {
        self.record_with_freq(callsign, dt_s, f64::NAN, at);
    }

    /// V2 (Session 3): Record a sighting with frequency. Used by the
    /// per-candidate callsign-keyed narrowing hook — sightings recorded
    /// here participate in `priors_near_freq` queries.
    pub fn record_with_freq(&self, callsign: &str, dt_s: f64, freq_hz: f64, at: SystemTime) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        // Evict expired sightings for this callsign.
        let max_age = g.max_age;
        let capacity = g.capacity;
        let entry = g.entries.entry(callsign.to_string()).or_default();
        while let Some(front) = entry.front() {
            match at.duration_since(front.at) {
                Ok(age) if age > max_age => {
                    entry.pop_front();
                }
                _ => break,
            }
        }
        if entry.len() == capacity {
            entry.pop_front();
        }
        entry.push_back(DtSighting { at, dt_s, freq_hz });
    }

    /// Number of callsigns currently tracked. Test/diagnostic helper.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.entries.len()).unwrap_or(0)
    }

    /// `true` if no callsigns are currently tracked.
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .map(|g| g.entries.is_empty())
            .unwrap_or(true)
    }
}

fn compute_prior_from_dts(dts: &mut [f64]) -> DtPrior {
    dts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = dts.len();
    let median_dt = if n % 2 == 1 {
        dts[n / 2]
    } else {
        (dts[n / 2 - 1] + dts[n / 2]) / 2.0
    };
    // Nearest-rank quartile (fine for n ≤ 10).
    let q1 = dts[n / 4];
    let q3 = dts[(3 * n) / 4];
    let iqr = (q3 - q1).abs();
    DtPrior {
        median_dt,
        iqr,
        sighting_count: n,
    }
}

impl DtPriorLookup for InMemoryDtHistory {
    fn prior(&self, callsign: &str) -> Option<DtPrior> {
        let g = self.inner.lock().ok()?;
        let entries = g.entries.get(callsign)?;
        if entries.len() < g.min_sightings {
            return None;
        }
        let mut dts: Vec<f64> = entries.iter().map(|s| s.dt_s).collect();
        Some(compute_prior_from_dts(&mut dts))
    }

    /// V2: walk every tracked callsign; for each, check if ANY of its
    /// (non-NaN) sightings is within `freq_window_hz` of
    /// `target_freq_hz`. If yes AND that callsign meets `min_sightings`,
    /// compute its DtPrior and add it to the result. O(N*K) where
    /// N = tracked callsigns, K = avg sightings/callsign (≤ capacity).
    /// On a chrono-replay session with ~50 callsigns and capacity 10,
    /// this is ≤ 500 floats per residual candidate — negligible.
    fn priors_near_freq(&self, target_freq_hz: f64, freq_window_hz: f64) -> Vec<DtPrior> {
        let Ok(g) = self.inner.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entries in g.entries.values() {
            if entries.len() < g.min_sightings {
                continue;
            }
            let near = entries.iter().any(|s| {
                s.freq_hz.is_finite() && (s.freq_hz - target_freq_hz).abs() <= freq_window_hz
            });
            if !near {
                continue;
            }
            let mut dts: Vec<f64> = entries.iter().map(|s| s.dt_s).collect();
            out.push(compute_prior_from_dts(&mut dts));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn empty_returns_none() {
        let h = InMemoryDtHistory::default();
        assert!(h.prior("K1ABC").is_none());
        assert!(h.is_empty());
    }

    #[test]
    fn single_sighting_below_gate() {
        let h = InMemoryDtHistory::default();
        h.record("K1ABC", 0.5, t0());
        assert!(h.prior("K1ABC").is_none());
    }

    #[test]
    fn three_sightings_yield_median_and_iqr() {
        let h = InMemoryDtHistory::default();
        let t = t0();
        h.record("K1ABC", 0.2, t);
        h.record("K1ABC", 0.4, t + Duration::from_secs(15));
        h.record("K1ABC", 0.3, t + Duration::from_secs(30));
        let p = h.prior("K1ABC").expect("three sightings");
        assert!((p.median_dt - 0.3).abs() < 1e-9);
        assert_eq!(p.sighting_count, 3);
    }

    #[test]
    fn capacity_drops_oldest() {
        let h = InMemoryDtHistory::new(3, Duration::from_secs(3600), 2);
        let t = t0();
        for i in 0..5 {
            h.record("K1ABC", i as f64 * 0.1, t + Duration::from_secs(i * 10));
        }
        let p = h.prior("K1ABC").unwrap();
        // Only last 3 (0.2, 0.3, 0.4) survive → median = 0.3
        assert_eq!(p.sighting_count, 3);
        assert!((p.median_dt - 0.3).abs() < 1e-9);
    }

    #[test]
    fn evicts_expired_on_record() {
        let h = InMemoryDtHistory::new(10, Duration::from_secs(60), 2);
        let t = t0();
        h.record("K1ABC", 0.2, t);
        h.record("K1ABC", 0.3, t + Duration::from_secs(5));
        // Insert a new sighting well past eviction window; old ones drop.
        h.record("K1ABC", 0.9, t + Duration::from_secs(200));
        let p = h.prior("K1ABC");
        // Only the third sighting survives → below min_sightings gate
        assert!(p.is_none());
    }

    #[test]
    fn priors_near_freq_returns_only_nearby_callsigns() {
        // V2 (Session 3): per-candidate callsign-keyed sync test. Three
        // callsigns recorded at distinct frequencies; query at one and
        // verify only the matching callsign's prior comes back.
        let h = InMemoryDtHistory::default();
        let t = t0();
        // K1ABC near 1000 Hz, two sightings → prior should fire.
        h.record_with_freq("K1ABC", 0.2, 1000.0, t);
        h.record_with_freq("K1ABC", 0.4, 1002.0, t + Duration::from_secs(15));
        // K2XYZ near 1500 Hz, two sightings → prior fires too, but far away.
        h.record_with_freq("K2XYZ", -0.3, 1500.0, t);
        h.record_with_freq("K2XYZ", -0.5, 1498.0, t + Duration::from_secs(15));
        // K3LMN near 1003 Hz, only one sighting → below gate, not returned.
        h.record_with_freq("K3LMN", 1.2, 1003.0, t);

        let near = h.priors_near_freq(1001.0, 25.0);
        assert_eq!(near.len(), 1, "only K1ABC should fire");
        assert!((near[0].median_dt - 0.3).abs() < 1e-9);

        let none = h.priors_near_freq(2000.0, 25.0);
        assert!(none.is_empty(), "no callsign near 2000 Hz");

        let both = h.priors_near_freq(1250.0, 260.0);
        assert_eq!(both.len(), 2, "wide window catches both K1ABC and K2XYZ");
    }

    #[test]
    fn legacy_record_excluded_from_freq_lookup() {
        // Sightings recorded without freq (via `record`) carry NaN and
        // are excluded from priors_near_freq queries — this preserves
        // backward-compat for callers that only use the V1 prior() path.
        let h = InMemoryDtHistory::default();
        let t = t0();
        h.record("K1ABC", 0.2, t);
        h.record("K1ABC", 0.4, t + Duration::from_secs(15));

        // priors_near_freq returns empty (no finite-freq sightings).
        let near = h.priors_near_freq(1000.0, 25.0);
        assert!(near.is_empty(), "legacy-recorded sightings excluded");

        // But the V1 prior() path still finds the callsign.
        assert!(h.prior("K1ABC").is_some());
    }
}
