//! Station-owned transmit audio drive, relative to the existing waveform.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use pancetta_core::tx_meter::{ForwardPower, TxMeterReading};
use pancetta_core::{Band, TxDriveMode};

/// The persisted part of one band's drive choice.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SavedTxDrive {
    pub db: f32,
    pub mode: TxDriveMode,
}

/// One waveform's immutable drive selection. Old reports cannot undo an operator choice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DriveSnapshot {
    pub db: f32,
    pub band: Option<Band>,
    pub generation: u64,
    pub mode: TxDriveMode,
    pub available: bool,
}

/// Conditions that must match for forward-power comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DriveContext {
    pub rig_generation: u64,
    pub ft_mode: u8,
    pub stream_count: usize,
}

/// Outcome of a completed, aired FT burst.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DriveDecision {
    Held,
    Changed(f32),
    Overdrive(f32),
    Floor,
    Unavailable,
    Stale,
}

#[derive(Debug, Clone, Copy)]
struct PowerSample {
    value: f32,
    watts: bool,
}

#[derive(Debug, Clone, Copy)]
struct PendingPower {
    baseline: PowerSample,
    context: DriveContext,
}

#[derive(Debug, Clone)]
struct BandDrive {
    db: f32,
    mode: TxDriveMode,
    generation: u64,
    available: bool,
    established: bool,
    last_context: Option<DriveContext>,
    flat_streak: u8,
    flat_hold: bool,
    pending_power: Option<PendingPower>,
}

impl BandDrive {
    fn new(db: f32, mode: TxDriveMode) -> Self {
        Self {
            db,
            mode,
            generation: 0,
            available: false,
            established: false,
            last_context: None,
            flat_streak: 0,
            flat_hold: false,
            pending_power: None,
        }
    }

    fn snapshot(&self, band: Option<Band>) -> DriveSnapshot {
        DriveSnapshot {
            db: self.db,
            band,
            generation: self.generation,
            mode: self.mode,
            available: self.available,
        }
    }

    fn clear_power(&mut self) {
        self.flat_streak = 0;
        self.flat_hold = false;
        self.pending_power = None;
    }
}

fn peak_power(samples: &[TxMeterReading]) -> Option<PowerSample> {
    let mut best: Option<PowerSample> = None;
    let mut mixed_units = false;
    for sample in samples {
        let candidate = match sample.forward_power {
            Some(ForwardPower::Watts(v)) if v.is_finite() && v > 0.0 => Some(PowerSample {
                value: v,
                watts: true,
            }),
            Some(ForwardPower::FractionOfMax(v)) if v.is_finite() && v > 0.0 => Some(PowerSample {
                value: v,
                watts: false,
            }),
            _ => None,
        };
        if let Some(candidate) = candidate {
            if best.is_some_and(|old| old.watts != candidate.watts) {
                mixed_units = true;
            }
            if best.map_or(true, |old| {
                old.watts != candidate.watts || candidate.value > old.value
            }) {
                best = Some(candidate);
            }
        }
    }
    if mixed_units {
        None
    } else {
        best
    }
}

pub(crate) struct TxDrive {
    ceiling_db: f32,
    floor_db: f32,
    start_db: f32,
    auto_enabled: bool,
    onset: f32,
    max_alc: f32,
    warn_only_enabled: bool,
    step_up_db: f32,
    step_down_db: f32,
    bands: Mutex<HashMap<Band, BandDrive>>,
    persistence_generation: AtomicU64,
    persisted_generation: AtomicU64,
    effective_bits: AtomicU32,
}

impl TxDrive {
    pub(crate) fn from_config(config: &pancetta_config::Config) -> Self {
        let mut drive = Self::new(
            config.audio.levels.output_gain_db,
            config.audio.levels.output_gain_ceiling_db,
        );
        drive.floor_db = config
            .audio
            .levels
            .output_gain_floor_db
            .min(drive.ceiling_db)
            .max(-20.0);
        drive.start_db = config
            .audio
            .auto_drive
            .start_db
            .clamp(drive.floor_db, drive.ceiling_db);
        drive.auto_enabled = config.audio.auto_drive.enabled;
        drive.onset = config.audio.auto_drive.alc_onset;
        let configured_max = config.rig.power_control.protection.max_alc;
        drive.max_alc =
            if configured_max.is_finite() && configured_max > 0.0 && configured_max < 1.0 {
                configured_max
            } else {
                0.5
            };
        drive.warn_only_enabled = config.rig.power_control.protection.alc_overdrive_warning;
        drive.step_up_db = config.audio.auto_drive.step_up_db;
        drive.step_down_db = config.audio.auto_drive.step_down_db;
        drive
    }

    pub(crate) fn new(configured_db: f32, ceiling_db: f32) -> Self {
        let effective = configured_db.clamp(-20.0, ceiling_db);
        Self {
            ceiling_db,
            floor_db: -20.0,
            start_db: -6.0_f32.clamp(-20.0, ceiling_db),
            auto_enabled: false,
            onset: 0.1,
            max_alc: 0.5,
            warn_only_enabled: true,
            step_up_db: 0.5,
            step_down_db: 2.0,
            bands: Mutex::new(HashMap::new()),
            persistence_generation: AtomicU64::new(0),
            persisted_generation: AtomicU64::new(0),
            effective_bits: AtomicU32::new(effective.to_bits()),
        }
    }

    pub(crate) fn get(&self) -> f32 {
        f32::from_bits(self.effective_bits.load(Ordering::Acquire))
    }

    pub(crate) fn ceiling_db(&self) -> f32 {
        self.ceiling_db
    }

    pub(crate) fn adjust(&self, step: i8) -> f32 {
        let mut old = self.effective_bits.load(Ordering::Acquire);
        loop {
            let next =
                (f32::from_bits(old) + f32::from(step)).clamp(self.floor_db, self.ceiling_db);
            match self.effective_bits.compare_exchange_weak(
                old,
                next.to_bits(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return next,
                Err(actual) => old = actual,
            }
        }
    }

    pub(crate) fn auto_enabled(&self) -> bool {
        self.auto_enabled
    }

    pub(crate) fn warn_only_limit(&self) -> Option<f32> {
        self.warn_only_enabled.then_some(self.max_alc)
    }

    /// A failed write remains dirty until a later successful save.
    pub(crate) fn needs_persist(&self) -> bool {
        self.persistence_generation.load(Ordering::Acquire)
            > self.persisted_generation.load(Ordering::Acquire)
    }

    pub(crate) fn mark_persisted(&self, generation: u64) {
        self.persisted_generation
            .fetch_max(generation, Ordering::AcqRel);
    }

    /// Select a drive next to the key-down enqueue. An unknown band uses the
    /// legacy fallback and never creates a learned entry.
    pub(crate) fn snapshot_for_band(&self, band: Option<Band>) -> DriveSnapshot {
        if self.auto_enabled {
            if let Some(band) = band {
                let mut bands = self.bands.lock().unwrap_or_else(|e| e.into_inner());
                return bands
                    .entry(band)
                    .or_insert_with(|| BandDrive::new(self.start_db, TxDriveMode::Auto))
                    .snapshot(Some(band));
            }
        }
        DriveSnapshot {
            db: self.get(),
            band: None,
            generation: 0,
            mode: TxDriveMode::Manual,
            available: false,
        }
    }

    /// A local F7/F8 press selects Manual on the effective TX band.
    pub(crate) fn adjust_band(&self, band: Option<Band>, step: i8) -> DriveSnapshot {
        if !self.auto_enabled || band.is_none() {
            self.adjust(step);
            return self.snapshot_for_band(None);
        }
        let band = band.expect("checked above");
        let mut bands = self.bands.lock().unwrap_or_else(|e| e.into_inner());
        let state = bands
            .entry(band)
            .or_insert_with(|| BandDrive::new(self.start_db, TxDriveMode::Auto));
        state.db = (state.db + f32::from(step)).clamp(self.floor_db, self.ceiling_db);
        state.mode = TxDriveMode::Manual;
        state.generation = state.generation.wrapping_add(1);
        state.clear_power();
        self.persistence_generation.fetch_add(1, Ordering::AcqRel);
        state.snapshot(Some(band))
    }

    pub(crate) fn toggle_band(&self, band: Option<Band>) -> DriveSnapshot {
        if !self.auto_enabled || band.is_none() {
            return self.snapshot_for_band(None);
        }
        let band = band.expect("checked above");
        let mut bands = self.bands.lock().unwrap_or_else(|e| e.into_inner());
        let state = bands
            .entry(band)
            .or_insert_with(|| BandDrive::new(self.start_db, TxDriveMode::Auto));
        state.mode = state.mode.toggle();
        state.generation = state.generation.wrapping_add(1);
        state.clear_power();
        self.persistence_generation.fetch_add(1, Ordering::AcqRel);
        state.snapshot(Some(band))
    }

    /// Capture the map and its generation under the same lock.
    pub(crate) fn restored_map_with_generation(&self) -> (HashMap<String, SavedTxDrive>, u64) {
        let bands = self.bands.lock().unwrap_or_else(|e| e.into_inner());
        let map = bands
            .iter()
            .filter(|(band, _)| !matches!(band, Band::Custom(_, _)))
            .map(|(band, state)| {
                (
                    band.name().to_string(),
                    SavedTxDrive {
                        db: state.db,
                        mode: state.mode,
                    },
                )
            })
            .collect();
        (map, self.persistence_generation.load(Ordering::Acquire))
    }

    pub(crate) fn restore_map(&self, saved: &HashMap<String, SavedTxDrive>) {
        if !self.auto_enabled {
            return;
        }
        let mut bands = self.bands.lock().unwrap_or_else(|e| e.into_inner());
        for (name, choice) in saved {
            if !choice.db.is_finite() {
                continue;
            }
            if let Ok(band) = name.parse::<Band>() {
                if matches!(band, Band::Custom(_, _)) {
                    continue;
                }
                let mut restored =
                    BandDrive::new(choice.db.clamp(self.floor_db, self.ceiling_db), choice.mode);
                restored.established = true;
                bands.insert(band, restored);
            }
        }
    }

    /// Consume one normal, completed burst at most once. The worker owns the
    /// report; refused, aborted and unplayed transmissions must not call this.
    pub(crate) fn complete(
        &self,
        snapshot: DriveSnapshot,
        samples: &[TxMeterReading],
        context: DriveContext,
    ) -> DriveDecision {
        let Some(band) = snapshot.band else {
            return DriveDecision::Stale;
        };
        let mut bands = self.bands.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = bands.get_mut(&band) else {
            return DriveDecision::Stale;
        };
        if !self.auto_enabled
            || state.mode != TxDriveMode::Auto
            || state.generation != snapshot.generation
            || state.db != snapshot.db
        {
            return DriveDecision::Stale;
        }
        if state.last_context != Some(context) {
            state.clear_power();
            state.last_context = Some(context);
        }
        let mut peak: Option<f32> = None;
        let mut high_run = 0;
        let mut confirmed_high = false;
        let mut lone_high = false;
        for reading in samples {
            if let Some(alc) = reading
                .alc
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
            {
                peak = Some(peak.map_or(alc, |p| p.max(alc)));
                if alc > self.max_alc {
                    high_run += 1;
                    lone_high = true;
                    if high_run >= 2 {
                        confirmed_high = true;
                    }
                } else {
                    high_run = 0;
                }
            }
        }
        let Some(peak) = peak else {
            state.available = false;
            return DriveDecision::Unavailable;
        };
        state.available = true;
        if !state.established {
            state.established = true;
            self.persistence_generation.fetch_add(1, Ordering::AcqRel);
        }
        state.generation = state.generation.wrapping_add(1);
        if confirmed_high {
            state.clear_power();
            if state.db <= self.floor_db {
                return DriveDecision::Floor;
            }
            state.db = (state.db - self.step_down_db).max(self.floor_db);
            self.persistence_generation.fetch_add(1, Ordering::AcqRel);
            return DriveDecision::Overdrive(state.db);
        }
        if lone_high || peak >= self.onset {
            return DriveDecision::Held;
        }
        let power = peak_power(samples);
        if let Some(pending) = state.pending_power.take() {
            match power {
                Some(now) if pending.context == context && pending.baseline.watts == now.watts => {
                    let gain_db = 10.0 * (now.value / pending.baseline.value).log10();
                    if gain_db < 0.5 {
                        state.flat_streak += 1;
                        if state.flat_streak >= 2 {
                            state.flat_hold = true;
                        }
                    } else {
                        state.flat_streak = 0;
                    }
                }
                _ => {
                    state.clear_power();
                }
            }
        }
        if state.flat_hold || state.db >= self.ceiling_db {
            return DriveDecision::Held;
        }
        state.db = (state.db + self.step_up_db).min(self.ceiling_db);
        self.persistence_generation.fetch_add(1, Ordering::AcqRel);
        if let Some(baseline) = power {
            state.pending_power = Some(PendingPower { baseline, context });
        }
        DriveDecision::Changed(state.db)
    }
}

/// Peak of FT audio after the modulator's final normalization
/// (`Ft8Modulator::apply_final_processing` and the multi-TX sum both leave it here).
pub(crate) const FT_NORMALIZED_PEAK: f32 = 0.95;

/// Scale only the final buffer. Provisional modulator buffers remain reusable.
///
/// `ft_frame` marks FT audio, single frame or bundle, whose peak is already
/// [`FT_NORMALIZED_PEAK`]. With a positive ceiling such audio is reserved down
/// so the ceiling itself peaks at −1 dBFS; Tune's 0.5-peak sine needs no reserve.
pub(crate) fn apply(samples: &mut [f32], db: f32, ceiling_db: f32, ft_frame: bool) {
    let reserve = if ft_frame && ceiling_db > 0.0 {
        (10.0_f32.powf(-1.0 / 20.0) / FT_NORMALIZED_PEAK) / 10.0_f32.powf(ceiling_db / 20.0)
    } else {
        1.0
    };
    let multiplier = reserve * 10.0_f32.powf(db / 20.0);
    if multiplier != 1.0 {
        for sample in samples {
            *sample *= multiplier;
        }
    }
}

#[cfg(test)]
#[path = "tx/tests/tx_drive_tests.rs"]
mod tx_drive_tests;

#[cfg(test)]
#[path = "tx_drive/tests/auto_drive_tests.rs"]
mod auto_drive_tests;
