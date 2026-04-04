# Phase 3: Multi-Stream TX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable pancetta to transmit N simultaneous FT8 signals at different audio frequencies, allowing parallel QSOs with smart frequency selection.

**Architecture:** New `SmartFrequencyAllocator` module scores candidate frequencies using spectral data and decode history. The existing `AutonomousOperator` gets multi-slot decision logic with a configurable threshold. All existing TX infrastructure (`modulate_multi_tx`, `MultiTransmitRequest`, coordinator bundling) is reused unchanged.

**Tech Stack:** Rust, pancetta-qso crate, pancetta-ft8 modulator, pancetta-config, tokio async

---

## File Structure

| File | Role |
|------|------|
| `pancetta-qso/src/frequency.rs` | **New** — Smart frequency allocator with spectral-aware scoring |
| `pancetta-qso/src/lib.rs` | **Modify** — Add `pub mod frequency` |
| `pancetta-qso/src/autonomous.rs` | **Modify** — Multi-slot decision logic, decode history buffer, wire smart allocator |
| `pancetta-config/src/autonomous.rs` | **Modify** — New config fields (`min_multi_slot_score`, frequency sub-section) |
| `pancetta/src/coordinator.rs` | **Modify** — Route WaterfallData to autonomous operator |
| `pancetta/tests/loopback_qso.rs` | **Modify** — New multi-stream loopback test |

---

### Task 1: Smart Frequency Allocator — Core Types and Empty Band Scoring

**Files:**
- Create: `pancetta-qso/src/frequency.rs`
- Modify: `pancetta-qso/src/lib.rs`

- [ ] **Step 1: Write the failing test for empty-band center selection**

In `pancetta-qso/src/frequency.rs`:

```rust
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
```

- [ ] **Step 2: Write the module skeleton with types**

In `pancetta-qso/src/frequency.rs`:

```rust
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
        let hi = ((center_bin + radius_bins) as usize).min(self.power_bins.len() - 1);
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
        let hi = ((center_bin + radius_bins) as usize).min(self.power_bins.len() - 1);
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
        score += 10.0_f64.max(10.0 - recent as f64 * 2.5).max(0.0);

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
```

- [ ] **Step 3: Add module declaration**

In `pancetta-qso/src/lib.rs`, add after `pub mod autonomous;`:

```rust
pub mod frequency;
```

And in the re-exports section, add:

```rust
pub use crate::frequency::*;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-qso test_empty_band_picks_center`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/frequency.rs pancetta-qso/src/lib.rs
git commit -m "feat: add SmartFrequencyAllocator with core types and center-bias scoring"
```

---

### Task 2: Smart Frequency Allocator — Scoring Tests

**Files:**
- Modify: `pancetta-qso/src/frequency.rs`

- [ ] **Step 1: Write tests for all scoring criteria**

Add to the `tests` module in `pancetta-qso/src/frequency.rs`:

```rust
    #[test]
    fn test_avoids_noisy_frequency() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let mut spectral = empty_spectral();
        // Make center (bin 70 ≈ 1500 Hz) noisy
        let center_bin = 70;
        for i in (center_bin - 3)..=(center_bin + 3) {
            spectral.power_bins[i] = 0.9;
        }

        let candidates = allocator.rank_candidates(
            &spectral,
            &empty_history(),
            &[],
            None,
        );
        // Best candidate should NOT be near the noisy center
        let best = &candidates[0];
        assert!(
            (best.offset_hz - 1500.0).abs() > 100.0,
            "Should avoid noisy center, got {}", best.offset_hz
        );
    }

    #[test]
    fn test_avoids_occupied_frequency() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let mut history = DecodeHistory::new(4);
        // Put activity at center
        history.push_cycle(vec![
            DecodeRecord { frequency_hz: 1500.0, time_slot: TimeSlot::First },
            DecodeRecord { frequency_hz: 1500.0, time_slot: TimeSlot::Second },
        ]);

        let candidates = allocator.rank_candidates(
            &empty_spectral(),
            &history,
            &[],
            None,
        );
        let best = &candidates[0];
        assert!(
            (best.offset_hz - 1500.0).abs() > 50.0,
            "Should avoid occupied center, got {}", best.offset_hz
        );
    }

    #[test]
    fn test_prefers_dx_proximity() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let candidates = allocator.rank_candidates(
            &empty_spectral(),
            &empty_history(),
            &[],
            Some(1000.0), // DX station at 1000 Hz
        );
        let best = &candidates[0];
        let dist = (best.offset_hz - 1000.0).abs();
        // Should be within 50–200 Hz of DX station
        assert!(
            dist >= 50.0 && dist <= 200.0,
            "Should be near DX at 1000 Hz (50-200 Hz away), got {} Hz (dist {})",
            best.offset_hz, dist
        );
    }

    #[test]
    fn test_avoids_own_frequencies() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let own = vec![1500.0];
        let candidates = allocator.rank_candidates(
            &empty_spectral(),
            &empty_history(),
            &own,
            None,
        );
        let best = &candidates[0];
        assert!(
            (best.offset_hz - 1500.0).abs() >= 75.0,
            "Should avoid own frequency at 1500, got {}", best.offset_hz
        );
    }

    #[test]
    fn test_clear_both_slots_preferred() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let mut history = DecodeHistory::new(4);
        // Activity at 1500 in first slot only
        history.push_cycle(vec![
            DecodeRecord { frequency_hz: 1500.0, time_slot: TimeSlot::First },
        ]);

        let candidates = allocator.rank_candidates(
            &empty_spectral(),
            &history,
            &[],
            None,
        );
        // Best should be clear in both slots (not 1500)
        assert!(candidates[0].clear_both_slots);
    }

    #[test]
    fn test_crowded_band_still_returns_candidates() {
        let allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
        let mut history = DecodeHistory::new(4);
        // Activity everywhere at 100 Hz intervals
        let mut records = Vec::new();
        let mut f = 200.0;
        while f <= 2800.0 {
            records.push(DecodeRecord { frequency_hz: f, time_slot: TimeSlot::First });
            records.push(DecodeRecord { frequency_hz: f, time_slot: TimeSlot::Second });
            f += 100.0;
        }
        history.push_cycle(records);

        let candidates = allocator.rank_candidates(
            &empty_spectral(),
            &history,
            &[],
            None,
        );
        // Should still return candidates even though band is full
        assert!(!candidates.is_empty());
    }

    #[test]
    fn test_decode_history_rolling_buffer() {
        let mut history = DecodeHistory::new(2);
        history.push_cycle(vec![
            DecodeRecord { frequency_hz: 1000.0, time_slot: TimeSlot::First },
        ]);
        assert_eq!(history.activity_near(1000.0, 50.0), 1);

        history.push_cycle(vec![
            DecodeRecord { frequency_hz: 1000.0, time_slot: TimeSlot::Second },
        ]);
        assert_eq!(history.activity_near(1000.0, 50.0), 2);

        // Third cycle should drop the first
        history.push_cycle(vec![]);
        assert_eq!(history.activity_near(1000.0, 50.0), 1);
    }

    #[test]
    fn test_spectral_snapshot_power_near() {
        let mut spectral = empty_spectral();
        // Set a single bin near 1500 Hz to high power
        let bin_width = (2800.0 - 200.0) / 140.0; // ~18.57 Hz
        let bin_index = ((1500.0 - 200.0) / bin_width) as usize;
        spectral.power_bins[bin_index] = 0.8;

        let power = spectral.power_near(1500.0, 25.0);
        assert!(power > 0.1, "Should detect power near 1500 Hz, got {}", power);

        let far_power = spectral.power_near(500.0, 25.0);
        assert!(far_power < 0.01, "Should be quiet at 500 Hz, got {}", far_power);
    }
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso frequency`
Expected: All 8 tests PASS

- [ ] **Step 3: Commit**

```bash
git add pancetta-qso/src/frequency.rs
git commit -m "test: comprehensive scoring tests for SmartFrequencyAllocator"
```

---

### Task 3: Configuration — Multi-Slot and Frequency Settings

**Files:**
- Modify: `pancetta-config/src/autonomous.rs`

- [ ] **Step 1: Add `min_multi_slot_score` and frequency config to `AutonomousConfig`**

In `pancetta-config/src/autonomous.rs`, add a new struct before `AutonomousConfig`:

```rust
/// Frequency allocator configuration for multi-QSO support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyConfig {
    /// How many recent decode cycles to consider for occupancy.
    pub decode_history_cycles: usize,
    /// Center of passband preference in Hz.
    pub center_bias_hz: f64,
    /// Minimum preferred offset from DX station in Hz.
    pub dx_proximity_min_hz: f64,
    /// Maximum preferred offset from DX station in Hz.
    pub dx_proximity_max_hz: f64,
    /// Minimum separation between own QSO frequencies in Hz.
    pub min_separation_hz: f64,
    /// Avoid strong signals within this range in Hz.
    pub neighbor_guard_hz: f64,
}

impl Default for FrequencyConfig {
    fn default() -> Self {
        Self {
            decode_history_cycles: 4,
            center_bias_hz: 1500.0,
            dx_proximity_min_hz: 50.0,
            dx_proximity_max_hz: 200.0,
            min_separation_hz: 75.0,
            neighbor_guard_hz: 100.0,
        }
    }
}
```

Then add two fields to `AutonomousConfig` struct (after `min_dx_score`):

```rust
    /// Minimum DX score required to open an additional QSO slot (0.0–1.0).
    /// Only applies to second+ concurrent QSOs. First QSO uses min_dx_score.
    pub min_multi_slot_score: f64,
    /// Frequency allocator settings for smart TX offset selection.
    pub frequency: FrequencyConfig,
```

Update `Default for AutonomousConfig`:

```rust
            min_multi_slot_score: 0.7,
            frequency: FrequencyConfig::default(),
```

Update `validate_section`:

```rust
        if !(0.0..=1.0).contains(&self.min_multi_slot_score) {
            return Err(ConfigError::InvalidValue {
                field: "autonomous.min_multi_slot_score".into(),
                value: self.min_multi_slot_score.to_string(),
            });
        }
```

Update `merge_with`:

```rust
        self.min_multi_slot_score = other.min_multi_slot_score;
        self.frequency = other.frequency;
```

- [ ] **Step 2: Add test for new config fields**

Add to existing `tests` module:

```rust
    #[test]
    fn test_multi_slot_score_validation() {
        let mut config = AutonomousConfig::default();
        config.min_multi_slot_score = 0.7;
        assert!(config.validate_section().is_ok());

        config.min_multi_slot_score = 1.5;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_frequency_config_serialization() {
        let config = AutonomousConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: AutonomousConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.min_multi_slot_score, deserialized.min_multi_slot_score);
        assert_eq!(
            config.frequency.center_bias_hz,
            deserialized.frequency.center_bias_hz
        );
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p pancetta-config`
Expected: All tests PASS (existing + 2 new)

- [ ] **Step 4: Commit**

```bash
git add pancetta-config/src/autonomous.rs
git commit -m "feat: add multi-slot score threshold and frequency allocator config"
```

---

### Task 4: Autonomous Operator — Decode History Buffer and Spectral Input

**Files:**
- Modify: `pancetta-qso/src/autonomous.rs`

- [ ] **Step 1: Add decode history and spectral snapshot to `AutonomousOperator`**

Add imports at top of `pancetta-qso/src/autonomous.rs`:

```rust
use crate::frequency::{
    DecodeHistory, DecodeRecord, FrequencyAllocatorConfig, SmartFrequencyAllocator,
    SpectralSnapshot, TimeSlot,
};
```

Add fields to `AutonomousOperator` struct (after `pending_sequencer_messages`):

```rust
    /// Rolling buffer of recent decode activity for frequency allocation.
    decode_history: DecodeHistory,
    /// Latest spectral snapshot from the waterfall data.
    spectral_snapshot: Option<SpectralSnapshot>,
    /// Smart frequency allocator (replaces simple FrequencyAllocator for new QSOs).
    smart_allocator: SmartFrequencyAllocator,
    /// Minimum score to open an additional QSO slot.
    min_multi_slot_score: f64,
```

Update `AutonomousOperator::new()` to initialize these fields:

```rust
        let decode_history = DecodeHistory::new(4); // default, will be configurable
        let smart_allocator = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
```

And in the `Self { ... }` block:

```rust
            decode_history,
            spectral_snapshot: None,
            smart_allocator,
            min_multi_slot_score: 0.7,
```

- [ ] **Step 2: Add method to accept spectral data**

Add to the `impl AutonomousOperator` block (in the "external inputs" section):

```rust
    /// Update the spectral snapshot from WaterfallData.
    /// Call this each decode cycle with the latest power data.
    pub fn update_spectral(&mut self, snapshot: SpectralSnapshot) {
        self.spectral_snapshot = Some(snapshot);
    }
```

- [ ] **Step 3: Update `feed_decoded_messages` to record decode history**

In `feed_decoded_messages()`, after the `self.frequency_allocator.update_observed(messages);` line, add:

```rust
        // Record decode history for smart frequency allocation.
        let current_slot = if SlotParity::current() == SlotParity::Even {
            TimeSlot::First
        } else {
            TimeSlot::Second
        };
        let records: Vec<DecodeRecord> = messages
            .iter()
            .map(|m| DecodeRecord {
                frequency_hz: m.frequency_hz,
                time_slot: current_slot,
            })
            .collect();
        self.decode_history.push_cycle(records);
```

- [ ] **Step 4: Add a helper to get smart allocator candidates**

Add to `impl AutonomousOperator`:

```rust
    /// Get the best frequency for a new QSO using the smart allocator.
    /// Falls back to the legacy allocator if no spectral data is available.
    fn allocate_smart_frequency(&self, dx_target_hz: Option<f64>) -> f64 {
        let own_freqs: Vec<f64> = self.frequency_allocator.own_frequencies().values().copied().collect();

        if let Some(ref spectral) = self.spectral_snapshot {
            let candidates = self.smart_allocator.rank_candidates(
                spectral,
                &self.decode_history,
                &own_freqs,
                dx_target_hz,
            );
            if let Some(best) = candidates.first() {
                return best.offset_hz;
            }
        }

        // Fallback: legacy allocator
        self.frequency_allocator.allocate_cq_frequency()
    }
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p pancetta-qso`
Expected: All existing tests PASS (no behavior changes yet)

- [ ] **Step 6: Commit**

```bash
git add pancetta-qso/src/autonomous.rs
git commit -m "feat: add decode history buffer, spectral input, and smart allocator to operator"
```

---

### Task 5: Autonomous Operator — Multi-Slot Decision Logic

**Files:**
- Modify: `pancetta-qso/src/autonomous.rs`

- [ ] **Step 1: Write test for multi-slot CQ response**

Add to the existing test module at the bottom of `pancetta-qso/src/autonomous.rs`:

```rust
    #[test]
    fn test_multi_slot_opens_for_high_score() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.max_concurrent_qsos = 2;
        config.slot_parity = SlotParityConfig::Even;

        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));
        op.min_multi_slot_score = 0.5;

        // Simulate one active QSO
        op.set_active_qso_count(1);
        op.add_pending_sequencer_message(
            "K1XYZ W1ABC -10".to_string(),
            1000.0,
            Some("qso-1".to_string()),
        );
        op.frequency_allocator_mut().register_qso_frequency("qso-1", 1000.0);

        // Feed a high-scoring CQ
        let evaluator = HighScoreEvaluator(0.8);
        op.feed_decoded_messages(
            &[DecodedMessageInfo {
                callsign: Some("3Y0J".to_string()),
                frequency_hz: 1500.0,
                snr: -5,
                message_text: "CQ 3Y0J JD15".to_string(),
            }],
            &evaluator,
        );

        // Force an even timestamp so it's our TX slot
        let even_ts = (chrono::Utc::now().timestamp() / 30) * 30;
        let actions = op.decide_at(even_ts);

        let tx_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .collect();

        // Should have 2 transmissions: sequencer message + new CQ response
        assert_eq!(tx_actions.len(), 2, "Expected 2 TX actions, got {:?}", tx_actions);
    }

    #[test]
    fn test_multi_slot_blocked_by_low_score() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.max_concurrent_qsos = 2;
        config.slot_parity = SlotParityConfig::Even;

        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));
        op.min_multi_slot_score = 0.9; // Very high threshold

        op.set_active_qso_count(1);
        op.add_pending_sequencer_message(
            "K1XYZ W1ABC -10".to_string(),
            1000.0,
            Some("qso-1".to_string()),
        );
        op.frequency_allocator_mut().register_qso_frequency("qso-1", 1000.0);

        // Feed a moderate-scoring CQ (below threshold)
        let evaluator = HighScoreEvaluator(0.6);
        op.feed_decoded_messages(
            &[DecodedMessageInfo {
                callsign: Some("VE3XYZ".to_string()),
                frequency_hz: 1500.0,
                snr: -10,
                message_text: "CQ VE3XYZ FN03".to_string(),
            }],
            &evaluator,
        );

        let even_ts = (chrono::Utc::now().timestamp() / 30) * 30;
        let actions = op.decide_at(even_ts);

        let tx_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .collect();

        // Should only have 1 transmission (existing QSO, not the new CQ)
        assert_eq!(tx_actions.len(), 1, "Expected 1 TX action, got {:?}", tx_actions);
    }

    #[test]
    fn test_max_concurrent_qsos_respected() {
        let mut config = AutonomousConfig::default();
        config.enabled = true;
        config.max_concurrent_qsos = 2;
        config.slot_parity = SlotParityConfig::Even;

        let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));
        op.min_multi_slot_score = 0.3;

        // Already at max QSOs
        op.set_active_qso_count(2);
        op.add_pending_sequencer_message("K1A W1ABC -10".to_string(), 1000.0, Some("q1".to_string()));
        op.add_pending_sequencer_message("K2B W1ABC -12".to_string(), 1200.0, Some("q2".to_string()));

        let evaluator = HighScoreEvaluator(0.95);
        op.feed_decoded_messages(
            &[DecodedMessageInfo {
                callsign: Some("3Y0J".to_string()),
                frequency_hz: 1500.0,
                snr: -5,
                message_text: "CQ 3Y0J JD15".to_string(),
            }],
            &evaluator,
        );

        let even_ts = (chrono::Utc::now().timestamp() / 30) * 30;
        let actions = op.decide_at(even_ts);

        let tx_count = actions
            .iter()
            .filter(|a| matches!(a, OperatorAction::Transmit { .. }))
            .count();

        // Should NOT add a third QSO
        assert_eq!(tx_count, 2, "Should not exceed max_concurrent_qsos");
    }

    /// Test helper: evaluator that returns a fixed score
    struct HighScoreEvaluator(f64);
    impl DxEvaluator for HighScoreEvaluator {
        fn evaluate_cq(&self, _: &str, _: Option<&str>, _: i8, _: f64) -> f64 {
            self.0
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso test_multi_slot`
Expected: FAIL — the current `decide_at()` uses `min_dx_score` for all slots, not `min_multi_slot_score`

- [ ] **Step 3: Update `decide_at()` for multi-slot logic**

In `decide_at()`, replace the "Step 3" section (lines ~920–997). The key change is: when we already have active QSOs (`tx_count > 0`), use `min_multi_slot_score` as the threshold for opening additional slots and use `allocate_smart_frequency` for the new QSO's offset:

```rust
                // Step 3: if we have capacity, try to respond to a CQ or call CQ.
                let total_active = tx_count + self.active_qso_count;
                let can_add_new = total_active < self.config.max_concurrent_qsos;

                if can_add_new {
                    // Choose threshold: first QSO uses min_dx_score,
                    // additional QSOs use the higher min_multi_slot_score.
                    let threshold = if total_active == 0 {
                        self.config.min_dx_score
                    } else {
                        self.min_multi_slot_score
                    };

                    let best_cq = self
                        .pending_cqs
                        .iter()
                        .filter(|cq| cq.dx_score >= threshold)
                        .find(|cq| self.frequency_allocator.is_clear_of_own(cq.frequency_hz))
                        .cloned();

                    if let Some(cq) = best_cq {
                        if tx_count == 0 && self.active_qso_count == 0 {
                            self.state = OperatingState::Hunting;
                        }
                        self.idle_cycles = 0;

                        // Use smart allocator to find best TX frequency near the DX station
                        let tx_freq = self.allocate_smart_frequency(Some(cq.frequency_hz));

                        let grid_part = self
                            .our_grid
                            .as_deref()
                            .map(|g| format!(" {}", g))
                            .unwrap_or_default();
                        let message_text =
                            format!("{} {}{}", cq.callsign, self.our_callsign, grid_part)
                                .trim()
                                .to_string();

                        debug!(
                            "Responding to CQ from {} (score={:.2}, snr={}) at {:.0} Hz (TX at {:.0} Hz)",
                            cq.callsign, cq.dx_score, cq.snr, cq.frequency_hz, tx_freq
                        );

                        actions.push(OperatorAction::Transmit {
                            message_text,
                            frequency_offset: tx_freq,
                            qso_id: None,
                        });
                        tx_count += 1;
                    } else if tx_count == 0 && self.active_qso_count == 0 {
                        // Step 4: no CQs worth answering and no active QSOs — CQ ourselves?
                        self.idle_cycles += 1;

                        if self.idle_cycles >= self.config.cq_after_idle_cycles {
                            self.state = OperatingState::CallingCq;
                            self.idle_cycles = 0;

                            let cq_freq = self.allocate_smart_frequency(None);

                            let cq_text = if self.config.cq_direction.is_empty() {
                                format!(
                                    "CQ {} {}",
                                    self.our_callsign,
                                    self.our_grid.as_deref().unwrap_or("")
                                )
                            } else {
                                format!(
                                    "CQ {} {} {}",
                                    self.config.cq_direction,
                                    self.our_callsign,
                                    self.our_grid.as_deref().unwrap_or("")
                                )
                            }
                            .trim()
                            .to_string();

                            actions.push(OperatorAction::Transmit {
                                message_text: cq_text,
                                frequency_offset: cq_freq,
                                qso_id: None,
                            });
                        } else {
                            self.state = OperatingState::Hunting;
                            actions.push(OperatorAction::Listen);
                        }
                    }
                }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p pancetta-qso`
Expected: All tests PASS (existing + 3 new multi-slot tests)

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/autonomous.rs
git commit -m "feat: multi-slot decision logic with configurable score threshold"
```

---

### Task 6: Coordinator — Route WaterfallData to Autonomous Operator

**Files:**
- Modify: `pancetta/src/coordinator.rs`

- [ ] **Step 1: Add a channel for waterfall data to autonomous operator**

In `start_autonomous_component()`, after the `let operator = ...` line (~line 2341), create a channel to send waterfall data:

```rust
        let (waterfall_to_auto_tx, waterfall_to_auto_rx) =
            crossbeam_channel::bounded::<Vec<Vec<f32>>>(2);
```

Store the sender somewhere the FT8 pipeline can access it. The simplest approach: add a new field to the Coordinator struct.

In the coordinator struct definition, add:

```rust
    /// Sender for waterfall data to the autonomous operator.
    waterfall_to_auto_tx: Option<crossbeam_channel::Sender<Vec<Vec<f32>>>>,
```

Initialize it as `None` in `Coordinator::new()`.

In `start_autonomous_component()`, set it:

```rust
        self.waterfall_to_auto_tx = Some(waterfall_to_auto_tx);
```

- [ ] **Step 2: Send waterfall data in FT8 pipeline**

In `start_ft8_pipeline()` (around line 807, where waterfall rows are sent to the TUI), add a second send to the autonomous operator:

After `let _ = waterfall_tx.send(rows);`, add:

```rust
                                    // Also send to autonomous operator for frequency allocation
                                    if let Some(ref auto_wf_tx) = self_waterfall_to_auto_tx {
                                        let _ = auto_wf_tx.try_send(rows.clone());
                                    }
```

Note: You'll need to capture `self.waterfall_to_auto_tx.clone()` before the spawn, like the other fields. Clone the sender as `let self_waterfall_to_auto_tx = self.waterfall_to_auto_tx.clone();` before the tokio::spawn in `start_ft8_pipeline`.

- [ ] **Step 3: Receive waterfall data in autonomous operator task**

In the autonomous operator's `tokio::spawn` loop (inside `start_autonomous_component()`), add a try_recv for waterfall data just before the `op.decide()` call:

```rust
                            // Update spectral data from waterfall
                            if let Ok(rows) = waterfall_to_auto_rx.try_recv() {
                                // Average rows into a single spectral snapshot
                                if let Some(first_row) = rows.first() {
                                    let num_bins = first_row.len();
                                    let mut avg = vec![0.0f32; num_bins];
                                    for row in &rows {
                                        for (i, &v) in row.iter().enumerate().take(num_bins) {
                                            avg[i] += v;
                                        }
                                    }
                                    let n = rows.len() as f32;
                                    for v in &mut avg {
                                        *v /= n;
                                    }
                                    op.update_spectral(pancetta_qso::frequency::SpectralSnapshot {
                                        power_bins: avg,
                                        freq_min_hz: 200.0,
                                        freq_max_hz: 4000.0,
                                    });
                                }
                            }
```

- [ ] **Step 4: Add slot open/close logging**

In the autonomous operator task, after processing `OperatorAction::Transmit`, add logging for new QSO slots:

```rust
                                    pancetta_qso::OperatorAction::Transmit {
                                        ref message_text,
                                        frequency_offset,
                                        ref qso_id,
                                    } => {
                                        if qso_id.is_none() {
                                            info!(
                                                "Autonomous: opening slot at {:.0} Hz: {}",
                                                frequency_offset, message_text
                                            );
                                        }
                                        tx_items.push(crate::message_bus::TransmitRequestItem {
                                            message_text: message_text.clone(),
                                            frequency_offset,
                                            qso_id: qso_id.clone(),
                                        });
                                    }
```

- [ ] **Step 5: Wire new config fields to operator**

In `start_autonomous_component()`, after creating the operator, set the new config values:

```rust
        {
            let mut op = operator.blocking_lock();
            op.min_multi_slot_score = config_snapshot.min_multi_slot_score;
            // The smart allocator config will use defaults matching the config file values
        }
```

Where `config_snapshot` is the autonomous config values read before `drop(config)`. You'll need to capture `min_multi_slot_score` alongside the other config values.

- [ ] **Step 6: Build and verify**

Run: `cargo check -p pancetta`
Expected: Compiles successfully

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "feat: route waterfall data to autonomous operator for smart frequency allocation"
```

---

### Task 7: Multi-Stream Loopback Test

**Files:**
- Modify: `pancetta/tests/loopback_qso.rs`

- [ ] **Step 1: Write the multi-stream loopback test**

Add to `pancetta/tests/loopback_qso.rs`:

```rust
/// Two simultaneous FT8 QSOs decoded from a single summed audio buffer.
///
/// Proves that:
/// 1. Two signals at different audio offsets can be modulated into one buffer
/// 2. The decoder extracts both signals from the summed audio
/// 3. Each QSO can run independently to completion
#[test]
fn test_two_simultaneous_qsos_loopback() {
    use pancetta_ft8::{modulate_multi_tx, MultiTxItem, ProtocolParams};

    let mut our_station = Station::new("W1ABC", "FN42");
    let mut dx_station_1 = Station::new("K2DEF", "FM18");
    let mut dx_station_2 = Station::new("JA1XYZ", "PM95");

    let freq_1 = 800.0;  // QSO 1 at 800 Hz
    let freq_2 = 1400.0; // QSO 2 at 1400 Hz (600 Hz separation)
    let ft8_params = ProtocolParams::ft8();

    // === Round 1: Both DX stations send CQ simultaneously ===
    let symbols_1 = dx_station_1.encoder.encode_message("CQ K2DEF FM18", None).unwrap();
    let symbols_2 = dx_station_2.encoder.encode_message("CQ JA1XYZ PM95", None).unwrap();

    let items = vec![
        MultiTxItem {
            symbols: &symbols_1,
            frequency_offset: freq_1,
            params: &ft8_params,
        },
        MultiTxItem {
            symbols: &symbols_2,
            frequency_offset: freq_2,
            params: &ft8_params,
        },
    ];
    let mut combined_audio = modulate_multi_tx(&items, 12000, 0.0, 0.5).unwrap();
    combined_audio.resize(WINDOW_SAMPLES, 0.0);

    let decoded = our_station.decode(&combined_audio);
    assert!(
        find_message(&decoded, "CQ K2DEF FM18").is_some(),
        "Should decode CQ from K2DEF. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    assert!(
        find_message(&decoded, "CQ JA1XYZ PM95").is_some(),
        "Should decode CQ from JA1XYZ. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // === Round 2: We respond to both simultaneously ===
    let resp_1_symbols = our_station.encoder.encode_message("K2DEF W1ABC FN42", None).unwrap();
    let resp_2_symbols = our_station.encoder.encode_message("JA1XYZ W1ABC FN42", None).unwrap();

    let items = vec![
        MultiTxItem {
            symbols: &resp_1_symbols,
            frequency_offset: freq_1,
            params: &ft8_params,
        },
        MultiTxItem {
            symbols: &resp_2_symbols,
            frequency_offset: freq_2,
            params: &ft8_params,
        },
    ];
    let mut combined_audio = modulate_multi_tx(&items, 12000, 0.0, 0.5).unwrap();
    combined_audio.resize(WINDOW_SAMPLES, 0.0);

    // DX station 1 decodes our response
    let decoded_1 = dx_station_1.decode(&combined_audio);
    assert!(
        find_message(&decoded_1, "K2DEF W1ABC FN42").is_some(),
        "DX1 should decode response. Got: {:?}",
        decoded_1.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // DX station 2 decodes our response
    let decoded_2 = dx_station_2.decode(&combined_audio);
    assert!(
        find_message(&decoded_2, "JA1XYZ W1ABC FN42").is_some(),
        "DX2 should decode response. Got: {:?}",
        decoded_2.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // === Round 3: Both DX stations send signal reports simultaneously ===
    let rpt_1_symbols = dx_station_1.encoder.encode_message("W1ABC K2DEF -10", None).unwrap();
    let rpt_2_symbols = dx_station_2.encoder.encode_message("W1ABC JA1XYZ -14", None).unwrap();

    let items = vec![
        MultiTxItem {
            symbols: &rpt_1_symbols,
            frequency_offset: freq_1,
            params: &ft8_params,
        },
        MultiTxItem {
            symbols: &rpt_2_symbols,
            frequency_offset: freq_2,
            params: &ft8_params,
        },
    ];
    let mut combined_audio = modulate_multi_tx(&items, 12000, 0.0, 0.5).unwrap();
    combined_audio.resize(WINDOW_SAMPLES, 0.0);

    let decoded = our_station.decode(&combined_audio);
    assert!(
        find_message(&decoded, "W1ABC K2DEF -10").is_some(),
        "Should decode report from K2DEF. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    assert!(
        find_message(&decoded, "W1ABC JA1XYZ -14").is_some(),
        "Should decode report from JA1XYZ. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // === Round 4: We send R+reports to both simultaneously ===
    let r_rpt_1_symbols = our_station.encoder.encode_message("K2DEF W1ABC R-12", None).unwrap();
    let r_rpt_2_symbols = our_station.encoder.encode_message("JA1XYZ W1ABC R-08", None).unwrap();

    let items = vec![
        MultiTxItem {
            symbols: &r_rpt_1_symbols,
            frequency_offset: freq_1,
            params: &ft8_params,
        },
        MultiTxItem {
            symbols: &r_rpt_2_symbols,
            frequency_offset: freq_2,
            params: &ft8_params,
        },
    ];
    let mut combined_audio = modulate_multi_tx(&items, 12000, 0.0, 0.5).unwrap();
    combined_audio.resize(WINDOW_SAMPLES, 0.0);

    let decoded_1 = dx_station_1.decode(&combined_audio);
    assert!(
        find_message(&decoded_1, "K2DEF W1ABC R -12").is_some(),
        "DX1 should decode R+report. Got: {:?}",
        decoded_1.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    let decoded_2 = dx_station_2.decode(&combined_audio);
    assert!(
        find_message(&decoded_2, "JA1XYZ W1ABC R -08").is_some(),
        "DX2 should decode R+report. Got: {:?}",
        decoded_2.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // === Round 5: Both DX stations send RR73 simultaneously ===
    let rr73_1_symbols = dx_station_1.encoder.encode_message("W1ABC K2DEF RR73", None).unwrap();
    let rr73_2_symbols = dx_station_2.encoder.encode_message("W1ABC JA1XYZ RR73", None).unwrap();

    let items = vec![
        MultiTxItem {
            symbols: &rr73_1_symbols,
            frequency_offset: freq_1,
            params: &ft8_params,
        },
        MultiTxItem {
            symbols: &rr73_2_symbols,
            frequency_offset: freq_2,
            params: &ft8_params,
        },
    ];
    let mut combined_audio = modulate_multi_tx(&items, 12000, 0.0, 0.5).unwrap();
    combined_audio.resize(WINDOW_SAMPLES, 0.0);

    let decoded = our_station.decode(&combined_audio);
    assert!(
        find_message(&decoded, "W1ABC K2DEF RR73").is_some(),
        "Should decode RR73 from K2DEF. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    assert!(
        find_message(&decoded, "W1ABC JA1XYZ RR73").is_some(),
        "Should decode RR73 from JA1XYZ. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // === Round 6: We send 73 to both simultaneously ===
    let s73_1_symbols = our_station.encoder.encode_message("K2DEF W1ABC 73", None).unwrap();
    let s73_2_symbols = our_station.encoder.encode_message("JA1XYZ W1ABC 73", None).unwrap();

    let items = vec![
        MultiTxItem {
            symbols: &s73_1_symbols,
            frequency_offset: freq_1,
            params: &ft8_params,
        },
        MultiTxItem {
            symbols: &s73_2_symbols,
            frequency_offset: freq_2,
            params: &ft8_params,
        },
    ];
    let mut combined_audio = modulate_multi_tx(&items, 12000, 0.0, 0.5).unwrap();
    combined_audio.resize(WINDOW_SAMPLES, 0.0);

    let decoded_1 = dx_station_1.decode(&combined_audio);
    assert!(
        find_message(&decoded_1, "K2DEF W1ABC 73").is_some(),
        "DX1 should decode 73. Got: {:?}",
        decoded_1.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    let decoded_2 = dx_station_2.decode(&combined_audio);
    assert!(
        find_message(&decoded_2, "JA1XYZ W1ABC 73").is_some(),
        "DX2 should decode 73. Got: {:?}",
        decoded_2.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --features transmit -p pancetta test_two_simultaneous_qsos_loopback`
Expected: PASS — proves two FT8 signals can be encoded, summed, and decoded independently

- [ ] **Step 3: Commit**

```bash
git add pancetta/tests/loopback_qso.rs
git commit -m "test: two simultaneous FT8 QSOs decoded from summed audio in loopback"
```

---

### Task 8: Integration Verification and Final Review

**Files:**
- All modified files

- [ ] **Step 1: Run full test suite for affected crates**

Run: `cargo test -p pancetta-qso -p pancetta-config`
Expected: All tests PASS

- [ ] **Step 2: Run the multi-stream loopback test**

Run: `cargo test --features transmit -p pancetta test_two_simultaneous_qsos_loopback -- --nocapture`
Expected: PASS with both QSOs completing all 6 rounds

- [ ] **Step 3: Check compilation of the full workspace**

Run: `cargo check --workspace`
Expected: Compiles with no errors (warnings are acceptable)

- [ ] **Step 4: Run all loopback tests**

Run: `cargo test --features transmit -p pancetta loopback`
Expected: All loopback tests PASS (existing single-stream + new multi-stream)

- [ ] **Step 5: Commit any remaining fixes**

Only if previous steps revealed issues.

```bash
git add -A
git commit -m "fix: address integration issues from Phase 3 multi-stream TX"
```
