//! One keyed carrier's bounded, feedback-driven drive sweep.

pub(super) const CAL_COARSE_STEP_DB: f32 = 2.0;
pub(super) const CAL_FINE_STEP_DB: f32 = 1.0;
pub(super) const CAL_SEGMENT_MS: u64 = 600;
pub(super) const CAL_CUSHION_MS: u64 = 150;
pub(super) const CAL_MAX_KEYDOWN_MS: u64 = 11_000;
const CAL_MISS_SEGMENTS: u8 = 2;

#[derive(Debug, PartialEq)]
pub(super) enum Decision {
    StepUp,
    Knee(f32),
    Ceiling(f32),
    Abort,
}

pub(super) fn display_db(db: f32) -> String {
    let value = if db.fract() == 0.0 {
        format!("{db:.0}")
    } else {
        format!("{db:.1}")
    };
    value.replace('-', "−")
}

pub(super) fn display_band(band: pancetta_core::Band) -> String {
    band.name().replace('m', " m")
}

/// Removes the active meter sink even if the worker task is cancelled.
pub(super) struct CalibrationSinkGuard {
    slot: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<super::super::tx_meter::DriveCalibrationReads>>>,
    >,
    sink: std::sync::Arc<super::super::tx_meter::DriveCalibrationReads>,
}

impl CalibrationSinkGuard {
    pub(super) fn install(
        slot: std::sync::Arc<
            std::sync::Mutex<Option<std::sync::Arc<super::super::tx_meter::DriveCalibrationReads>>>,
        >,
    ) -> Self {
        let sink = std::sync::Arc::new(super::super::tx_meter::DriveCalibrationReads::default());
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink.clone());
        Self { slot, sink }
    }
    pub(super) fn sink(&self) -> &super::super::tx_meter::DriveCalibrationReads {
        &self.sink
    }
}

impl Drop for CalibrationSinkGuard {
    fn drop(&mut self) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .is_some_and(|active| std::sync::Arc::ptr_eq(active, &self.sink))
        {
            *slot = None;
        }
    }
}

pub(super) struct CalibrationStepper {
    floor: f32,
    ceiling: f32,
    onset: f32,
    level: f32,
    previous: f32,
    fine: bool,
    misses: u8,
}

impl CalibrationStepper {
    pub(super) fn new(floor: f32, ceiling: f32, onset: f32) -> Self {
        Self {
            floor,
            ceiling,
            onset,
            level: floor,
            previous: floor,
            fine: false,
            misses: 0,
        }
    }

    pub(super) fn level(&self) -> f32 {
        self.level
    }

    pub(super) fn observe(&mut self, readings: &[Option<f32>]) -> Decision {
        let peak = readings
            .iter()
            .flatten()
            .copied()
            .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
            .reduce(f32::max);
        let Some(peak) = peak else {
            self.misses += 1;
            return if self.misses >= CAL_MISS_SEGMENTS {
                Decision::Abort
            } else {
                Decision::StepUp
            };
        };
        self.misses = 0;
        if peak >= self.onset {
            if !self.fine && self.level > self.floor {
                // Revisit the interval just crossed in one dB increments.
                self.level = (self.previous + CAL_FINE_STEP_DB).min(self.level);
                self.fine = true;
                return Decision::StepUp;
            }
            return Decision::Knee((self.level - CAL_FINE_STEP_DB).max(self.floor));
        }
        if self.level >= self.ceiling {
            return Decision::Ceiling(self.ceiling);
        }
        self.previous = self.level;
        self.level = (self.level
            + if self.fine {
                CAL_FINE_STEP_DB
            } else {
                CAL_COARSE_STEP_DB
            })
        .min(self.ceiling);
        Decision::StepUp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coarse_then_fine_finds_the_knee() {
        let mut s = CalibrationStepper::new(-20.0, 5.0, 0.1);
        let mut levels = Vec::new();
        loop {
            levels.push(s.level());
            match s.observe(&[Some(if s.level() >= -2.0 { 0.11 } else { 0.01 })]) {
                Decision::Knee(db) => {
                    assert_eq!(db, -3.0);
                    break;
                }
                Decision::StepUp => {}
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(levels[0], -20.0);
        assert!(levels.contains(&-3.0));
    }
    #[test]
    fn spurious_high_then_clean_continues() {
        let mut s = CalibrationStepper::new(-20.0, 5.0, 0.1);
        let mut first_minus_four = true;
        loop {
            let level = s.level();
            let alc = if level == -4.0 && first_minus_four {
                first_minus_four = false;
                0.11
            } else if level >= -2.0 {
                0.11
            } else {
                0.01
            };
            match s.observe(&[Some(alc)]) {
                Decision::StepUp => {}
                Decision::Knee(db) => {
                    assert_eq!(db, -3.0);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
    }
    #[test]
    fn calibration_without_onset_saves_the_ceiling() {
        let mut s = CalibrationStepper::new(-20.0, 5.0, 0.1);
        loop {
            if let Decision::Ceiling(db) = s.observe(&[Some(0.0)]) {
                assert_eq!(db, 5.0);
                break;
            }
        }
    }
    #[test]
    fn calibration_with_onset_at_the_floor_saves_the_floor() {
        let mut s = CalibrationStepper::new(-20.0, 5.0, 0.1);
        assert_eq!(s.observe(&[Some(0.1)]), Decision::Knee(-20.0));
    }
    #[test]
    fn two_segments_without_a_valid_reading_abort() {
        let mut s = CalibrationStepper::new(-20.0, 5.0, 0.1);
        assert_eq!(s.observe(&[None]), Decision::StepUp);
        assert_eq!(s.observe(&[None]), Decision::Abort);
    }
    #[test]
    fn worst_case_schedule_fits_the_budget() {
        let mut stepper = CalibrationStepper::new(-20.0, 5.0, 0.1);
        let mut count = 0;
        loop {
            count += 1;
            let alc = if stepper.level() >= 4.0 { 0.11 } else { 0.01 };
            match stepper.observe(&[Some(alc)]) {
                Decision::StepUp => {}
                Decision::Knee(_) => break,
                other => panic!("{other:?}"),
            }
        }
        assert!(count <= 15, "{count} segments");
        assert!(count * CAL_SEGMENT_MS + CAL_CUSHION_MS < CAL_MAX_KEYDOWN_MS);
    }
}
