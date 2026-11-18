//! Station-owned transmit audio drive, relative to the existing waveform.

use std::sync::atomic::{AtomicU32, Ordering};

pub(crate) struct TxDrive {
    ceiling_db: f32,
    effective_bits: AtomicU32,
}

impl TxDrive {
    pub(crate) fn from_config(config: &pancetta_config::Config) -> Self {
        Self::new(
            config.audio.levels.output_gain_db,
            config.audio.levels.output_gain_ceiling_db,
        )
    }

    pub(crate) fn new(configured_db: f32, ceiling_db: f32) -> Self {
        let effective = configured_db.clamp(-20.0, ceiling_db);
        Self {
            ceiling_db,
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
            let next = (f32::from_bits(old) + f32::from(step)).clamp(-20.0, self.ceiling_db);
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
}

/// Scale only the final buffer. Provisional modulator buffers remain reusable.
pub(crate) fn apply(samples: &mut [f32], db: f32, ceiling_db: f32, bundle: bool) {
    let reserve = if bundle && ceiling_db > 0.0 {
        (10.0_f32.powf(-1.0 / 20.0) / 0.95) / 10.0_f32.powf(ceiling_db / 20.0)
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
#[path = "tests/tx_drive_tests.rs"]
mod tx_drive_tests;
