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
    pub fn activity_near_in_slot(
        &self,
        offset_hz: f64,
        radius_hz: f64,
        slot: TimeSlot,
    ) -> usize {
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
}

/// A scored frequency candidate.
#[derive(Debug, Clone)]
pub struct FrequencyCandidate {
    pub offset_hz: f64,
    pub score: f64,
    pub clear_both_slots: bool,
    pub noise_floor: f32,
}

/// Stateless frequency allocator. Given spectral + decode data, returns ranked candidates.
pub struct SmartFrequencyAllocator {
    config: FrequencyAllocatorConfig,
}

impl SmartFrequencyAllocator {
    pub fn new(config: FrequencyAllocatorConfig) -> Self {
        Self { config }
    }

    /// Score and rank all candidate frequencies.
    ///
    /// - `spectral`: current passband power snapshot
    /// - `history`: recent decode activity
    /// - `own_frequencies`: offsets in use by our active QSOs
    /// - `dx_target_hz`: optional offset of the DX station we're calling
    pub fn rank_candidates(
        &self,
        spectral: &SpectralSnapshot,
        history: &DecodeHistory,
        own_frequencies: &[f64],
        dx_target_hz: Option<f64>,
    ) -> Vec<FrequencyCandidate> {
        let (min_f, max_f) = self.config.range;
        let step = self.config.step_hz;
        let mut candidates = Vec::new();

        let mut freq = min_f;
        while freq <= max_f {
            let score = self.score_candidate(freq, spectral, history, own_frequencies, dx_target_hz);
            let noise = spectral.power_near(freq, 25.0);
            let clear = history.is_clear_both_slots(freq, 50.0);

            candidates.push(FrequencyCandidate {
                offset_hz: freq,
                score,
                clear_both_slots: clear,
                noise_floor: noise,
            });

            freq += step;
        }

        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        candidates
    }

    fn score_candidate(
        &self,
        freq: f64,
        spectral: &SpectralSnapshot,
        history: &DecodeHistory,
        own_frequencies: &[f64],
        dx_target_hz: Option<f64>,
    ) -> f64 {
        let mut score = 0.0;

        // 1. Clear in both slots (strong positive)
        let clear_both = history.is_clear_both_slots(freq, 50.0);
        if clear_both {
            score += 30.0;
        } else {
            // Partially clear (only our TX slot is free) gets some credit
            // We don't know our slot here — caller should filter if needed
            let activity = history.activity_near(freq, 50.0);
            score += 15.0_f64.max(25.0 - activity as f64 * 5.0);
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
            if dist >= self.config.dx_proximity_min_hz
                && dist <= self.config.dx_proximity_max_hz
            {
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
    fn test_empty_band_picks_center() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates = allocator.rank_candidates(
            &empty_spectral(),
            &empty_history(),
            &[],    // no own frequencies
            None,   // no DX target
        );
        assert!(!candidates.is_empty());
        // Best candidate should be near center (1500 Hz)
        let best = &candidates[0];
        assert!((best.offset_hz - 1500.0).abs() < 200.0,
            "Expected near center, got {}", best.offset_hz);
    }
}
