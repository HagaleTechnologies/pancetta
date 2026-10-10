//! UTC slot-authority helpers (PAN-114).
//!
//! Every slot deadline pancetta tracks is "the next boundary strictly after
//! some `now`" (`slot::next_slot_start_with_period`,
//! `slot::next_phase_with_period`). A healthy deadline is therefore never
//! more than one period ahead of the wall clock, and a consumer polling it
//! every few hundred ms never passes it by a whole period. A deadline outside
//! that band means the wall clock jumped.

use chrono::{DateTime, Duration, Utc};

/// A wall-clock jump inferred from a slot deadline that left the healthy
/// one-period band around `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallClockJump {
    /// `now` passed the deadline by more than one period (sleep/wake or a
    /// forward NTP step). Carries `now - deadline`.
    Forward(Duration),
    /// The deadline is more than one period ahead (backward NTP step).
    /// Carries `deadline - now`.
    Backward(Duration),
}

impl WallClockJump {
    /// Signed jump size in seconds: positive for [`WallClockJump::Forward`],
    /// negative for [`WallClockJump::Backward`].
    pub fn signed_secs(self) -> f64 {
        match self {
            WallClockJump::Forward(d) => duration_secs_f64(d),
            WallClockJump::Backward(d) => -duration_secs_f64(d),
        }
    }
}

fn duration_secs_f64(d: Duration) -> f64 {
    match d.num_nanoseconds() {
        Some(ns) => ns as f64 / 1e9,
        None => d.num_milliseconds() as f64 / 1e3,
    }
}

/// Classify `deadline` against `now` for a slot of `period_ns` nanoseconds.
///
/// Returns `None` while the deadline sits inside the healthy band
/// (`now - period ..= now + period`); otherwise reports which way the wall
/// clock jumped. The band edges themselves are healthy: a deadline exactly
/// one period ahead is what `next_slot_start_with_period` returns at a
/// boundary.
pub fn classify_deadline_jump(
    now: DateTime<Utc>,
    deadline: DateTime<Utc>,
    period_ns: i64,
) -> Option<WallClockJump> {
    let period = Duration::nanoseconds(period_ns);
    let ahead = deadline - now;
    if ahead > period {
        Some(WallClockJump::Backward(ahead))
    } else if -ahead > period {
        Some(WallClockJump::Forward(-ahead))
    } else {
        None
    }
}

/// Smallest wall-vs-monotonic divergence reported as a step. NTP slewing
/// moves the wall clock at most 500 ppm (0.5 ms/s).
pub const CLOCK_STEP_THRESHOLD_MS: i64 = 250;

/// Detects wall-clock steps (and, where the monotonic clock stops during
/// suspend — Linux, macOS — system sleep) by comparing wall-clock progress
/// with monotonic progress between observations.
#[derive(Debug, Clone)]
pub struct ClockStepDetector {
    last_wall: DateTime<Utc>,
}

impl ClockStepDetector {
    /// Start a detector baselined at `wall_now`.
    pub fn new(wall_now: DateTime<Utc>) -> Self {
        Self {
            last_wall: wall_now,
        }
    }

    /// `mono_elapsed` is monotonic time since the previous call (or `new`).
    /// Returns wall-minus-monotonic skew when it reaches the threshold
    /// (positive = jumped forward / slept, negative = stepped back).
    /// Always re-baselines.
    pub fn observe(
        &mut self,
        wall_now: DateTime<Utc>,
        mono_elapsed: std::time::Duration,
    ) -> Option<Duration> {
        let wall_elapsed = wall_now - self.last_wall;
        self.last_wall = wall_now;
        let mono = Duration::from_std(mono_elapsed).unwrap_or(Duration::MAX);
        let skew = wall_elapsed - mono;
        (skew.num_milliseconds().abs() >= CLOCK_STEP_THRESHOLD_MS).then_some(skew)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FT8_NS: i64 = 15_000_000_000;
    const FT4_NS: i64 = 7_500_000_000;
    const FT2_NS: i64 = 3_200_000_000;

    /// Same helper as `slot.rs`: a UTC instant `seconds` past
    /// 2026-01-01 00:00:00 UTC (a 15 s boundary).
    fn at(seconds: f64) -> DateTime<Utc> {
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let ns = (seconds * 1_000_000_000.0) as i64;
        base + Duration::nanoseconds(ns)
    }

    #[test]
    fn deadline_within_one_period_ahead_is_not_a_jump() {
        let now = at(100.0);
        let deadline = now + Duration::seconds(15);
        assert_eq!(classify_deadline_jump(now, deadline, FT8_NS), None);
    }

    #[test]
    fn deadline_passed_by_less_than_one_period_is_not_a_jump() {
        let deadline = at(105.0);
        let now = deadline + Duration::milliseconds(250);
        assert_eq!(classify_deadline_jump(now, deadline, FT8_NS), None);
    }

    #[test]
    fn deadline_more_than_one_period_ahead_is_a_backward_jump() {
        // Deadline 15 s ahead, then the clock steps back 600 s.
        let deadline = at(1_000.0);
        let now = deadline - Duration::seconds(15) - Duration::seconds(600);
        assert_eq!(
            classify_deadline_jump(now, deadline, FT8_NS),
            Some(WallClockJump::Backward(Duration::seconds(615)))
        );
    }

    #[test]
    fn deadline_passed_by_more_than_one_period_is_a_forward_jump() {
        let deadline = at(1_000.0);
        let now = deadline + Duration::milliseconds(3_604_321);
        assert_eq!(
            classify_deadline_jump(now, deadline, FT8_NS),
            Some(WallClockJump::Forward(Duration::milliseconds(3_604_321)))
        );
    }

    #[test]
    fn jump_band_scales_with_ft4_and_ft2_periods() {
        let now = at(100.0);
        let deadline = now + Duration::seconds(8);
        assert_eq!(classify_deadline_jump(now, deadline, FT8_NS), None);
        assert!(matches!(
            classify_deadline_jump(now, deadline, FT4_NS),
            Some(WallClockJump::Backward(_))
        ));
        assert!(matches!(
            classify_deadline_jump(now, deadline, FT2_NS),
            Some(WallClockJump::Backward(_))
        ));
        let deadline = now - Duration::seconds(4);
        assert_eq!(classify_deadline_jump(now, deadline, FT4_NS), None);
        assert!(matches!(
            classify_deadline_jump(now, deadline, FT2_NS),
            Some(WallClockJump::Forward(_))
        ));
    }

    #[test]
    fn signed_secs_is_positive_forward_negative_backward() {
        assert_eq!(
            WallClockJump::Forward(Duration::milliseconds(3_604_300)).signed_secs(),
            3604.3
        );
        assert_eq!(
            WallClockJump::Backward(Duration::seconds(600)).signed_secs(),
            -600.0
        );
    }

    #[test]
    fn step_detector_ignores_matching_wall_and_monotonic_progress() {
        let t0 = at(0.0);
        let mut d = ClockStepDetector::new(t0);
        assert_eq!(
            d.observe(t0 + Duration::seconds(1), std::time::Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn step_detector_ignores_ntp_slew_sized_divergence() {
        let t0 = at(0.0);
        let mut d = ClockStepDetector::new(t0);
        assert_eq!(
            d.observe(
                t0 + Duration::microseconds(1_000_500),
                std::time::Duration::from_secs(1)
            ),
            None
        );
    }

    #[test]
    fn step_detector_reports_forward_step_and_suspend() {
        let t0 = at(0.0);
        let mut d = ClockStepDetector::new(t0);
        assert_eq!(
            d.observe(
                t0 + Duration::seconds(3601),
                std::time::Duration::from_secs(1)
            ),
            Some(Duration::seconds(3600))
        );
    }

    #[test]
    fn step_detector_reports_backward_step() {
        let t0 = at(1_000.0);
        let mut d = ClockStepDetector::new(t0);
        assert_eq!(
            d.observe(
                t0 - Duration::seconds(599),
                std::time::Duration::from_secs(1)
            ),
            Some(Duration::seconds(-600))
        );
    }

    #[test]
    fn step_detector_rebaselines_after_reporting() {
        let t0 = at(0.0);
        let mut d = ClockStepDetector::new(t0);
        let t1 = t0 + Duration::seconds(3601);
        assert!(d.observe(t1, std::time::Duration::from_secs(1)).is_some());
        assert_eq!(
            d.observe(t1 + Duration::seconds(1), std::time::Duration::from_secs(1)),
            None
        );
    }
}
