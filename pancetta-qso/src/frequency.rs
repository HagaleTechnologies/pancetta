//! Smart frequency allocator with spectral and occupancy awareness.
//!
//! Scores candidate TX frequencies based on noise floor, decoded activity,
//! neighbor interference, center bias, and DX proximity. All criteria are
//! soft-scored — no hard gates. On a crowded band the best candidate may
//! score low, but it's still the best available.

use serde::{Deserialize, Serialize};

/// Configuration for the smart frequency allocator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyAllocatorConfig {
    /// How many recent decode cycles to consider for occupancy (default 4, ~60s).
    pub decode_history_cycles: usize,
    /// Center of passband preference in Hz (default 1500).
    pub center_bias_hz: f64,
    /// Minimum preferred offset from DX station in Hz (default 50).
    pub dx_proximity_min_hz: f64,
    /// Maximum preferred offset from DX station in Hz (default 200).
    pub dx_proximity_max_hz: f64,
    /// Minimum separation between own QSO frequencies in Hz (default 75).
    pub min_separation_hz: f64,
    /// Avoid strong signals within this range in Hz (default 100).
    pub neighbor_guard_hz: f64,
    /// Candidate step size in Hz (default 25 — quarter of FT8 bandwidth).
    pub step_hz: f64,
    /// Allocation range (min, max) in Hz.
    pub range: (f64, f64),
}

impl Default for FrequencyAllocatorConfig {
    fn default() -> Self {
        Self {
            decode_history_cycles: 4,
            center_bias_hz: 1500.0,
            dx_proximity_min_hz: 50.0,
            dx_proximity_max_hz: 200.0,
            min_separation_hz: 75.0,
            neighbor_guard_hz: 100.0,
            step_hz: 25.0,
            range: (200.0, 2800.0),
        }
    }
}

/// A snapshot of spectral power across the passband.
#[derive(Debug, Clone)]
pub struct SpectralSnapshot {
    /// Power values per frequency bin (linear, normalized 0.0–1.0).
    pub power_bins: Vec<f32>,
    /// Frequency of the first bin in Hz.
    pub freq_min_hz: f64,
    /// Frequency of the last bin in Hz.
    pub freq_max_hz: f64,
}

impl SpectralSnapshot {
    /// Get the average power near a given frequency offset.
    pub fn power_near(&self, offset_hz: f64, radius_hz: f64) -> f32 {
        if self.power_bins.is_empty() {
            return 0.0;
        }
        let bin_width = (self.freq_max_hz - self.freq_min_hz) / self.power_bins.len() as f64;
        if bin_width <= 0.0 {
            return 0.0;
        }
        let center_bin = ((offset_hz - self.freq_min_hz) / bin_width) as isize;
        let radius_bins = (radius_hz / bin_width).ceil() as isize;
        let lo = (center_bin - radius_bins).max(0) as usize;
        let hi = (center_bin + radius_bins).max(0) as usize;
        let hi = hi.min(self.power_bins.len() - 1);
        if lo > hi {
            return 0.0;
        }
        let sum: f32 = self.power_bins[lo..=hi].iter().sum();
        sum / (hi - lo + 1) as f32
    }

    /// Get the peak power near a given frequency offset.
    pub fn peak_near(&self, offset_hz: f64, radius_hz: f64) -> f32 {
        if self.power_bins.is_empty() {
            return 0.0;
        }
        let bin_width = (self.freq_max_hz - self.freq_min_hz) / self.power_bins.len() as f64;
        if bin_width <= 0.0 {
            return 0.0;
        }
        let center_bin = ((offset_hz - self.freq_min_hz) / bin_width) as isize;
        let radius_bins = (radius_hz / bin_width).ceil() as isize;
        let lo = (center_bin - radius_bins).max(0) as usize;
        let hi = (center_bin + radius_bins).max(0) as usize;
        let hi = hi.min(self.power_bins.len() - 1);
        if lo > hi {
            return 0.0;
        }
        self.power_bins[lo..=hi]
            .iter()
            .copied()
            .fold(0.0f32, f32::max)
    }
}

/// Which 15-second time slot a decode occurred in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSlot {
    First,
    Second,
}

impl TimeSlot {
    /// The other slot. `First <-> Second`. Idempotent under double-flip.
    ///
    /// Mirrors `pancetta_core::slot::SlotParity::opposite` — used to map a
    /// DX station's own `SlotParity` (Even/Odd) into the `TimeSlot` we would
    /// actually transmit in (the opposite slot), for parity-aware scoring
    /// (FQ-F8).
    pub fn opposite(self) -> TimeSlot {
        match self {
            TimeSlot::First => TimeSlot::Second,
            TimeSlot::Second => TimeSlot::First,
        }
    }
}

/// A record of one decoded signal for occupancy tracking.
#[derive(Debug, Clone)]
pub struct DecodeRecord {
    pub frequency_hz: f64,
    pub time_slot: TimeSlot,
}

/// Rolling buffer of recent decode activity across multiple cycles.
#[derive(Debug, Clone)]
pub struct DecodeHistory {
    max_cycles: usize,
    /// Each entry is one cycle's worth of decode records.
    cycles: std::collections::VecDeque<Vec<DecodeRecord>>,
}

impl DecodeHistory {
    pub fn new(max_cycles: usize) -> Self {
        Self {
            max_cycles,
            cycles: std::collections::VecDeque::new(),
        }
    }

    /// Push a new cycle of decode records. Drops oldest if over capacity.
    pub fn push_cycle(&mut self, records: Vec<DecodeRecord>) {
        if self.cycles.len() >= self.max_cycles {
            self.cycles.pop_front();
        }
        self.cycles.push_back(records);
    }

    /// Count decodes near a frequency across all retained cycles.
    pub fn activity_near(&self, offset_hz: f64, radius_hz: f64) -> usize {
        self.cycles
            .iter()
            .flat_map(|c| c.iter())
            .filter(|r| (r.frequency_hz - offset_hz).abs() <= radius_hz)
            .count()
    }

    /// Count decodes near a frequency in a specific time slot.
    pub fn activity_near_in_slot(&self, offset_hz: f64, radius_hz: f64, slot: TimeSlot) -> usize {
        self.cycles
            .iter()
            .flat_map(|c| c.iter())
            .filter(|r| r.time_slot == slot && (r.frequency_hz - offset_hz).abs() <= radius_hz)
            .count()
    }

    /// Check if a frequency is clear in both time slots.
    pub fn is_clear_both_slots(&self, offset_hz: f64, radius_hz: f64) -> bool {
        self.activity_near(offset_hz, radius_hz) == 0
    }

    /// How many decode cycles are currently retained (0..=`max_cycles`).
    /// Used to distinguish a freshly-started/just-hopped history (thin data,
    /// don't trust rankings yet) from a full rolling window.
    pub fn cycles_recorded(&self) -> usize {
        self.cycles.len()
    }
}

/// A scored frequency candidate.
#[derive(Debug, Clone)]
pub struct FrequencyCandidate {
    pub offset_hz: f64,
    pub score: f64,
    pub clear_both_slots: bool,
    /// No decode activity within 50 Hz in the First (even) slot across retained history.
    pub clear_first: bool,
    /// No decode activity within 50 Hz in the Second (odd) slot across retained history.
    pub clear_second: bool,
    pub noise_floor: f32,
}

/// A ranked snapshot of band openness for the TX-placement instrument
/// (see `docs/superpowers/specs/2026-07-03-tui-redesign-design.md` §2).
///
/// Produced by [`crate::autonomous::AutonomousOperator::placement_snapshot`]
/// as a pure read of the SAME allocator/history state the autonomous
/// operator uses to make real TX-frequency decisions — never a separate
/// computation (single-scorer invariant).
#[derive(Debug, Clone)]
pub struct PlacementSnapshot {
    /// Top-N ranked candidates, sorted by score descending.
    pub slices: Vec<FrequencyCandidate>,
    /// Per-bin openness code across the full allocation range:
    /// 0=busy-both, 1=second-only-clear, 2=first-only-clear, 3=clear-both.
    pub openness: Vec<u8>,
    /// Bin width in Hz (matches the allocator's `step_hz`).
    pub bin_hz: f64,
    /// Allocation range (min, max) in Hz.
    pub range: (f64, f64),
}

/// Stateless frequency allocator. Given spectral + decode data, returns ranked candidates.
pub struct SmartFrequencyAllocator {
    config: FrequencyAllocatorConfig,
}

impl SmartFrequencyAllocator {
    pub fn new(config: FrequencyAllocatorConfig) -> Self {
        Self { config }
    }

    /// Access the allocator's configuration (range, step size, etc.) — used
    /// by the TX-placement snapshot to bin openness at the same resolution
    /// the allocator scores candidates at.
    pub fn config(&self) -> &FrequencyAllocatorConfig {
        &self.config
    }

    /// Score and rank all candidate frequencies.
    ///
    /// - `spectral`: current passband power snapshot
    /// - `history`: recent decode activity
    /// - `own_frequencies`: offsets in use by our active QSOs
    /// - `dx_target_hz`: optional offset of the DX station we're calling
    ///
    /// Slot-blind: does not know which `TimeSlot` a candidate would
    /// actually transmit in, so occupancy scoring can't distinguish "clear
    /// in the slot that matters for our TX" from "clear in the DX's/
    /// opposite slot" (FQ-F8). Delegates to
    /// [`Self::rank_candidates_with_parity`] with `target_parity: None`,
    /// which is exactly today's behavior — kept as a stable, unbroken
    /// entry point for callers that don't (yet) know their TX parity at
    /// scoring time (e.g. the self-CQ path, where either parity may be
    /// chosen).
    pub fn rank_candidates(
        &self,
        spectral: &SpectralSnapshot,
        history: &DecodeHistory,
        own_frequencies: &[f64],
        dx_target_hz: Option<f64>,
    ) -> Vec<FrequencyCandidate> {
        self.rank_candidates_with_parity(spectral, history, own_frequencies, dx_target_hz, None)
    }

    /// Same as [`Self::rank_candidates`], but additionally takes the
    /// `TimeSlot` parity the candidate would actually transmit in, when
    /// knowable (FQ-F8). For a pounce, this is the opposite of the DX
    /// station's own observed slot parity (mirroring the `tx_parity =
    /// slot_parity.opposite()` convention used when latching a response's
    /// TX parity elsewhere in this crate). When `None` (e.g. a self-CQ,
    /// where either slot may end up being chosen), scoring degrades
    /// gracefully to the slot-blind behavior of [`Self::rank_candidates`].
    pub fn rank_candidates_with_parity(
        &self,
        spectral: &SpectralSnapshot,
        history: &DecodeHistory,
        own_frequencies: &[f64],
        dx_target_hz: Option<f64>,
        target_parity: Option<TimeSlot>,
    ) -> Vec<FrequencyCandidate> {
        let (min_f, max_f) = self.config.range;
        let step = self.config.step_hz;
        let mut candidates = Vec::new();

        let mut freq = min_f;
        while freq <= max_f {
            let score = self.score_candidate(
                freq,
                spectral,
                history,
                own_frequencies,
                dx_target_hz,
                target_parity,
            );
            let noise = spectral.power_near(freq, 25.0);
            let clear = history.is_clear_both_slots(freq, 50.0);
            let clear_first = history.activity_near_in_slot(freq, 50.0, TimeSlot::First) == 0;
            let clear_second = history.activity_near_in_slot(freq, 50.0, TimeSlot::Second) == 0;

            candidates.push(FrequencyCandidate {
                offset_hz: freq,
                score,
                clear_both_slots: clear,
                clear_first,
                clear_second,
                noise_floor: noise,
            });

            freq += step;
        }

        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates
    }

    fn score_candidate(
        &self,
        freq: f64,
        spectral: &SpectralSnapshot,
        history: &DecodeHistory,
        own_frequencies: &[f64],
        dx_target_hz: Option<f64>,
        target_parity: Option<TimeSlot>,
    ) -> f64 {
        let mut score = 0.0;

        // 1. Clear in both slots (strong positive)
        let clear_both = history.is_clear_both_slots(freq, 50.0);
        if clear_both {
            score += 30.0;
        } else {
            match target_parity {
                Some(target) => {
                    // FQ-F8: we now know which slot we'd actually transmit
                    // in, so weight clearness in THAT slot much more
                    // heavily than the opposite/DX slot — a decode in the
                    // DX's own listening window is irrelevant to whether
                    // OUR transmission collides with something.
                    let target_activity = history.activity_near_in_slot(freq, 50.0, target);
                    if target_activity == 0 {
                        // Our TX slot is clear; activity in the opposite
                        // slot poses no collision risk to us, so it earns
                        // only a token deduction — kept a hair below the
                        // genuinely-clear-both bonus (30) so that outcome
                        // still ranks best.
                        let opposite_activity =
                            history.activity_near_in_slot(freq, 50.0, target.opposite());
                        score += (28.0 - opposite_activity as f64 * 1.0).max(20.0);
                    } else {
                        // Our TX slot itself has decode activity — real
                        // collision risk, regardless of what the
                        // opposite/DX slot looks like. Floors at 0 for a
                        // heavily busy target slot.
                        score += (12.0 - target_activity as f64 * 4.0).max(0.0);
                    }
                }
                None => {
                    // Slot-blind fallback (self-CQ path, or any caller
                    // that doesn't know its TX parity yet): identical to
                    // the pre-FQ-F8 formula.
                    // FQ-F7: floor this term at 0, not 15 — `15.0_f64.max(...)`
                    // previously clamped UP, so any bin with activity >= 2 scored
                    // an identical flat 15 regardless of how busy it actually was,
                    // defeating the whole point of penalizing busy bins.
                    let activity = history.activity_near(freq, 50.0);
                    score += (25.0 - activity as f64 * 5.0).max(0.0);
                }
            }
        }

        // 2. Low noise floor (lower = better, scale 0–20)
        let noise = spectral.power_near(freq, 25.0);
        score += 20.0 * (1.0 - noise as f64).max(0.0);

        // 3. No noisy neighbors (peak within guard band, scale 0–15)
        let peak = spectral.peak_near(freq, self.config.neighbor_guard_hz);
        score += 15.0 * (1.0 - peak as f64).max(0.0);

        // 4. No recent decode activity (scale 0–10)
        let recent = history.activity_near(freq, 50.0);
        score += (10.0 - recent as f64 * 2.5).max(0.0);

        // 5. Center bias (scale 0–10)
        let center_dist = (freq - self.config.center_bias_hz).abs();
        let max_dist = (self.config.range.1 - self.config.range.0) / 2.0;
        score += 10.0 * (1.0 - center_dist / max_dist).max(0.0);

        // 6. DX proximity bias (scale 0–8)
        if let Some(dx_freq) = dx_target_hz {
            let dist = (freq - dx_freq).abs();
            if dist >= self.config.dx_proximity_min_hz && dist <= self.config.dx_proximity_max_hz {
                // Sweet spot: nearby but not on top
                score += 8.0;
            } else if dist < self.config.dx_proximity_min_hz && dist > 0.0 {
                // Too close — usable but not ideal
                score += 4.0;
            } else if dist == 0.0 {
                // Same frequency — last resort within proximity range
                score += 2.0;
            }
            // Beyond dx_proximity_max_hz: no bonus (0)
        }

        // 7. Own-frequency separation (strong penalty if violated)
        let min_own_dist = own_frequencies
            .iter()
            .map(|&f| (f - freq).abs())
            .fold(f64::MAX, f64::min);
        if min_own_dist < self.config.min_separation_hz {
            score -= 50.0; // Effectively eliminates this candidate
        }

        score
    }
}

#[cfg(test)]
// rationale: test-only builder structs assigned field-by-field after
// default(); sequential assignment reads clearer than a struct-update splat.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn empty_history() -> DecodeHistory {
        DecodeHistory::new(4)
    }

    fn empty_spectral() -> SpectralSnapshot {
        // 140 bins covering 200–2800 Hz at ~19 Hz spacing
        SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        }
    }

    #[test]
    fn cycles_recorded_tracks_pushes_up_to_capacity() {
        let mut history = DecodeHistory::new(4);
        assert_eq!(history.cycles_recorded(), 0);

        history.push_cycle(vec![]);
        assert_eq!(history.cycles_recorded(), 1);

        for _ in 0..5 {
            history.push_cycle(vec![]);
        }
        // Capped at max_cycles even after more pushes than capacity.
        assert_eq!(history.cycles_recorded(), 4);
    }

    #[test]
    fn test_empty_band_picks_center() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates = allocator.rank_candidates(
            &empty_spectral(),
            &empty_history(),
            &[],  // no own frequencies
            None, // no DX target
        );
        assert!(!candidates.is_empty());
        // Best candidate should be near center (1500 Hz)
        let best = &candidates[0];
        assert!(
            (best.offset_hz - 1500.0).abs() < 200.0,
            "Expected near center, got {}",
            best.offset_hz
        );
    }

    #[test]
    fn test_avoids_noisy_frequency() {
        // Make center bins (around 1500 Hz) noisy
        let mut spectral = empty_spectral();
        // bin_width = (2800 - 200) / 140 = ~18.57 Hz/bin
        // index for 1500 Hz = (1500 - 200) / 18.57 ≈ 70
        let center_bin = 70usize;
        let radius = 5usize;
        for i in center_bin.saturating_sub(radius)..=(center_bin + radius).min(139) {
            spectral.power_bins[i] = 0.9;
        }

        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates = allocator.rank_candidates(&spectral, &empty_history(), &[], None);

        assert!(!candidates.is_empty());
        let best = &candidates[0];
        assert!(
            (best.offset_hz - 1500.0).abs() > 100.0,
            "Expected best candidate >100 Hz from noisy center, got {} Hz",
            best.offset_hz
        );
    }

    #[test]
    fn test_avoids_occupied_frequency() {
        // Put decode activity at 1500 Hz in both slots
        let mut history = empty_history();
        history.push_cycle(vec![
            DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::First,
            },
            DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::Second,
            },
        ]);

        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates = allocator.rank_candidates(&empty_spectral(), &history, &[], None);

        assert!(!candidates.is_empty());
        let best = &candidates[0];
        assert!(
            (best.offset_hz - 1500.0).abs() > 50.0,
            "Expected best candidate >50 Hz from occupied 1500 Hz, got {} Hz",
            best.offset_hz
        );
    }

    #[test]
    fn test_prefers_dx_proximity() {
        let dx_target = 1000.0;
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates =
            allocator.rank_candidates(&empty_spectral(), &empty_history(), &[], Some(dx_target));

        assert!(!candidates.is_empty());
        let best = &candidates[0];
        let dist = (best.offset_hz - dx_target).abs();
        assert!(
            (50.0..=200.0).contains(&dist),
            "Expected best candidate 50–200 Hz from DX at {} Hz, got {} Hz (dist {})",
            dx_target,
            best.offset_hz,
            dist
        );
    }

    #[test]
    fn test_avoids_own_frequencies() {
        let own = vec![1500.0];
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates = allocator.rank_candidates(&empty_spectral(), &empty_history(), &own, None);

        assert!(!candidates.is_empty());
        let best = &candidates[0];
        assert!(
            (best.offset_hz - 1500.0).abs() >= 75.0,
            "Expected best candidate ≥75 Hz from own frequency 1500 Hz, got {} Hz",
            best.offset_hz
        );
    }

    #[test]
    fn test_clear_both_slots_preferred() {
        // Activity at 1500 Hz in first slot only
        let mut history = empty_history();
        history.push_cycle(vec![DecodeRecord {
            frequency_hz: 1500.0,
            time_slot: TimeSlot::First,
        }]);

        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates = allocator.rank_candidates(&empty_spectral(), &history, &[], None);

        assert!(!candidates.is_empty());
        let best = &candidates[0];
        assert!(
            best.clear_both_slots,
            "Expected best candidate to have clear_both_slots=true, got offset={} Hz",
            best.offset_hz
        );
    }

    #[test]
    fn test_crowded_band_still_returns_candidates() {
        // Activity at every 100 Hz across the band
        let mut history = empty_history();
        let mut records = Vec::new();
        let mut f = 200.0f64;
        while f <= 2800.0 {
            records.push(DecodeRecord {
                frequency_hz: f,
                time_slot: TimeSlot::First,
            });
            records.push(DecodeRecord {
                frequency_hz: f,
                time_slot: TimeSlot::Second,
            });
            f += 100.0;
        }
        history.push_cycle(records);

        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates = allocator.rank_candidates(&empty_spectral(), &history, &[], None);

        assert!(
            !candidates.is_empty(),
            "Expected candidates even on a crowded band"
        );
    }

    /// FQ-F7 regression: the "partially clear" occupancy term must floor at
    /// 0, not 15 — a heavily-active bin should score far worse than a
    /// lightly-active one, not get clamped up to the same flat partial
    /// credit.
    #[test]
    fn partially_clear_score_floors_at_zero_not_fifteen() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let spectral = empty_spectral();

        // Lightly busy: 1 decode near 1500 Hz, only in the First slot, so
        // the Second slot remains clear -> "partially clear" branch with
        // activity=1: (25 - 1*5).max(0) = 20.
        let mut light_history = empty_history();
        light_history.push_cycle(vec![DecodeRecord {
            frequency_hz: 1500.0,
            time_slot: TimeSlot::First,
        }]);

        // Heavily busy: 10 decodes clustered near 1500 Hz in the First
        // slot -> activity=10, so (25 - 10*5).max(0) should floor at 0,
        // not clamp back up to 15.
        let mut heavy_history = empty_history();
        let mut heavy_records = Vec::new();
        for i in 0..10 {
            heavy_records.push(DecodeRecord {
                frequency_hz: 1500.0 + i as f64, // all within the 50 Hz radius
                time_slot: TimeSlot::First,
            });
        }
        heavy_history.push_cycle(heavy_records);

        let light_score =
            allocator.score_candidate(1500.0, &spectral, &light_history, &[], None, None);
        let heavy_score =
            allocator.score_candidate(1500.0, &spectral, &heavy_history, &[], None, None);

        // Isolate the effect: only the occupancy-related terms (#1 and #4)
        // differ between light and heavy history; everything else (noise,
        // neighbor peak, center bias, DX proximity, own-freq) is identical
        // since both histories are otherwise empty. Term #1 alone should
        // account for a 20-point swing (20 -> 0), not the pre-fix 5-point
        // swing (20 -> 15, since `.max(15.0)` used to clamp the heavy case
        // back up).
        assert!(
            heavy_score < light_score - 15.0,
            "heavy occupancy (score={heavy_score}) should score much worse \
             than light occupancy (score={light_score}); pre-fix the gap \
             was clamped to only ~5 points"
        );
    }

    /// FQ-F8: when the candidate's actual TX slot parity is known, a
    /// candidate that is clear in that slot but occupied in the opposite
    /// (DX's) slot must score meaningfully higher than one that is occupied
    /// in the target slot but clear in the opposite slot — the exact
    /// scenario the old slot-blind code treated identically (both had
    /// `activity_near == 2`, so both got the same flat partial credit).
    #[test]
    fn parity_aware_scoring_favors_clear_target_slot_over_clear_opposite_slot() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let spectral = empty_spectral();
        let target = TimeSlot::First;

        // Scenario A: target slot (First) is clear; opposite (Second) is
        // busy with 2 decodes near 1500 Hz.
        let mut history_target_clear = empty_history();
        history_target_clear.push_cycle(vec![
            DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::Second,
            },
            DecodeRecord {
                frequency_hz: 1501.0,
                time_slot: TimeSlot::Second,
            },
        ]);

        // Scenario B: target slot (First) is busy with 2 decodes near
        // 1500 Hz; opposite (Second) is clear. Total activity_near is the
        // same magnitude as scenario A, so term #4 (recent activity) and
        // every other additive term are identical between A and B — only
        // the parity-aware term #1 branch differs.
        let mut history_target_busy = empty_history();
        history_target_busy.push_cycle(vec![
            DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::First,
            },
            DecodeRecord {
                frequency_hz: 1501.0,
                time_slot: TimeSlot::First,
            },
        ]);

        let score_target_clear = allocator.score_candidate(
            1500.0,
            &spectral,
            &history_target_clear,
            &[],
            None,
            Some(target),
        );
        let score_target_busy = allocator.score_candidate(
            1500.0,
            &spectral,
            &history_target_busy,
            &[],
            None,
            Some(target),
        );

        assert!(
            score_target_clear > score_target_busy + 15.0,
            "clear-in-target-slot (score={score_target_clear}) should score much \
             higher than clear-in-opposite-slot-only (score={score_target_busy}); \
             the old slot-blind code scored these identically"
        );
    }

    /// FQ-F8 regression: when `target_parity` is `None` (the self-CQ path,
    /// which can't commit to a single TX parity at scoring time), the
    /// "partially clear" branch must remain byte-identical to the original
    /// slot-blind formula `(25.0 - activity * 5.0).max(0.0)`. This pins the
    /// full additive score for a known scenario so any accidental change to
    /// the None-path arithmetic (or any other term) trips this test.
    #[test]
    fn target_parity_none_is_byte_identical_to_legacy_slot_blind_scoring() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let spectral = empty_spectral(); // all-zero power bins -> noise=0, peak=0
        let mut history = empty_history();
        // 2 decodes near 1500 Hz (one per slot) -> activity_near == 2,
        // not clear_both.
        history.push_cycle(vec![
            DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::First,
            },
            DecodeRecord {
                frequency_hz: 1500.0,
                time_slot: TimeSlot::Second,
            },
        ]);

        let score = allocator.score_candidate(1500.0, &spectral, &history, &[], None, None);

        // Hand-computed expected total at freq=1500.0 with default config,
        // empty spectral, no own frequencies, no DX target:
        //   #1 partial-clear (activity=2, slot-blind): (25 - 2*5).max(0) = 15
        //   #2 noise floor (noise=0):                   20*(1-0)         = 20
        //   #3 neighbor peak (peak=0):                  15*(1-0)         = 15
        //   #4 recent activity (activity=2):             (10 - 2*2.5).max(0) = 5
        //   #5 center bias (freq == center_bias_hz):     10*(1-0/1300)   = 10
        //   #6 DX proximity: dx_target_hz is None -> 0
        //   #7 own-frequency separation: no own freqs -> 0
        //   total = 15 + 20 + 15 + 5 + 10 = 65
        assert!(
            (score - 65.0).abs() < 1e-9,
            "expected legacy slot-blind total score 65.0, got {score}"
        );
    }

    #[test]
    fn test_decode_history_rolling_buffer() {
        // max-2 buffer: push 3 cycles, oldest should be dropped
        let mut history = DecodeHistory::new(2);

        history.push_cycle(vec![DecodeRecord {
            frequency_hz: 1000.0,
            time_slot: TimeSlot::First,
        }]);
        history.push_cycle(vec![DecodeRecord {
            frequency_hz: 1200.0,
            time_slot: TimeSlot::First,
        }]);
        // After these two pushes activity at 1000 Hz should be 1
        assert_eq!(history.activity_near(1000.0, 10.0), 1);

        // Third push should evict the first cycle (1000 Hz)
        history.push_cycle(vec![DecodeRecord {
            frequency_hz: 1400.0,
            time_slot: TimeSlot::First,
        }]);
        assert_eq!(
            history.activity_near(1000.0, 10.0),
            0,
            "Oldest cycle should have been evicted"
        );
        assert_eq!(history.activity_near(1200.0, 10.0), 1);
        assert_eq!(history.activity_near(1400.0, 10.0), 1);
    }

    #[test]
    fn candidates_carry_per_slot_clear_flags() {
        let alloc = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let spectral = SpectralSnapshot {
            power_bins: vec![0.0; 128],
            freq_min_hz: 200.0,
            freq_max_hz: 3000.0,
        };
        let mut history = DecodeHistory::new(4);
        // 1500 Hz busy in First slot only.
        history.push_cycle(vec![DecodeRecord {
            frequency_hz: 1500.0,
            time_slot: TimeSlot::First,
        }]);
        let cands = alloc.rank_candidates(&spectral, &history, &[], None);
        let at_1500 = cands
            .iter()
            .find(|c| (c.offset_hz - 1500.0).abs() < 1.0)
            .unwrap();
        assert!(!at_1500.clear_first);
        assert!(at_1500.clear_second);
        assert!(!at_1500.clear_both_slots);
        let far = cands
            .iter()
            .find(|c| (c.offset_hz - 700.0).abs() < 1.0)
            .unwrap();
        assert!(far.clear_first && far.clear_second && far.clear_both_slots);
    }

    #[test]
    fn test_spectral_snapshot_power_near() {
        let mut spectral = empty_spectral();
        // bin_width ≈ 18.57 Hz; bin 70 ≈ 1500 Hz
        spectral.power_bins[70] = 0.8;

        // Should detect the high power bin near 1500 Hz
        let power_at_1500 = spectral.power_near(1500.0, 30.0);
        assert!(
            power_at_1500 > 0.1,
            "Expected elevated power near 1500 Hz, got {}",
            power_at_1500
        );

        // Should be quiet far away (e.g. near 600 Hz — bin ~21)
        let power_at_600 = spectral.power_near(600.0, 30.0);
        assert!(
            power_at_600 < 0.01,
            "Expected quiet power near 600 Hz, got {}",
            power_at_600
        );
    }

    /// FQ-F1 regression: the FT8 decoder's waterfall bins start at 0 Hz
    /// (`pancetta-ft8/src/decoder.rs`'s `bin_start = 0usize`, spanning
    /// ~0-3000 Hz), so a `SpectralSnapshot` built from real waterfall data
    /// must be labeled `freq_min_hz: 0.0` to match — NOT 200.0, which used
    /// to mislabel the axis and make `power_near`/`peak_near` read a bin
    /// ~100-200 Hz away from the frequency actually being scored. This
    /// proves the bin-index math in this file is correct for the corrected
    /// (0-3000 Hz) axis: a synthetic peak placed at a known real-world
    /// offset must be read back from exactly that offset, not a shifted one.
    #[test]
    fn power_near_reads_correct_bin_on_corrected_zero_hz_axis() {
        let num_bins = 150; // matches the ~150-bin waterfall used in production
        let mut power_bins = vec![0.0f32; num_bins];
        let freq_min_hz = 0.0;
        let freq_max_hz = 3000.0;
        let bin_width = (freq_max_hz - freq_min_hz) / num_bins as f64;

        // Place a synthetic peak at a known real-world offset: 1500 Hz.
        let peak_offset_hz = 1500.0;
        let peak_bin = (peak_offset_hz / bin_width) as usize;
        power_bins[peak_bin] = 0.9;

        let spectral = SpectralSnapshot {
            power_bins,
            freq_min_hz,
            freq_max_hz,
        };

        // Reading AT the real offset must see the peak (radius 0 pins the
        // lookup to exactly the center bin, avoiding averaging dilution).
        let at_peak = spectral.power_near(peak_offset_hz, 0.0);
        assert!(
            (at_peak - 0.9).abs() < 1e-6,
            "Expected to find the peak at its real 1500 Hz offset, got {}",
            at_peak
        );
        let peak_val = spectral.peak_near(peak_offset_hz, 20.0);
        assert!(
            (peak_val - 0.9).abs() < 1e-6,
            "Expected peak_near to read the exact peak value at 1500 Hz, got {}",
            peak_val
        );

        // Reading at the OLD (mislabeled) 200-Hz-shifted interpretation of
        // this offset (i.e. what a caller using freq_min_hz=200.0 would
        // have actually landed on, roughly 200 Hz away) must NOT see it —
        // this is what the bug looked like: scoring code asking for 1500 Hz
        // actually read data belonging to a different real frequency.
        let shifted = spectral.power_near(peak_offset_hz - 200.0, 20.0);
        assert!(
            shifted < 0.1,
            "Expected no peak 200 Hz away from the real peak location, got {}",
            shifted
        );
    }
}
