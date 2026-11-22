//! UTC slot clock (PAN-358) and slot-authority helpers (PAN-114).
//!
//! [`SlotClock`] is the one time source slot consumers read. Every
//! [`SlotClock::read`] samples the wall and monotonic sources under one lock,
//! compares their progress since the previous read by any holder of the
//! clock, and records a [`ClockEvent`] when they diverge by
//! [`CLOCK_STEP_THRESHOLD_MS`] or more (an NTP step, a manual change, or a
//! sleep on Linux or macOS). A divergence of [`SLOT_RESYNC_THRESHOLD_MS`] or
//! more is also a resync: slot timers re-anchor to the first boundary after
//! the step. Because the samples are ordered by the lock, a reading whose
//! `now` is post-step always carries the post-step epoch, so every consumer
//! observes the same step under the same epoch.
//!
//! Every slot deadline pancetta tracks is "the next boundary strictly after
//! some `now`" (`slot::next_slot_start_with_period`,
//! `slot::next_phase_with_period`). A healthy deadline is therefore never
//! more than one period ahead of the wall clock.

use crate::time::UtcClock;
use chrono::{DateTime, Duration, Utc};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

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

/// Smallest wall-vs-monotonic divergence that re-anchors slot timing (a
/// resync). Equals the TX worker's PTT hold slack: a smaller step cannot
/// move a UTC-anchored deadline off the grid in a way any consumer acts on.
pub const SLOT_RESYNC_THRESHOLD_MS: i64 = 1_000;

/// Monotonic time since an arbitrary origin.
pub type MonoClock = Arc<dyn Fn() -> std::time::Duration + Send + Sync>;

/// One wall-clock step observed by a [`SlotClock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockEvent {
    /// 1 for the first event a clock reports, then 2, 3, ...
    pub epoch: u64,
    /// Wall elapsed minus monotonic elapsed between the two reads that
    /// bracket the step; positive when the clock stepped forward or slept.
    pub skew: Duration,
    /// Wall time of the read that observed the step.
    pub observed_at: DateTime<Utc>,
    /// Active slot period at that read.
    pub slot_ns: i64,
    /// `|skew| >= SLOT_RESYNC_THRESHOLD_MS`.
    pub resync: bool,
}

impl ClockEvent {
    /// The step as a [`WallClockJump`] carrying its real size.
    pub fn jump(&self) -> WallClockJump {
        if self.skew >= Duration::zero() {
            WallClockJump::Forward(self.skew)
        } else {
            WallClockJump::Backward(-self.skew)
        }
    }

    /// Start of the slot the step landed in: every consumer that re-anchors
    /// on this event lands in this slot.
    pub fn slot_start(&self) -> DateTime<Utc> {
        crate::slot::current_slot_start_with_period(self.observed_at, self.slot_ns)
    }
}

/// One [`SlotClock::read`]: the time, the active period and the clock's
/// event state, all from one locked sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotReading {
    /// Wall time of this read.
    pub now: DateTime<Utc>,
    /// Active slot period; a non-positive source falls back to
    /// [`crate::slot::SLOT_NS`].
    pub slot_ns: i64,
    /// Number of events (steps of 250 ms or more) seen so far.
    pub epoch: u64,
    /// Number of resync events (steps of 1 s or more) seen so far.
    pub resync_epoch: u64,
    /// The latest event, if any.
    pub last_event: Option<ClockEvent>,
    /// The latest resync event, if any.
    pub last_resync: Option<ClockEvent>,
}

/// An instant on both clocks, taken from one sample (PAN-289 ages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockStamp {
    /// Wall time of the sample.
    pub wall: DateTime<Utc>,
    /// Monotonic time of the sample.
    pub mono: std::time::Duration,
}

impl ClockStamp {
    /// Age at `now`: the larger of monotonic and wall-clock elapsed time.
    /// Wall time counts a suspend; monotonic time survives a backward step.
    /// A negative wall elapsed (the wall clock stepped back) counts as zero.
    pub fn age_at(&self, now: ClockStamp) -> std::time::Duration {
        let mono = now.mono.saturating_sub(self.mono);
        let wall = (now.wall - self.wall).to_std().unwrap_or_default();
        mono.max(wall)
    }
}

#[derive(Debug)]
struct ClockState {
    last_wall: DateTime<Utc>,
    last_mono: std::time::Duration,
    epoch: u64,
    resync_epoch: u64,
    last_event: Option<ClockEvent>,
    last_resync: Option<ClockEvent>,
}

/// Where the clock's monotonic time comes from.
enum MonoSource {
    /// A real monotonic source.
    Clock(MonoClock),
    /// Derived from the wall source (`wall - origin`, clamped at zero), for
    /// virtual-clock harnesses: wall and "monotonic" never diverge.
    FromWall(DateTime<Utc>),
}

struct ClockInner {
    wall: UtcClock,
    mono: MonoSource,
    slot_ns: Arc<AtomicI64>,
    detect: bool,
    state: Mutex<ClockState>,
}

/// The one UTC slot clock (PAN-358). A cheap `Clone` handle; every clone
/// shares one detector, so a step one holder observes is seen by all.
#[derive(Clone)]
pub struct SlotClock {
    inner: Arc<ClockInner>,
}

impl std::fmt::Debug for SlotClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotClock")
            .field("detect", &self.inner.detect)
            .finish_non_exhaustive()
    }
}

impl SlotClock {
    /// The production clock: `Utc::now` and `Instant` since construction.
    pub fn system(slot_ns: Arc<AtomicI64>) -> Self {
        let origin = std::time::Instant::now();
        Self::new(
            Arc::new(Utc::now),
            Arc::new(move || origin.elapsed()),
            slot_ns,
        )
    }

    /// A clock over the given wall and monotonic sources, with step
    /// detection.
    pub fn new(wall: UtcClock, mono: MonoClock, slot_ns: Arc<AtomicI64>) -> Self {
        let last_wall = wall();
        let last_mono = mono();
        Self::build(wall, MonoSource::Clock(mono), slot_ns, true, last_wall, last_mono)
    }

    /// A clock that never reports an event: monotonic time is derived from
    /// the wall source, so a virtual clock that jumps keeps its meaning.
    pub fn without_step_detection(wall: UtcClock, slot_ns: Arc<AtomicI64>) -> Self {
        let origin = wall();
        Self::build(
            wall,
            MonoSource::FromWall(origin),
            slot_ns,
            false,
            origin,
            std::time::Duration::ZERO,
        )
    }

    fn build(
        wall: UtcClock,
        mono: MonoSource,
        slot_ns: Arc<AtomicI64>,
        detect: bool,
        last_wall: DateTime<Utc>,
        last_mono: std::time::Duration,
    ) -> Self {
        Self {
            inner: Arc::new(ClockInner {
                wall,
                mono,
                slot_ns,
                detect,
                state: Mutex::new(ClockState {
                    last_wall,
                    last_mono,
                    epoch: 0,
                    resync_epoch: 0,
                    last_event: None,
                    last_resync: None,
                }),
            }),
        }
    }

    fn slot_ns_now(&self) -> i64 {
        let ns = self.inner.slot_ns.load(Ordering::Relaxed);
        if ns > 0 {
            ns
        } else {
            crate::slot::SLOT_NS
        }
    }

    /// Sample both sources under the lock, record any step, re-baseline.
    fn sample(&self) -> (SlotReading, std::time::Duration) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let wall = (self.inner.wall)();
        let mono = match &self.inner.mono {
            MonoSource::Clock(mono) => mono(),
            MonoSource::FromWall(origin) => (wall - *origin).to_std().unwrap_or_default(),
        };
        let slot_ns = self.slot_ns_now();
        if self.inner.detect {
            let wall_elapsed = wall - state.last_wall;
            let mono_elapsed = Duration::from_std(mono.saturating_sub(state.last_mono))
                .unwrap_or(Duration::MAX);
            let skew = wall_elapsed - mono_elapsed;
            let abs_ms = skew.num_milliseconds().abs();
            if abs_ms >= CLOCK_STEP_THRESHOLD_MS {
                state.epoch += 1;
                let event = ClockEvent {
                    epoch: state.epoch,
                    skew,
                    observed_at: wall,
                    slot_ns,
                    resync: abs_ms >= SLOT_RESYNC_THRESHOLD_MS,
                };
                state.last_event = Some(event);
                if event.resync {
                    state.resync_epoch += 1;
                    state.last_resync = Some(event);
                }
            }
        }
        state.last_wall = wall;
        state.last_mono = state.last_mono.max(mono);
        (
            SlotReading {
                now: wall,
                slot_ns,
                epoch: state.epoch,
                resync_epoch: state.resync_epoch,
                last_event: state.last_event,
                last_resync: state.last_resync,
            },
            mono,
        )
    }

    /// Read the clock, observing any step since the previous read.
    pub fn read(&self) -> SlotReading {
        self.sample().0
    }

    /// Wall time of a fresh read.
    pub fn now(&self) -> DateTime<Utc> {
        self.read().now
    }

    /// Both clocks from one sample; observes like [`SlotClock::read`].
    pub fn stamp(&self) -> ClockStamp {
        let (reading, mono) = self.sample();
        ClockStamp {
            wall: reading.now,
            mono,
        }
    }

    /// Age of `stamp` at a fresh sample (PAN-289 rule).
    pub fn age_of(&self, stamp: &ClockStamp) -> std::time::Duration {
        stamp.age_at(self.stamp())
    }

    /// The active slot-period source this clock reads.
    pub fn slot_ns_source(&self) -> Arc<AtomicI64> {
        self.inner.slot_ns.clone()
    }

    /// Whether `other` is a handle on the same clock.
    pub fn same_clock(&self, other: &SlotClock) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// Test and simulation aid: wall and monotonic time moved by hand.
#[derive(Clone, Debug)]
pub struct ManualClock {
    wall: Arc<Mutex<DateTime<Utc>>>,
    mono: Arc<Mutex<std::time::Duration>>,
}

impl ManualClock {
    /// Both clocks start at `start` (monotonic zero).
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            wall: Arc::new(Mutex::new(start)),
            mono: Arc::new(Mutex::new(std::time::Duration::ZERO)),
        }
    }

    /// Current wall time.
    pub fn now(&self) -> DateTime<Utc> {
        *self.wall.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Move both clocks forward by `by`.
    pub fn advance(&self, by: std::time::Duration) {
        let mut wall = self.wall.lock().unwrap_or_else(|p| p.into_inner());
        let mut mono = self.mono.lock().unwrap_or_else(|p| p.into_inner());
        *wall += Duration::from_std(by).unwrap_or(Duration::MAX);
        *mono += by;
    }

    /// Move the wall clock alone: a step, or a suspend on Linux/macOS.
    pub fn step_wall(&self, by: Duration) {
        let mut wall = self.wall.lock().unwrap_or_else(|p| p.into_inner());
        *wall += by;
    }

    /// The wall source.
    pub fn wall(&self) -> UtcClock {
        let wall = self.wall.clone();
        Arc::new(move || *wall.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// The monotonic source.
    pub fn mono(&self) -> MonoClock {
        let mono = self.mono.clone();
        Arc::new(move || *mono.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// A [`SlotClock`] with step detection over these sources.
    pub fn slot_clock(&self, slot_ns: Arc<AtomicI64>) -> SlotClock {
        SlotClock::new(self.wall(), self.mono(), slot_ns)
    }
}

#[cfg(test)]
#[path = "slot_clock/tests/slot_clock_tests.rs"]
mod slot_clock_tests;

#[cfg(test)]
#[path = "slot_clock/tests/clock_stamp_tests.rs"]
mod clock_stamp_tests;

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
