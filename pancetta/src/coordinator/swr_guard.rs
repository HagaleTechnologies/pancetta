//! PAN-251 SWR protection. While the rig is keyed the Hamlib poll loop reads
//! the SWR meter every tick. Valid readings that stay above `max_swr` for
//! `swr_trip_delay_ms` (PAN-262: lets an antenna tuner settle after
//! key-down) halt the station (see `tx_halt`). A rig that cannot report SWR
//! is warned about, never blocked. SWR is only measurable under forward
//! power, so the trip latches in `TxPolicy::Disabled` until the operator
//! presses `g`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{error, warn};

use super::tx_halt::TxHaltHandles;
use crate::message_bus::{
    ComponentId, ComponentMessage, DurableDelivery, MessageBus, MessageType, SendIfOutcome,
};

/// PAN-262: high-SWR time from earlier transmissions stops counting after
/// this long without a high reading, so one bad frame an hour ago doesn't
/// shorten today's tuner-settle allowance.
pub(crate) const SWR_HIGH_STALE_AFTER: Duration = Duration::from_secs(120);
/// Keyed poll ticks (500 ms each) with no valid reading before the
/// runtime "unavailable" alert.
pub(crate) const SWR_UNAVAILABLE_MIN_ATTEMPTS: u32 = 4;
/// Bound on the startup `reports_swr` capability probe.
pub(crate) const SWR_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_MAX_SWR: f32 = 3.0;
const DIAGNOSTIC_TARGET: &str = "rig.swr";

/// What the startup probe learned about the rig's SWR meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwrCapability {
    Supported,
    Unsupported,
    /// The probe failed or timed out; the runtime check decides instead.
    Unknown,
}

/// Why SWR protection is unavailable on this rig.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwrUnavailable {
    /// The backend's capability list has no SWR level.
    NotReported,
    /// A keyed period produced no valid reading.
    NoReadingWhileKeyed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SwrAction {
    None,
    Trip {
        swr: f32,
        max_swr: f32,
    },
    /// Still keyed after a trip: abort and unkey again.
    ReassertUnkey,
    ReportUnavailable(SwrUnavailable),
}

/// A physical SWR reading: finite and at least 1.0:1. Hamlib may report
/// 0 with no RF; that is "no reading", never "perfect match".
pub(crate) fn valid_swr(reading: f32) -> bool {
    reading.is_finite() && reading >= 1.0
}

/// Per-Hamlib-session SWR protection state. Pure: the poll loop feeds it
/// one observation per tick and acts on the returned [`SwrAction`].
#[derive(Debug)]
pub(crate) struct SwrGuard {
    /// `protection.swr_protection`.
    enabled: bool,
    /// Validated `max_swr`, else [`DEFAULT_MAX_SWR`].
    max_swr: f32,
    /// A real CAT rig: availability alerts apply. The mock rig's strip
    /// already says `rig: no CAT`.
    warn_unavailable: bool,
    capability_checked: bool,
    /// A keyed episode is open.
    keyed: bool,
    tripped: bool,
    /// PAN-262: how long valid readings must stay above `max_swr`.
    trip_delay: Duration,
    /// Keyed time spent above the limit, summed across transmissions so
    /// short frames (FT4, FT2) still trip. A valid reading at or below the
    /// limit clears it, as does a gap longer than [`SWR_HIGH_STALE_AFTER`].
    high_keyed: Duration,
    /// The last high reading in the current keyed episode; unkey clears it
    /// so time spent unkeyed never counts.
    last_high_keyed: Option<Instant>,
    /// The last high reading in any episode, for the stale-gap reset.
    last_high_seen: Option<Instant>,
    /// Keyed ticks in this episode.
    attempts: u32,
    /// Valid readings in this episode.
    valid: u32,
    /// One availability alert per session until a valid reading re-arms it.
    unavailable_reported: bool,
}

impl SwrGuard {
    pub(crate) fn new(protection: &pancetta_config::PowerProtectionConfig, real_rig: bool) -> Self {
        let configured = protection.max_swr;
        let max_swr = if configured.is_finite() && configured > 1.0 {
            configured
        } else {
            if protection.swr_protection {
                warn!(
                    target: "rig.swr",
                    "max_swr {} is not a number above 1.0; SWR protection uses {}:1",
                    configured,
                    DEFAULT_MAX_SWR
                );
            }
            DEFAULT_MAX_SWR
        };
        Self {
            enabled: protection.swr_protection,
            max_swr,
            warn_unavailable: real_rig,
            capability_checked: false,
            keyed: false,
            tripped: false,
            trip_delay: Duration::from_millis(u64::from(
                protection
                    .swr_trip_delay_ms
                    .min(pancetta_config::MAX_SWR_TRIP_DELAY_MS),
            )),
            high_keyed: Duration::ZERO,
            last_high_keyed: None,
            last_high_seen: None,
            attempts: 0,
            valid: 0,
            unavailable_reported: false,
        }
    }

    /// The startup probe still has to run for this session.
    pub(crate) fn needs_capability_probe(&self) -> bool {
        self.enabled && self.warn_unavailable && !self.capability_checked
    }

    pub(crate) fn note_capability(&mut self, cap: SwrCapability) -> SwrAction {
        self.capability_checked = true;
        match cap {
            SwrCapability::Unsupported
                if self.enabled && self.warn_unavailable && !self.unavailable_reported =>
            {
                self.unavailable_reported = true;
                SwrAction::ReportUnavailable(SwrUnavailable::NotReported)
            }
            _ => SwrAction::None,
        }
    }

    /// One poll tick. `reading` is `Some` only for a valid reading.
    pub(crate) fn observe(&mut self, keyed: bool, reading: Option<f32>) -> SwrAction {
        self.observe_at(keyed, reading, Instant::now())
    }

    /// [`Self::observe`] with an explicit clock, for tests.
    pub(crate) fn observe_at(
        &mut self,
        keyed: bool,
        reading: Option<f32>,
        now: Instant,
    ) -> SwrAction {
        if !keyed {
            let mut action = SwrAction::None;
            if self.keyed {
                if self.valid > 0 {
                    self.unavailable_reported = false;
                } else if self.enabled
                    && self.warn_unavailable
                    && !self.tripped
                    && self.attempts >= SWR_UNAVAILABLE_MIN_ATTEMPTS
                    && !self.unavailable_reported
                {
                    self.unavailable_reported = true;
                    action = SwrAction::ReportUnavailable(SwrUnavailable::NoReadingWhileKeyed);
                }
            }
            self.keyed = false;
            self.tripped = false;
            self.last_high_keyed = None;
            self.attempts = 0;
            self.valid = 0;
            return action;
        }

        self.keyed = true;
        self.attempts = self.attempts.saturating_add(1);
        if reading.is_some() {
            self.valid = self.valid.saturating_add(1);
        }
        if !self.enabled {
            return SwrAction::None;
        }
        if self.tripped {
            return SwrAction::ReassertUnkey;
        }
        match reading {
            Some(swr) if swr > self.max_swr => {
                if self
                    .last_high_seen
                    .is_some_and(|seen| now.saturating_duration_since(seen) > SWR_HIGH_STALE_AFTER)
                {
                    self.high_keyed = Duration::ZERO;
                }
                if let Some(prev) = self.last_high_keyed {
                    self.high_keyed += now.saturating_duration_since(prev);
                }
                self.last_high_keyed = Some(now);
                self.last_high_seen = Some(now);
                if self.high_keyed >= self.trip_delay {
                    self.tripped = true;
                    self.high_keyed = Duration::ZERO;
                    return SwrAction::Trip {
                        swr,
                        max_swr: self.max_swr,
                    };
                }
            }
            Some(_) => {
                self.high_keyed = Duration::ZERO;
                self.last_high_keyed = None;
                self.last_high_seen = None;
            }
            None => {}
        }
        SwrAction::None
    }
}

/// The readiness-strip alert for a trip.
pub(crate) fn trip_alert(swr: f32, max_swr: f32) -> String {
    format!(
        "High SWR {swr:.2}:1 (limit {max_swr}:1): TX halted. Check the antenna, then press g to \
         re-enable TX"
    )
}

/// The retained Shift+D entry and log line for a trip.
pub(crate) fn trip_detail(swr: f32, max_swr: f32) -> String {
    format!(
        "SWR {swr:.2}:1 read while transmitting is above max_swr {max_swr}:1. Halted: PTT off, \
         frame aborted, autonomous off, active QSOs cancelled, TX policy Disabled. Press g to \
         re-enable TX and a to resume autonomous"
    )
}

/// The readiness-strip alert for a rig that cannot report SWR.
pub(crate) fn unavailable_alert(kind: SwrUnavailable) -> &'static str {
    match kind {
        SwrUnavailable::NotReported => {
            "SWR protection is unavailable on this rig (it does not report SWR over CAT)"
        }
        SwrUnavailable::NoReadingWhileKeyed => {
            "SWR protection is unavailable on this rig (no SWR reading during the last \
             transmission)"
        }
    }
}

/// The retained Shift+D entry and log line for a rig that cannot report SWR.
pub(crate) fn unavailable_detail(kind: SwrUnavailable) -> String {
    let why = match kind {
        SwrUnavailable::NotReported => "this rig does not report SWR over CAT",
        SwrUnavailable::NoReadingWhileKeyed => {
            "no SWR reading arrived during the last transmission"
        }
    };
    format!(
        "swr_protection is on, but {why}. Transmissions are not checked for high SWR. Set \
         [rig.power_control.protection] swr_protection = false to silence this warning"
    )
}

/// Keyed for SWR purposes: the TX worker's PttGuard, or any SetPtt(true)
/// the Hamlib loop accepted (manual PTT included).
pub(crate) async fn poll_sees_keyed(
    ptt_active: &AtomicBool,
    ptt_on_since: &RwLock<Option<Instant>>,
) -> bool {
    ptt_active.load(Ordering::Acquire) || ptt_on_since.read().await.is_some()
}

/// Ask the backend once whether it can read SWR, bounded by
/// [`SWR_PROBE_TIMEOUT`]. A failure or timeout is `Unknown`, never a
/// false "unsupported" alarm.
pub(crate) async fn probe_swr_capability(
    rig: &(dyn pancetta_hamlib::RigControl + Send + Sync),
) -> SwrCapability {
    match tokio::time::timeout(SWR_PROBE_TIMEOUT, rig.reports_swr()).await {
        Ok(Ok(true)) => SwrCapability::Supported,
        Ok(Ok(false)) => SwrCapability::Unsupported,
        _ => SwrCapability::Unknown,
    }
}

/// One poll tick of SWR protection. Best-effort like every meter read:
/// it never affects poll_ok. Reads only while keyed.
pub(crate) async fn swr_poll_step(
    rig: &(dyn pancetta_hamlib::RigControl + Send + Sync),
    guard: &mut SwrGuard,
    keyed: bool,
    bus: &MessageBus,
    halt: &TxHaltHandles,
    display_feed_enabled: &AtomicBool,
) {
    let reading = if keyed {
        match rig.get_swr().await {
            Ok(swr) if valid_swr(swr) => {
                let swr_msg = ComponentMessage::new(
                    ComponentId::Hamlib,
                    ComponentId::Tui,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SwrResponse {
                        swr,
                    }),
                    Instant::now(),
                );
                let _ = bus.send_message(swr_msg).await;
                Some(swr)
            }
            _ => None,
        }
    } else {
        None
    };
    act(
        guard.observe(keyed, reading),
        bus,
        halt,
        display_feed_enabled,
    )
    .await;
}

/// Carry out a guard decision.
pub(crate) async fn act(
    action: SwrAction,
    bus: &MessageBus,
    halt: &TxHaltHandles,
    display_feed_enabled: &AtomicBool,
) {
    match action {
        SwrAction::Trip { swr, max_swr } => {
            // Synchronous: the PTT-on gate is closed before any await.
            halt.halt();
            if super::tx::send_ptt_command(bus, ComponentId::Hamlib, false, || true)
                != SendIfOutcome::Enqueued
            {
                error!(
                    target: "rig.swr",
                    "SWR trip: PTT-off was not enqueued; the TX worker abort and the 14 s PTT \
                     watchdog still unkey"
                );
            }
            // PAN-341: announce the halt to remote service peers — after the
            // safety gate closes and the synchronous PTT-off enqueue, before
            // QSO cancellation and the best-effort TUI sends. Same additive
            // pattern as every other display-feed relay; a no-op when nobody
            // needs the feed.
            super::remote_gateway::relay_to_gateway(
                bus,
                display_feed_enabled,
                ComponentId::Hamlib,
                MessageType::TxPolicyStatus {
                    policy: pancetta_core::TxPolicy::Disabled,
                },
            )
            .await;
            let detail = trip_detail(swr, max_swr);
            error!(target: "rig.swr", "{}", detail);
            if super::tx_halt::cancel_all_qsos(bus, ComponentId::Hamlib).await
                == DurableDelivery::Dropped
            {
                error!(target: "rig.swr", "SWR trip: CancelAllQsos was dropped");
            }
            // Best-effort TUI feed: headless has no TUI channel.
            to_tui(
                bus,
                MessageType::TxPolicyStatus {
                    policy: pancetta_core::TxPolicy::Disabled,
                },
            )
            .await;
            to_tui(
                bus,
                MessageType::Error {
                    component_id: ComponentId::Hamlib,
                    error_message: trip_alert(swr, max_swr),
                    error_code: None,
                },
            )
            .await;
            to_tui(
                bus,
                MessageType::DiagnosticEvent {
                    target: DIAGNOSTIC_TARGET,
                    level: pancetta_core::DiagnosticLevel::Error,
                    text: detail,
                    qso_id: None,
                    callsign: None,
                },
            )
            .await;
        }
        SwrAction::ReassertUnkey => {
            halt.abort_current_tx.store(true, Ordering::Release);
            let _ = super::tx::send_ptt_command(bus, ComponentId::Hamlib, false, || true);
        }
        SwrAction::ReportUnavailable(kind) => {
            let detail = unavailable_detail(kind);
            warn!(target: "rig.swr", "{}", detail);
            to_tui(
                bus,
                MessageType::Error {
                    component_id: ComponentId::Hamlib,
                    error_message: unavailable_alert(kind).to_string(),
                    error_code: None,
                },
            )
            .await;
            to_tui(
                bus,
                MessageType::DiagnosticEvent {
                    target: DIAGNOSTIC_TARGET,
                    level: pancetta_core::DiagnosticLevel::Warn,
                    text: detail,
                    qso_id: None,
                    callsign: None,
                },
            )
            .await;
        }
        SwrAction::None => {}
    }
}

async fn to_tui(bus: &MessageBus, message_type: MessageType) {
    let _ = bus
        .send_message(ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Tui,
            message_type,
            Instant::now(),
        ))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::{QsoMessage, RigControlMessage};
    use anyhow::Result;
    use pancetta_config::PowerProtectionConfig;
    use pancetta_hamlib::{MockRig, MockRigConfig, Mode, PttState, RigControl, RigStatus, Vfo};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, AtomicU8, AtomicUsize};
    use std::sync::Arc;

    /// A guard that trips on the first high reading (delay 0), for the
    /// state-machine tests that predate PAN-262's settle delay.
    fn guard(real_rig: bool) -> SwrGuard {
        let protection = PowerProtectionConfig {
            swr_trip_delay_ms: 0,
            ..Default::default()
        };
        SwrGuard::new(&protection, real_rig)
    }

    fn ms(t0: Instant, millis: u64) -> Instant {
        t0 + Duration::from_millis(millis)
    }

    // ---- PAN-262: tuner settle delay ----

    #[test]
    fn high_swr_while_the_tuner_settles_does_not_trip() {
        let mut g = SwrGuard::new(&PowerProtectionConfig::default(), true);
        let t0 = Instant::now();
        // 6.0:1 for three seconds of 500 ms ticks, then a good match.
        for tick in 0..6 {
            assert_eq!(
                g.observe_at(true, Some(6.0), ms(t0, tick * 500)),
                SwrAction::None,
                "tick {tick}"
            );
        }
        assert_eq!(
            g.observe_at(true, Some(1.4), ms(t0, 3_000)),
            SwrAction::None
        );
        // A later high reading starts a fresh window.
        assert_eq!(
            g.observe_at(true, Some(6.0), ms(t0, 3_500)),
            SwrAction::None
        );
        assert_eq!(
            g.observe_at(true, Some(6.0), ms(t0, 8_000)),
            SwrAction::None
        );
    }

    #[test]
    fn sustained_high_swr_trips_after_the_default_delay() {
        let mut g = SwrGuard::new(&PowerProtectionConfig::default(), true);
        let t0 = Instant::now();
        for tick in 0..10 {
            assert_eq!(
                g.observe_at(true, Some(4.2), ms(t0, tick * 500)),
                SwrAction::None,
                "tick {tick}"
            );
        }
        assert_eq!(g.observe_at(true, Some(4.2), ms(t0, 5_000)), trip_at(4.2));
        assert_eq!(
            g.observe_at(true, Some(1.2), ms(t0, 5_500)),
            SwrAction::ReassertUnkey
        );
    }

    #[test]
    fn missing_readings_neither_reset_nor_extend_the_window() {
        let mut g = SwrGuard::new(&PowerProtectionConfig::default(), true);
        let t0 = Instant::now();
        assert_eq!(g.observe_at(true, Some(4.2), t0), SwrAction::None);
        for tick in 1..10 {
            assert_eq!(
                g.observe_at(true, None, ms(t0, tick * 500)),
                SwrAction::None
            );
        }
        assert_eq!(g.observe_at(true, Some(4.2), ms(t0, 5_000)), trip_at(4.2));
    }

    #[test]
    fn the_trip_delay_is_configurable() {
        let protection = PowerProtectionConfig {
            swr_trip_delay_ms: 2_000,
            ..Default::default()
        };
        let mut g = SwrGuard::new(&protection, true);
        let t0 = Instant::now();
        assert_eq!(g.observe_at(true, Some(4.2), t0), SwrAction::None);
        assert_eq!(
            g.observe_at(true, Some(4.2), ms(t0, 1_500)),
            SwrAction::None
        );
        assert_eq!(g.observe_at(true, Some(4.2), ms(t0, 2_000)), trip_at(4.2));
    }

    #[test]
    fn high_swr_in_short_frames_adds_up_across_transmissions() {
        // FT2 frames are ~3.16 s: no single frame reaches the 5 s delay, so
        // keyed high time must carry from one frame to the next.
        let mut g = SwrGuard::new(&PowerProtectionConfig::default(), true);
        let t0 = Instant::now();
        let mut tripped_at = None;
        'frames: for frame in 0..4u64 {
            let start = frame * 7_500;
            for tick in 0..7u64 {
                let action = g.observe_at(true, Some(4.2), ms(t0, start + tick * 500));
                if action == trip_at(4.2) {
                    tripped_at = Some(frame);
                    break 'frames;
                }
                assert_eq!(action, SwrAction::None, "frame {frame} tick {tick}");
            }
            assert_eq!(
                g.observe_at(false, None, ms(t0, start + 3_500)),
                SwrAction::None
            );
        }
        assert_eq!(
            tripped_at,
            Some(1),
            "3 s of keyed high SWR per frame trips in the second frame"
        );
    }

    #[test]
    fn a_long_quiet_gap_forgets_earlier_high_swr() {
        let mut g = SwrGuard::new(&PowerProtectionConfig::default(), true);
        let t0 = Instant::now();
        for tick in 0..8 {
            assert_eq!(
                g.observe_at(true, Some(4.2), ms(t0, tick * 500)),
                SwrAction::None
            );
        }
        assert_eq!(g.observe_at(false, None, ms(t0, 4_000)), SwrAction::None);
        // Three minutes later the earlier 3.5 s no longer counts.
        let later = 4_000 + 180_000;
        for tick in 0..8 {
            assert_eq!(
                g.observe_at(true, Some(4.2), ms(t0, later + tick * 500)),
                SwrAction::None,
                "tick {tick}"
            );
        }
    }

    #[test]
    fn unkeyed_time_does_not_count_toward_the_delay() {
        let mut g = SwrGuard::new(&PowerProtectionConfig::default(), true);
        let t0 = Instant::now();
        assert_eq!(g.observe_at(true, Some(4.2), t0), SwrAction::None);
        assert_eq!(g.observe_at(false, None, ms(t0, 4_000)), SwrAction::None);
        assert_eq!(
            g.observe_at(true, Some(4.2), ms(t0, 4_500)),
            SwrAction::None
        );
        assert_eq!(
            g.observe_at(true, Some(4.2), ms(t0, 9_000)),
            SwrAction::None
        );
        assert_eq!(g.observe_at(true, Some(4.2), ms(t0, 9_500)), trip_at(4.2));
    }

    fn trip_at(swr: f32) -> SwrAction {
        SwrAction::Trip { swr, max_swr: 3.0 }
    }

    // ---- state machine ----

    #[test]
    fn one_valid_reading_above_the_limit_trips() {
        let mut g = guard(true);
        assert_eq!(g.observe(true, Some(4.2)), trip_at(4.2));
    }

    #[test]
    fn a_reading_at_the_limit_does_not_trip() {
        let mut g = guard(true);
        assert_eq!(g.observe(true, Some(3.0)), SwrAction::None);
    }

    #[test]
    fn invalid_readings_never_trip() {
        // The poll step only passes valid readings; anything else is None.
        for raw in [f32::NAN, f32::INFINITY, 0.0, 0.5] {
            assert!(!valid_swr(raw), "{raw} must not count as a reading");
            let mut g = guard(true);
            let reading = Some(raw).filter(|v| valid_swr(*v));
            assert_eq!(g.observe(true, reading), SwrAction::None, "{raw}");
        }
        let mut g = guard(true);
        assert_eq!(g.observe(true, None), SwrAction::None);
    }

    #[test]
    fn protection_off_never_trips_or_reports() {
        let protection = PowerProtectionConfig {
            swr_protection: false,
            ..Default::default()
        };
        let mut g = SwrGuard::new(&protection, true);
        assert!(!g.needs_capability_probe());
        assert_eq!(
            g.note_capability(SwrCapability::Unsupported),
            SwrAction::None
        );
        for _ in 0..3 {
            for reading in [Some(9.0), None, None, None, None, Some(4.2)] {
                assert_eq!(g.observe(true, reading), SwrAction::None);
            }
            assert_eq!(g.observe(false, None), SwrAction::None);
            for _ in 0..6 {
                assert_eq!(g.observe(true, None), SwrAction::None);
            }
            assert_eq!(g.observe(false, None), SwrAction::None);
        }
    }

    #[test]
    fn a_trip_latches_and_reasserts_unkey_until_unkeyed() {
        let mut g = guard(true);
        assert_eq!(g.observe(true, Some(4.2)), trip_at(4.2));
        assert_eq!(g.observe(true, Some(1.2)), SwrAction::ReassertUnkey);
        assert_eq!(g.observe(true, None), SwrAction::ReassertUnkey);
    }

    #[test]
    fn unkey_rearms_the_trip() {
        let mut g = guard(true);
        assert_eq!(g.observe(true, Some(4.2)), trip_at(4.2));
        assert_eq!(g.observe(false, None), SwrAction::None);
        assert_eq!(g.observe(true, Some(4.0)), trip_at(4.0));
    }

    #[test]
    fn startup_unsupported_reports_once() {
        let mut g = guard(true);
        assert!(g.needs_capability_probe());
        assert_eq!(
            g.note_capability(SwrCapability::Unsupported),
            SwrAction::ReportUnavailable(SwrUnavailable::NotReported)
        );
        assert!(!g.needs_capability_probe());
        for _ in 0..6 {
            assert_eq!(g.observe(true, None), SwrAction::None);
        }
        assert_eq!(g.observe(false, None), SwrAction::None);
    }

    #[test]
    fn startup_unknown_defers_to_the_runtime_check() {
        let mut g = guard(true);
        assert_eq!(g.note_capability(SwrCapability::Unknown), SwrAction::None);
        assert!(!g.needs_capability_probe());
        for _ in 0..SWR_UNAVAILABLE_MIN_ATTEMPTS {
            assert_eq!(g.observe(true, None), SwrAction::None);
        }
        assert_eq!(
            g.observe(false, None),
            SwrAction::ReportUnavailable(SwrUnavailable::NoReadingWhileKeyed)
        );
    }

    #[test]
    fn a_short_transmission_does_not_report_unavailable() {
        let mut g = guard(true);
        for _ in 0..3 {
            assert_eq!(g.observe(true, None), SwrAction::None);
        }
        assert_eq!(g.observe(false, None), SwrAction::None);
    }

    #[test]
    fn a_valid_reading_rearms_the_unavailable_alert() {
        let mut g = guard(true);
        let silent_episode = |g: &mut SwrGuard| {
            for _ in 0..SWR_UNAVAILABLE_MIN_ATTEMPTS {
                assert_eq!(g.observe(true, None), SwrAction::None);
            }
            g.observe(false, None)
        };
        let reported = SwrAction::ReportUnavailable(SwrUnavailable::NoReadingWhileKeyed);
        assert_eq!(silent_episode(&mut g), reported);
        // Latched: a second silent episode does not report again.
        assert_eq!(silent_episode(&mut g), SwrAction::None);
        // One valid reading re-arms it.
        assert_eq!(g.observe(true, Some(1.3)), SwrAction::None);
        assert_eq!(g.observe(false, None), SwrAction::None);
        assert_eq!(silent_episode(&mut g), reported);
    }

    #[test]
    fn mock_rig_sessions_never_report_unavailable() {
        let mut g = guard(false);
        assert!(!g.needs_capability_probe());
        for _ in 0..10 {
            assert_eq!(g.observe(true, None), SwrAction::None);
        }
        assert_eq!(g.observe(false, None), SwrAction::None);
    }

    #[test]
    fn an_invalid_in_code_threshold_falls_back_to_three() {
        let protection = PowerProtectionConfig {
            max_swr: f32::NAN,
            swr_trip_delay_ms: 0,
            ..Default::default()
        };
        let mut g = SwrGuard::new(&protection, true);
        assert_eq!(g.observe(true, Some(2.9)), SwrAction::None);
        assert_eq!(g.observe(true, Some(3.1)), trip_at(3.1));
    }

    // ---- text ----

    #[test]
    fn trip_alert_text_matches_the_design() {
        assert_eq!(
            trip_alert(4.2, 3.0),
            "High SWR 4.20:1 (limit 3:1): TX halted. Check the antenna, then press g to \
             re-enable TX"
        );
        assert_eq!(
            trip_detail(4.2, 3.0),
            "SWR 4.20:1 read while transmitting is above max_swr 3:1. Halted: PTT off, frame \
             aborted, autonomous off, active QSOs cancelled, TX policy Disabled. Press g to \
             re-enable TX and a to resume autonomous"
        );
    }

    #[test]
    fn unavailable_alert_texts_name_the_rig() {
        for kind in [
            SwrUnavailable::NotReported,
            SwrUnavailable::NoReadingWhileKeyed,
        ] {
            assert!(
                unavailable_alert(kind).starts_with("SWR protection is unavailable on this rig ("),
                "{kind:?}"
            );
            assert!(unavailable_detail(kind).starts_with("swr_protection is on, but "));
        }
    }

    // ---- poll step and probe (bus level) ----

    struct Inboxes {
        bus: MessageBus,
        qso: crossbeam_channel::Receiver<ComponentMessage>,
        tui: crossbeam_channel::Receiver<ComponentMessage>,
        hamlib: crossbeam_channel::Receiver<ComponentMessage>,
        gateway: crossbeam_channel::Receiver<ComponentMessage>,
    }

    async fn inboxes() -> Inboxes {
        let bus = MessageBus::new(16).unwrap();
        let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
        let (_, tui) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let (_, hamlib) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let (_, gateway) = bus
            .create_channel(ComponentId::RemoteGateway)
            .await
            .unwrap();
        Inboxes {
            bus,
            qso,
            tui,
            hamlib,
            gateway,
        }
    }

    fn feed_on() -> AtomicBool {
        AtomicBool::new(true)
    }

    fn drain(rx: &crossbeam_channel::Receiver<ComponentMessage>) -> Vec<MessageType> {
        rx.try_iter().map(|m| m.message_type).collect()
    }

    fn halt_handles() -> TxHaltHandles {
        TxHaltHandles {
            abort_current_tx: Arc::new(AtomicBool::new(false)),
            autonomous_enabled: Arc::new(AtomicBool::new(true)),
            tx_policy: Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8())),
        }
    }

    fn policy(h: &TxHaltHandles) -> pancetta_core::TxPolicy {
        pancetta_core::TxPolicy::from_u8(h.tx_policy.load(Ordering::Acquire))
    }

    async fn connected_mock(config: MockRigConfig) -> MockRig {
        let rig = MockRig::new(MockRigConfig {
            connection_delay_ms: 0,
            operation_delay_ms: 0,
            ..config
        });
        rig.connect().await.unwrap();
        rig
    }

    fn hard_mute_reason(h: &TxHaltHandles) -> Option<&'static str> {
        let pending_frequency: Arc<std::sync::Mutex<HashMap<Vfo, ComponentMessage>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        super::super::tx::tx_hard_mute_reason(
            &h.tx_policy,
            &Arc::new(AtomicU32::new(0)),
            &Arc::new(AtomicBool::new(true)),
            &pending_frequency,
            &Arc::new(std::sync::Mutex::new(None)),
            &Arc::new(AtomicU32::new(0)),
            &Arc::new(AtomicU8::new(
                super::super::hamlib::RigConnState::Connected.as_u8(),
            )),
            &Arc::new(AtomicBool::new(true)),
            &Arc::default(),
            &pancetta_hamlib::DataModeReadiness::new(false),
        )
    }

    #[tokio::test]
    async fn step_trip_halts_unkeys_cancels_and_reports() {
        let ib = inboxes().await;
        let rig = connected_mock(MockRigConfig {
            swr_override: Some(4.2),
            ..Default::default()
        })
        .await;
        let halt = halt_handles();
        assert_eq!(hard_mute_reason(&halt), None);
        let mut g = guard(false);

        swr_poll_step(&rig, &mut g, true, &ib.bus, &halt, &feed_on()).await;

        assert!(halt.abort_current_tx.load(Ordering::Acquire));
        assert!(!halt.autonomous_enabled.load(Ordering::Acquire));
        assert_eq!(policy(&halt), pancetta_core::TxPolicy::Disabled);
        assert_eq!(hard_mute_reason(&halt), Some("TX policy is Disabled"));

        let hamlib = drain(&ib.hamlib);
        assert!(
            hamlib.iter().any(|m| matches!(
                m,
                MessageType::RigControl(RigControlMessage::SetPtt { state: false, .. })
            )),
            "{hamlib:?}"
        );
        let qso = drain(&ib.qso);
        assert!(
            qso.iter()
                .any(|m| matches!(m, MessageType::QsoMessage(QsoMessage::CancelAllQsos))),
            "{qso:?}"
        );
        // PAN-341: the trip also relays the Disabled policy to the display
        // feed (addressed to RemoteGateway, source Hamlib).
        let gateway = drain(&ib.gateway);
        assert!(
            gateway.iter().any(|m| matches!(
                m,
                MessageType::TxPolicyStatus {
                    policy: pancetta_core::TxPolicy::Disabled
                }
            )),
            "{gateway:?}"
        );
        let tui = drain(&ib.tui);
        assert!(tui.iter().any(|m| matches!(
            m,
            MessageType::RigControl(RigControlMessage::SwrResponse { swr }) if *swr == 4.2
        )));
        assert!(tui.iter().any(|m| matches!(
            m,
            MessageType::TxPolicyStatus {
                policy: pancetta_core::TxPolicy::Disabled
            }
        )));
        assert!(tui.iter().any(|m| matches!(
            m,
            MessageType::Error { error_message, .. } if *error_message == trip_alert(4.2, 3.0)
        )));
        assert!(tui.iter().any(|m| matches!(
            m,
            MessageType::DiagnosticEvent {
                target: "rig.swr",
                level: pancetta_core::DiagnosticLevel::Error,
                ..
            }
        )));
    }

    /// PAN-341: an SWR trip relays `TxPolicyStatus { Disabled }` to the
    /// display feed even with NO TUI channel on the bus (headless has no
    /// TUI): the gateway relay does not depend on the best-effort TUI sends.
    #[tokio::test]
    async fn trip_relays_policy_without_tui() {
        let bus = MessageBus::new(16).unwrap();
        let (_, hamlib) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
        let (_, gateway) = bus
            .create_channel(ComponentId::RemoteGateway)
            .await
            .unwrap();
        // Deliberately NO ComponentId::Tui channel: headless.
        let halt = halt_handles();

        act(
            SwrAction::Trip {
                swr: 4.2,
                max_swr: 3.0,
            },
            &bus,
            &halt,
            &feed_on(),
        )
        .await;

        // Safety behavior is unchanged: halt + PTT-off + QSO cancel.
        assert_eq!(policy(&halt), pancetta_core::TxPolicy::Disabled);
        assert!(halt.abort_current_tx.load(Ordering::Acquire));
        assert!(!halt.autonomous_enabled.load(Ordering::Acquire));
        let hamlib = drain(&hamlib);
        assert!(hamlib.iter().any(|m| matches!(
            m,
            MessageType::RigControl(RigControlMessage::SetPtt { state: false, .. })
        )));
        let qso = drain(&qso);
        assert!(qso
            .iter()
            .any(|m| matches!(m, MessageType::QsoMessage(QsoMessage::CancelAllQsos))));
        // The policy announcement reached the display feed.
        let gateway: Vec<_> = drain(&gateway);
        assert!(
            gateway.iter().any(|m| matches!(
                m,
                MessageType::TxPolicyStatus {
                    policy: pancetta_core::TxPolicy::Disabled
                }
            )),
            "{gateway:?}"
        );
    }

    /// PAN-341: only the TRIP announces a policy transition. Reassert-unkey
    /// and availability warnings halt nothing and change no policy, so they
    /// must enqueue no gateway message at all.
    #[tokio::test]
    async fn non_trip_actions_do_not_relay_policy() {
        let ib = inboxes().await;
        let halt = halt_handles();

        act(SwrAction::None, &ib.bus, &halt, &feed_on()).await;
        act(SwrAction::ReassertUnkey, &ib.bus, &halt, &feed_on()).await;
        act(
            SwrAction::ReportUnavailable(SwrUnavailable::NotReported),
            &ib.bus,
            &halt,
            &feed_on(),
        )
        .await;

        assert_eq!(policy(&halt), pancetta_core::TxPolicy::Full);
        assert_eq!(
            drain(&ib.gateway).len(),
            0,
            "no SWR action short of a trip may reach the display feed"
        );
    }

    #[tokio::test]
    async fn step_below_limit_only_displays() {
        let ib = inboxes().await;
        let rig = connected_mock(MockRigConfig {
            swr_override: Some(1.5),
            ..Default::default()
        })
        .await;
        let halt = halt_handles();
        let mut g = guard(false);
        swr_poll_step(&rig, &mut g, true, &ib.bus, &halt, &feed_on()).await;

        let tui = drain(&ib.tui);
        assert_eq!(tui.len(), 1, "{tui:?}");
        assert!(matches!(
            tui[0],
            MessageType::RigControl(RigControlMessage::SwrResponse { swr }) if swr == 1.5
        ));
        assert!(drain(&ib.hamlib).is_empty());
        assert!(drain(&ib.qso).is_empty());
        assert!(!halt.abort_current_tx.load(Ordering::Acquire));
        assert_eq!(policy(&halt), pancetta_core::TxPolicy::Full);
    }

    #[tokio::test]
    async fn step_does_not_display_an_invalid_reading() {
        let ib = inboxes().await;
        let rig = connected_mock(MockRigConfig {
            swr_override: Some(0.0),
            ..Default::default()
        })
        .await;
        let halt = halt_handles();
        let mut g = guard(false);
        swr_poll_step(&rig, &mut g, true, &ib.bus, &halt, &feed_on()).await;
        assert!(drain(&ib.tui).is_empty());
        assert_eq!(policy(&halt), pancetta_core::TxPolicy::Full);
    }

    /// Delegates to `MockRig`, counting `get_swr` calls. Keeps the trait's
    /// default `reports_swr`, which errors.
    struct CountingRig {
        inner: MockRig,
        swr_reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RigControl for CountingRig {
        async fn connect(&self) -> Result<()> {
            self.inner.connect().await
        }
        async fn disconnect(&self) -> Result<()> {
            self.inner.disconnect().await
        }
        async fn get_status(&self) -> Result<RigStatus> {
            self.inner.get_status().await
        }
        async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()> {
            self.inner.set_frequency(vfo, freq).await
        }
        async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
            self.inner.get_frequency(vfo).await
        }
        async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()> {
            self.inner.set_mode(vfo, mode, width).await
        }
        async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
            self.inner.get_mode(vfo).await
        }
        async fn set_vfo(&self, vfo: Vfo) -> Result<()> {
            self.inner.set_vfo(vfo).await
        }
        async fn get_vfo(&self) -> Result<Vfo> {
            self.inner.get_vfo().await
        }
        async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()> {
            self.inner.set_ptt(vfo, ptt).await
        }
        async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
            self.inner.get_ptt(vfo).await
        }
        async fn set_power_level(&self, level: f32) -> Result<()> {
            self.inner.set_power_level(level).await
        }
        async fn get_power_level(&self) -> Result<f32> {
            self.inner.get_power_level().await
        }
        async fn get_s_meter(&self) -> Result<i32> {
            self.inner.get_s_meter().await
        }
        async fn get_swr(&self) -> Result<f32> {
            self.swr_reads.fetch_add(1, Ordering::SeqCst);
            self.inner.get_swr().await
        }
        async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
            self.inner.set_memory_channel(vfo, channel).await
        }
        async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32> {
            self.inner.get_memory_channel(vfo).await
        }
        async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
            self.inner.set_scan(vfo, enable).await
        }
        async fn get_info(&self) -> Result<String> {
            self.inner.get_info().await
        }
    }

    async fn counting_rig() -> CountingRig {
        CountingRig {
            inner: connected_mock(MockRigConfig::default()).await,
            swr_reads: AtomicUsize::new(0),
        }
    }

    #[tokio::test]
    async fn step_unkeyed_does_not_read_the_meter() {
        let ib = inboxes().await;
        let rig = counting_rig().await;
        let halt = halt_handles();
        let mut g = guard(false);
        swr_poll_step(&rig, &mut g, false, &ib.bus, &halt, &feed_on()).await;
        assert_eq!(rig.swr_reads.load(Ordering::SeqCst), 0);
        swr_poll_step(&rig, &mut g, true, &ib.bus, &halt, &feed_on()).await;
        assert_eq!(rig.swr_reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn step_rig_without_meter_reports_unavailable_after_a_transmission() {
        let ib = inboxes().await;
        let rig = connected_mock(MockRigConfig {
            simulate_swr: false,
            ..Default::default()
        })
        .await;
        let halt = halt_handles();
        let mut g = guard(true);
        for _ in 0..SWR_UNAVAILABLE_MIN_ATTEMPTS {
            swr_poll_step(&rig, &mut g, true, &ib.bus, &halt, &feed_on()).await;
        }
        swr_poll_step(&rig, &mut g, false, &ib.bus, &halt, &feed_on()).await;

        let tui = drain(&ib.tui);
        let errors: Vec<_> = tui
            .iter()
            .filter_map(|m| match m {
                MessageType::Error { error_message, .. } => Some(error_message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            errors,
            vec![unavailable_alert(SwrUnavailable::NoReadingWhileKeyed)]
        );
        let diagnostics = tui
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    MessageType::DiagnosticEvent {
                        target: "rig.swr",
                        level: pancetta_core::DiagnosticLevel::Warn,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(diagnostics, 1);
        assert!(drain(&ib.hamlib).is_empty());
        assert!(drain(&ib.qso).is_empty());
        assert_eq!(policy(&halt), pancetta_core::TxPolicy::Full);
        assert!(!halt.abort_current_tx.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn probe_classifies_capabilities() {
        let with_meter = connected_mock(MockRigConfig::default()).await;
        assert_eq!(
            probe_swr_capability(&with_meter).await,
            SwrCapability::Supported
        );
        let without_meter = connected_mock(MockRigConfig {
            simulate_swr: false,
            ..Default::default()
        })
        .await;
        assert_eq!(
            probe_swr_capability(&without_meter).await,
            SwrCapability::Unsupported
        );
        assert_eq!(
            probe_swr_capability(&counting_rig().await).await,
            SwrCapability::Unknown
        );
    }

    #[tokio::test]
    async fn keyed_includes_manual_ptt() {
        assert!(poll_sees_keyed(&AtomicBool::new(false), &RwLock::new(Some(Instant::now()))).await);
        assert!(poll_sees_keyed(&AtomicBool::new(true), &RwLock::new(None)).await);
        assert!(!poll_sees_keyed(&AtomicBool::new(false), &RwLock::new(None)).await);
    }
}
