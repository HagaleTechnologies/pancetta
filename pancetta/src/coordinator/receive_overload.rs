//! Pure receive-window policy, shared PTT admission and bounded CAT transaction.

/// Identity of a capture. The capture epoch changes on retunes (including an
/// away-and-back retune), mode/clock changes and audio discontinuities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub dial_hz: u64,
    pub connection_generation: u64,
    pub gain_epoch: u64,
    pub capture_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowValidity {
    /// A complete, continuous capture with confirmed receive throughout pre-roll.
    Receive,
    /// An explicitly observed TX window. It does not break a receive streak.
    KnownTx,
    /// Incomplete capture, unknown PTT overlap, or a mixed-gain capture.
    Invalid,
}

#[derive(Debug, Clone, Copy)]
pub struct Measurement {
    /// FT8 UTC source-slot index, not arrival time or decoder sequence number.
    pub slot: i64,
    pub identity: Identity,
    /// Silence is negative infinity; None means empty or invalid samples.
    pub peak_dbfs: Option<f64>,
    pub validity: WindowValidity,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingStep {
    pub identity: Identity,
    pub source_slot: i64,
    pub peak_dbfs: f64,
}

#[derive(Debug, Default)]
pub struct OverloadPolicy {
    identity: Option<Identity>,
    last_slot: Option<i64>,
    hot: u8,
    cool: u8,
    hot_peak: f64,
    pending: Option<PendingStep>,
    maximum_notified: bool,
}

impl OverloadPolicy {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// All expected source slots must be observed, including known TX slots.
    /// A missing record resets pending work as well as the counters.
    pub fn observe(&mut self, measurement: Measurement) {
        if measurement.identity.dial_hz == 0 {
            self.reset();
            return;
        }
        if self.identity != Some(measurement.identity) {
            self.reset();
            self.identity = Some(measurement.identity);
        }
        if let Some(last) = self.last_slot {
            if measurement.slot <= last {
                // Duplicate or delayed measurements cannot fabricate a streak.
                return;
            }
            if last.checked_add(1) != Some(measurement.slot) {
                self.reset();
                self.identity = Some(measurement.identity);
            }
        }
        self.last_slot = Some(measurement.slot);
        if measurement.validity == WindowValidity::KnownTx {
            return;
        }
        let peak = match measurement.peak_dbfs {
            Some(peak) if peak.is_finite() || peak == f64::NEG_INFINITY => peak,
            _ => {
                self.clear_episode();
                return;
            }
        };
        if measurement.validity != WindowValidity::Receive {
            self.clear_episode();
            return;
        }
        if peak < -6.0 {
            self.cool = self.cool.saturating_add(1);
            if self.cool >= 3 {
                self.clear_episode();
            }
        } else {
            self.cool = 0;
        }
        if peak >= -1.0 {
            if self.hot == 0 {
                self.hot_peak = peak;
            } else {
                self.hot_peak = self.hot_peak.max(peak);
            }
            self.hot = self.hot.saturating_add(1);
            if self.hot >= 3 && self.pending.is_none() && !self.maximum_notified {
                self.pending = Some(PendingStep {
                    identity: measurement.identity,
                    source_slot: measurement.slot,
                    peak_dbfs: self.hot_peak,
                });
            }
        } else {
            self.hot = 0;
            self.hot_peak = f64::NEG_INFINITY;
        }
    }

    pub fn pending(&self) -> Option<PendingStep> {
        self.pending
    }

    /// Only call after the setter acknowledgement and matching ATT readback.
    pub fn applied(&mut self) {
        self.clear_episode();
    }

    /// Consume a maximum-setting outcome. The caller emits its warning only
    /// when this returns true. Three cool windows permit a new episode.
    pub fn at_maximum(&mut self) -> bool {
        self.pending = None;
        let first = !self.maximum_notified;
        self.maximum_notified = true;
        self.hot = 0;
        first
    }

    fn clear_episode(&mut self) {
        self.hot = 0;
        self.cool = 0;
        self.hot_peak = f64::NEG_INFINITY;
        self.pending = None;
        self.maximum_notified = false;
    }
}

/// Raw captured audio, before software gain. Do not clamp values above one.
pub fn raw_peak_dbfs(samples: &[f32]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut peak = 0.0_f64;
    for &sample in samples {
        if !sample.is_finite() {
            return None;
        }
        peak = peak.max(f64::from(sample).abs());
    }
    Some(20.0 * peak.log10())
}

/// No model table or guessed step. Only nonnegative advertised steps count.
pub fn next_attenuator(current: i32, steps: &[i32]) -> Option<i32> {
    if current < 0 {
        return None;
    }
    steps
        .iter()
        .copied()
        .filter(|&step| step >= 0 && step > current)
        .min()
}

/// Shared radio gain/PTT admission. CAT operations must run after releasing the
/// coordinator's synchronous PTT gate. Unknown hardware state starts inhibited.
#[derive(Debug)]
pub struct GainSafety {
    state: std::sync::atomic::AtomicU8,
    pending_on: std::sync::atomic::AtomicU64,
    epoch: std::sync::atomic::AtomicU64,
    last_activity_ms: std::sync::atomic::AtomicI64,
    last_gain_ms: std::sync::atomic::AtomicI64,
}

impl Default for GainSafety {
    fn default() -> Self {
        Self {
            state: std::sync::atomic::AtomicU8::new(1),
            pending_on: std::sync::atomic::AtomicU64::new(0),
            epoch: std::sync::atomic::AtomicU64::new(0),
            last_activity_ms: std::sync::atomic::AtomicI64::new(i64::MAX),
            last_gain_ms: std::sync::atomic::AtomicI64::new(i64::MIN),
        }
    }
}

impl GainSafety {
    /// Returns true only after registering a pending hardware key command.
    /// Pair each true result with ptt_command_consumed, including enqueue errors.
    pub fn admit_ptt(&self, now_ms: i64) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.pending_on.fetch_add(1, SeqCst);
        let admitted = match self.state.compare_exchange(0, 1, SeqCst, SeqCst) {
            Ok(_) | Err(1) => true,
            Err(_) => false,
        };
        if admitted {
            self.touch(now_ms);
        } else {
            self.pending_on.fetch_sub(1, SeqCst);
        }
        admitted
    }

    pub fn ptt_command_consumed(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        // Saturation also allows existing callers that refuse before admission.
        let _ = self
            .pending_on
            .fetch_update(SeqCst, SeqCst, |count| count.checked_sub(1));
    }

    /// Only a hardware-acknowledged unkey may establish receive state. A queued
    /// key after that unkey remains an intent even before hardware handles it.
    pub fn confirmed_off(&self, now_ms: i64) {
        use std::sync::atomic::Ordering::SeqCst;
        self.touch(now_ms);
        if self.pending_on.load(SeqCst) == 0 {
            let _ = self.state.compare_exchange(1, 0, SeqCst, SeqCst);
        }
    }

    pub fn reserve(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        if self.pending_on.load(SeqCst) != 0
            || self.state.compare_exchange(0, 2, SeqCst, SeqCst).is_err()
        {
            return false;
        }
        if self.pending_on.load(SeqCst) != 0 {
            let _ = self.state.compare_exchange(2, 0, SeqCst, SeqCst);
            return false;
        }
        true
    }

    /// No setter was sent, so captured audio retains its gain identity.
    pub fn cancel_gain(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        let _ = self.state.compare_exchange(2, 0, SeqCst, SeqCst);
    }

    pub fn finish_gain(&self, now_ms: i64) {
        use std::sync::atomic::Ordering::SeqCst;
        self.last_gain_ms.store(now_ms, SeqCst);
        self.touch(now_ms);
        self.epoch.fetch_add(1, SeqCst);
        let _ = self.state.compare_exchange(2, 0, SeqCst, SeqCst);
    }

    pub fn fail_gain(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.state.store(3, SeqCst);
        self.epoch.fetch_add(1, SeqCst);
    }

    /// Recovery requires both a fresh confirmed PTT Off and stable ATT read.
    /// The owner must call this only after those hardware operations succeed.
    pub fn recover_gain(&self, now_ms: i64) {
        use std::sync::atomic::Ordering::SeqCst;
        self.last_gain_ms.store(now_ms, SeqCst);
        self.touch(now_ms);
        self.epoch.fetch_add(1, SeqCst);
        if self.pending_on.load(SeqCst) == 0 {
            let _ = self.state.compare_exchange(3, 0, SeqCst, SeqCst);
        }
    }

    pub fn blocked(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::SeqCst) >= 2
    }

    pub fn receiving(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.state.load(SeqCst) == 0 && self.pending_on.load(SeqCst) == 0
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn last_activity_ms(&self) -> i64 {
        self.last_activity_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn touch(&self, now_ms: i64) {
        use std::sync::atomic::Ordering::SeqCst;
        self.last_activity_ms.store(now_ms, SeqCst);
    }

    /// The serialized CAT owner checks even commands from internal/raw callers.
    pub fn hardware_ptt_on(&self, now_ms: i64) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        match self.state.compare_exchange(0, 1, SeqCst, SeqCst) {
            Ok(_) | Err(1) => {
                self.touch(now_ms);
                true
            }
            Err(_) => false,
        }
    }

    /// Classify the entire capture interval, including pre-roll. A confirmed
    /// unkey after capture began still contaminates the window, even if the
    /// rig is receiving by the time DSP emits it.
    pub fn window_validity(&self, capture_start_ms: i64) -> WindowValidity {
        use std::sync::atomic::Ordering::SeqCst;
        let activity = self.last_activity_ms();
        if self.blocked()
            || activity == i64::MAX
            || self.last_gain_ms.load(SeqCst) >= capture_start_ms
        {
            WindowValidity::Invalid
        } else if !self.receiving() || activity >= capture_start_ms {
            WindowValidity::KnownTx
        } else {
            WindowValidity::Receive
        }
    }

    pub fn gain_reserved(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::SeqCst) == 2
    }
}

/// Capture-boundary admission with an absolute deadline before FT8 begins.
/// `slot` is the source-slot index. The returned budget ends 250 ms before
/// slot start, after at most 50 ms admission slippage at capture pre-roll.
pub fn boundary_opportunity(now_ms: i64, lead_ms: i64) -> Option<(i64, u64)> {
    if lead_ms <= 250 {
        return None;
    }
    let shifted = now_ms.checked_add(lead_ms)?;
    if shifted.rem_euclid(15_000) > 50 {
        return None;
    }
    let slot = shifted.div_euclid(15_000);
    let deadline_ms = slot.checked_mul(15_000)?.checked_sub(250)?;
    let budget_ms = deadline_ms.checked_sub(now_ms)?;
    Some((slot, u64::try_from(budget_ms).ok()?)).filter(|(_, budget)| *budget > 0)
}

/// Result of an acknowledged transaction. Enqueueing never produces Increased.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Increased { old: i32, new: i32 },
    Maximum { current: i32 },
    Deferred,
}

/// Execute within a reservation acquired under the coordinator PTT gate.
/// The caller owns boundary admission; this helper enforces the absolute CAT
/// deadline and rechecks the live authorization immediately before the setter.
#[cfg(feature = "pancetta-hamlib")]
pub async fn execute_step(
    rig: &dyn pancetta_hamlib::RigControl,
    safety: &GainSafety,
    steps: &[i32],
    deadline: tokio::time::Instant,
    authorize: impl Fn() -> bool,
) -> anyhow::Result<StepOutcome> {
    use pancetta_hamlib::{PttState, Vfo};
    let now_ms = || chrono::Utc::now().timestamp_millis();
    if !safety.gain_reserved() {
        return Ok(StepOutcome::Deferred);
    }
    if tokio::time::Instant::now() >= deadline || !authorize() {
        safety.cancel_gain();
        return Ok(StepOutcome::Deferred);
    }
    let operation = async {
        if rig.get_ptt(Vfo::Current).await? != PttState::Off {
            safety.cancel_gain();
            safety.hardware_ptt_on(now_ms());
            return Ok(StepOutcome::Deferred);
        }
        let old = rig.get_attenuator().await?;
        anyhow::ensure!(old >= 0, "negative attenuator readback");
        if !authorize() || !safety.gain_reserved() || tokio::time::Instant::now() >= deadline {
            return Ok(StepOutcome::Deferred);
        }
        let Some(new) = next_attenuator(old, steps) else {
            return Ok(StepOutcome::Maximum { current: old });
        };
        rig.set_attenuator(new).await?;
        let observed = rig.get_attenuator().await?;
        anyhow::ensure!(
            observed == new,
            "attenuator readback mismatch: wanted {new}, read {observed}"
        );
        anyhow::ensure!(
            safety.gain_reserved(),
            "gain reservation lost before confirmation"
        );
        Ok(StepOutcome::Increased { old, new })
    };
    match tokio::time::timeout_at(deadline, operation).await {
        Ok(Ok(outcome)) => {
            if matches!(outcome, StepOutcome::Increased { .. }) {
                safety.finish_gain(now_ms());
            } else {
                safety.cancel_gain();
            }
            Ok(outcome)
        }
        result => {
            safety.fail_gain();
            // Closing the socket discards uncertain replies. It cannot cancel a
            // physical command the radio has already accepted.
            let _ = rig.disconnect().await;
            match result {
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            dial_hz: 7_074_000,
            connection_generation: 1,
            gain_epoch: 0,
            capture_epoch: 0,
        }
    }

    fn window(slot: i64, db: f64) -> Measurement {
        Measurement {
            slot,
            identity: identity(),
            peak_dbfs: Some(db),
            validity: WindowValidity::Receive,
        }
    }

    #[test]
    fn three_hot_receive_windows_request_one_step() {
        let mut policy = OverloadPolicy::default();
        policy.observe(window(0, -1.0));
        policy.observe(window(1, -0.5));
        assert!(policy.pending().is_none());
        policy.observe(window(2, -0.8));
        assert_eq!(policy.pending().unwrap().peak_dbfs, -0.5);
        policy.observe(window(3, 0.0));
        assert_eq!(policy.pending().unwrap().source_slot, 2);
    }
    fn trip(policy: &mut OverloadPolicy, first: i64) {
        for slot in first..first + 3 {
            policy.observe(window(slot, -0.5));
        }
        assert!(policy.pending().is_some());
    }

    #[test]
    fn raw_peak_uses_negative_samples_and_preserves_overrange() {
        assert_eq!(raw_peak_dbfs(&[-1.0, 0.1]), Some(0.0));
        assert!(raw_peak_dbfs(&[-2.0]).unwrap() > 6.0);
        assert_eq!(raw_peak_dbfs(&[0.0; 4]), Some(f64::NEG_INFINITY));
        assert_eq!(raw_peak_dbfs(&[]), None);
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(raw_peak_dbfs(&[0.9, invalid]), None);
        }
    }

    #[test]
    fn reference_audio_does_not_trip() {
        for amplitude in [0.25, 0.28] {
            let peak = raw_peak_dbfs(&[amplitude]).unwrap();
            assert!(peak < -11.0);
            let mut policy = OverloadPolicy::default();
            for slot in 0..8 {
                policy.observe(window(slot, peak));
            }
            assert!(policy.pending().is_none());
        }
    }

    #[test]
    fn trip_threshold_is_inclusive_and_requires_consecutive_hot_windows() {
        let mut policy = OverloadPolicy::default();
        policy.observe(window(0, -1.0));
        policy.observe(window(1, -1.0));
        policy.observe(window(2, -1.00001));
        policy.observe(window(3, -1.0));
        policy.observe(window(4, -1.0));
        assert!(policy.pending().is_none());
        policy.observe(window(5, -1.0));
        assert!(policy.pending().is_some());
    }

    #[test]
    fn cool_threshold_is_strict_and_three_cool_windows_cancel_deferred_step() {
        let mut policy = OverloadPolicy::default();
        trip(&mut policy, 0);
        for slot in 3..7 {
            policy.observe(window(slot, -6.0));
        }
        assert!(policy.pending().is_some());
        policy.observe(window(7, -6.01));
        policy.observe(window(8, -6.01));
        assert!(policy.pending().is_some());
        policy.observe(window(9, -6.01));
        assert!(policy.pending().is_none());
        for slot in 10..15 {
            policy.observe(window(slot, -3.0));
        }
        assert!(policy.pending().is_none());
    }

    #[test]
    fn successful_step_requires_three_fresh_hot_windows() {
        let mut policy = OverloadPolicy::default();
        trip(&mut policy, 0);
        policy.applied();
        for slot in 3..5 {
            policy.observe(window(slot, -0.5));
        }
        assert!(policy.pending().is_none());
        policy.observe(window(5, -0.5));
        assert_eq!(policy.pending().unwrap().source_slot, 5);
    }

    #[test]
    fn nonuniform_steps_only_move_up() {
        let steps = [20, 0, 10, 10, -1];
        assert_eq!(next_attenuator(0, &steps), Some(10));
        assert_eq!(next_attenuator(12, &steps), Some(20));
        assert_eq!(next_attenuator(20, &steps), None);
        assert_eq!(next_attenuator(-1, &steps), None);
        assert_eq!(next_attenuator(0, &[]), None);
    }

    #[test]
    fn maximum_notice_is_once_per_overload_episode() {
        let mut policy = OverloadPolicy::default();
        trip(&mut policy, 0);
        assert!(policy.at_maximum());
        for slot in 3..8 {
            policy.observe(window(slot, 0.0));
        }
        assert!(policy.pending().is_none());
        assert!(!policy.at_maximum());
        for slot in 8..11 {
            policy.observe(window(slot, -10.0));
        }
        trip(&mut policy, 11);
        assert!(policy.at_maximum());
    }

    #[test]
    fn known_tx_skips_counting_but_missing_receive_breaks_streak() {
        let mut policy = OverloadPolicy::default();
        policy.observe(window(0, -0.5));
        let mut tx = window(1, 0.0);
        tx.validity = WindowValidity::KnownTx;
        policy.observe(tx);
        policy.observe(window(2, -0.5));
        assert!(policy.pending().is_none());
        policy.observe(window(3, -0.5));
        assert!(policy.pending().is_some());
        policy.reset();
        policy.observe(window(0, -0.5));
        policy.observe(window(1, -0.5));
        policy.observe(window(3, -0.5));
        assert!(policy.pending().is_none());
        policy.observe(window(4, -0.5));
        assert!(policy.pending().is_none());
        policy.observe(window(5, -0.5));
        assert!(policy.pending().is_some());
        policy.observe(window(7, -0.5));
        assert!(policy.pending().is_none());
    }

    #[test]
    fn duplicate_and_stale_slots_cannot_advance_or_cancel() {
        let mut policy = OverloadPolicy::default();
        policy.observe(window(10, -0.5));
        policy.observe(window(10, -0.5));
        policy.observe(window(9, -0.5));
        policy.observe(window(11, -0.5));
        assert!(policy.pending().is_none());
        policy.observe(window(12, -0.5));
        assert!(policy.pending().is_some());
        policy.observe(window(9, -10.0));
        assert!(policy.pending().is_some());
    }

    #[test]
    fn incomplete_unknown_ptt_and_nonfinite_captures_cancel() {
        for invalid_peak in [None, Some(f64::NAN), Some(f64::INFINITY)] {
            let mut policy = OverloadPolicy::default();
            trip(&mut policy, 0);
            let mut invalid = window(3, 0.0);
            invalid.peak_dbfs = invalid_peak;
            policy.observe(invalid);
            assert!(policy.pending().is_none());
        }
        let mut policy = OverloadPolicy::default();
        trip(&mut policy, 0);
        let mut invalid = window(3, 0.0);
        invalid.validity = WindowValidity::Invalid;
        policy.observe(invalid);
        assert!(policy.pending().is_none());
        policy.observe(window(4, 0.0));
        policy.observe(window(5, 0.0));
        assert!(policy.pending().is_none());
    }

    #[test]
    fn every_provenance_change_cancels_pending() {
        let base = identity();
        for changed in [
            Identity {
                dial_hz: 14_074_000,
                ..base
            },
            Identity { dial_hz: 0, ..base },
            Identity {
                connection_generation: 2,
                ..base
            },
            Identity {
                gain_epoch: 1,
                ..base
            },
            Identity {
                capture_epoch: 1,
                ..base
            },
        ] {
            let mut policy = OverloadPolicy::default();
            trip(&mut policy, 0);
            let mut next = window(3, -0.5);
            next.identity = changed;
            policy.observe(next);
            assert!(policy.pending().is_none());
        }
    }

    #[test]
    fn invalid_windows_do_not_clear_an_overload_episode_with_cool_audio() {
        let mut policy = OverloadPolicy::default();
        trip(&mut policy, 0);
        for slot in 3..5 {
            policy.observe(window(slot, -20.0));
        }
        let mut tx = window(5, -20.0);
        tx.validity = WindowValidity::KnownTx;
        policy.observe(tx);
        assert!(policy.pending().is_some());
        policy.observe(window(6, f64::NEG_INFINITY));
        assert!(policy.pending().is_none());
    }
    #[test]
    fn gain_first_refuses_all_key_intent_until_readback() {
        let safety = GainSafety::default();
        assert!(!safety.receiving());
        assert!(!safety.reserve());
        safety.confirmed_off(100);
        assert!(safety.reserve());
        assert!(safety.blocked());
        assert!(!safety.admit_ptt(101));
        // PTT off is permitted but cannot release a gain reservation.
        safety.confirmed_off(102);
        assert!(safety.blocked());
        safety.finish_gain(103);
        assert!(!safety.blocked());
        assert!(safety.receiving());
        assert_eq!(safety.last_activity_ms(), 103);
    }

    #[test]
    fn ptt_first_defers_gain_through_hardware_unkey() {
        let safety = GainSafety::default();
        safety.confirmed_off(100);
        assert!(safety.admit_ptt(101));
        assert!(!safety.reserve());
        safety.ptt_command_consumed();
        assert!(!safety.reserve());
        safety.confirmed_off(102);
        assert!(safety.reserve());
    }

    #[test]
    fn earlier_unkey_cannot_erase_later_queued_key() {
        let safety = GainSafety::default();
        safety.confirmed_off(100);
        assert!(safety.admit_ptt(101));
        safety.ptt_command_consumed();
        assert!(safety.admit_ptt(102));
        // This acknowledgement belongs to an earlier T0, before queued T1.
        safety.confirmed_off(103);
        assert!(!safety.receiving());
        assert!(!safety.reserve());
        safety.ptt_command_consumed();
        assert!(!safety.reserve());
        safety.confirmed_off(104);
        assert!(safety.reserve());
    }

    #[test]
    fn failed_gain_stays_inhibited_until_confirmed_recovery() {
        let safety = GainSafety::default();
        safety.confirmed_off(100);
        assert!(safety.reserve());
        let epoch = safety.epoch();
        safety.fail_gain();
        assert!(safety.epoch() > epoch);
        assert!(safety.blocked());
        assert!(!safety.admit_ptt(101));
        safety.confirmed_off(102);
        assert!(safety.blocked());
        safety.finish_gain(103);
        assert!(safety.blocked());
        safety.recover_gain(104);
        assert!(safety.receiving());
    }

    #[test]
    fn concurrent_gain_and_ptt_have_only_one_winner() {
        use std::sync::{Arc, Barrier};
        for _ in 0..100 {
            let safety = Arc::new(GainSafety::default());
            safety.confirmed_off(100);
            let barrier = Arc::new(Barrier::new(2));
            let ptt_safety = Arc::clone(&safety);
            let ptt_barrier = Arc::clone(&barrier);
            let key = std::thread::spawn(move || {
                ptt_barrier.wait();
                ptt_safety.admit_ptt(101)
            });
            barrier.wait();
            let gain = safety.reserve();
            let ptt = key.join().unwrap();
            assert!(!(gain && ptt));
            if ptt {
                safety.ptt_command_consumed();
            }
        }
    }
    #[test]
    fn hardware_ptt_writers_must_respect_gain_reservation() {
        let safety = GainSafety::default();
        safety.confirmed_off(100);
        let epoch = safety.epoch();
        assert!(safety.hardware_ptt_on(101));
        safety.confirmed_off(102);
        assert_eq!(
            safety.epoch(),
            epoch,
            "ordinary TX must not change gain epoch"
        );
        assert!(safety.reserve());
        assert!(safety.gain_reserved());
        assert!(!safety.hardware_ptt_on(103));
        safety.finish_gain(104);
        assert!(!safety.gain_reserved());
        assert!(safety.epoch() > epoch);
    }
    #[test]
    fn capture_boundary_is_admitted_only_within_fifty_milliseconds() {
        assert_eq!(boundary_opportunity(14_500, 500), Some((1, 250)));
        assert_eq!(boundary_opportunity(14_550, 500), Some((1, 200)));
        assert_eq!(boundary_opportunity(14_551, 500), None);
        assert_eq!(boundary_opportunity(14_499, 500), None);
        assert_eq!(boundary_opportunity(14_000, 1000), Some((1, 750)));
        assert_eq!(boundary_opportunity(14_050, 1000), Some((1, 700)));
        assert_eq!(boundary_opportunity(14_051, 1000), None);
    }

    #[test]
    fn boundary_budget_always_leaves_settling_headroom() {
        assert_eq!(boundary_opportunity(14_750, 250), None);
        assert_eq!(boundary_opportunity(14_900, 100), None);
        assert_eq!(boundary_opportunity(i64::MAX, 500), None);
        assert_eq!(boundary_opportunity(-500, 500), Some((0, 250)));
    }
    #[test]
    fn capture_overlap_includes_preroll_and_acknowledged_unkey() {
        let safety = GainSafety::default();
        assert_eq!(safety.window_validity(100), WindowValidity::Invalid);
        safety.confirmed_off(0);
        assert_eq!(safety.window_validity(100), WindowValidity::Receive);
        assert!(safety.admit_ptt(90));
        safety.ptt_command_consumed();
        // Enqueuing an unkey has not acknowledged it.
        assert_eq!(safety.window_validity(100), WindowValidity::KnownTx);
        safety.confirmed_off(101);
        // Even though emission is later, TX overlapped pre-roll.
        assert_eq!(safety.window_validity(100), WindowValidity::KnownTx);
        assert_eq!(safety.window_validity(102), WindowValidity::Receive);
        assert!(safety.reserve());
        safety.finish_gain(105);
        assert_eq!(safety.window_validity(102), WindowValidity::Invalid);
        assert_eq!(safety.window_validity(106), WindowValidity::Receive);
    }
}
