//! PAN-89: the coordinator side of the TUI readiness strip.
//!
//! [`ReadinessSources`] holds clones of the std-sync shared state the strip
//! reads (no tokio locks, so the TUI relay's OS thread can read it), and
//! [`ReadinessSources::snapshot`] turns it into the TUI's
//! [`StationReadiness`]. [`ReadinessPublisher`] decides when the relay's 2 s
//! tick should push a snapshot (first call and every change) and spots the
//! falling edge of `ptt_off_unconfirmed`. The relay itself only wires these
//! together (`tui_relay.rs`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use pancetta_tui::readiness::{ClockState, StationReadiness, TxMuteCause as TuiTxMuteCause};

use super::tx::{self, TxMuteCause};
use crate::clock_skew::{ClockSkewShared, ClockStatus};
use crate::message_bus::ComponentMessage;
use tracing::debug;

/// Clones of every coordinator input to a [`StationReadiness`] snapshot.
pub(crate) struct ReadinessSources {
    tx_policy: Arc<AtomicU8>,
    tx_restart_inhibit: Arc<AtomicU32>,
    hamlib_command_loop_ready: Arc<AtomicBool>,
    hamlib_pending_frequency: Arc<Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>>,
    hamlib_pending_split: Arc<Mutex<Option<ComponentMessage>>>,
    hamlib_command_in_flight: Arc<AtomicU32>,
    rig_conn_state: Arc<AtomicU8>,
    audio_output_alive: Arc<AtomicBool>,
    ptt_off_unconfirmed: Arc<AtomicBool>,
    clock_skew: ClockSkewShared,
    degraded_summary: Arc<Mutex<Option<String>>>,
    /// PAN-104: the shared audit log, read only through its health lock.
    audit: pancetta_agent::audit::AuditLog,
    /// The cause list of the previous snapshot, so a changed TX verdict is
    /// logged once (in the gate's own words) rather than every tick.
    last_tx_mute_causes: RefCell<Option<Vec<TxMuteCause>>>,
}

impl ReadinessSources {
    pub(crate) fn from_coordinator(c: &super::ApplicationCoordinator) -> Self {
        Self {
            tx_policy: c.tx_policy.clone(),
            tx_restart_inhibit: c.tx_restart_inhibit.clone(),
            hamlib_command_loop_ready: c.hamlib_command_loop_ready.clone(),
            hamlib_pending_frequency: c.hamlib_pending_frequency.clone(),
            hamlib_pending_split: c.hamlib_pending_split.clone(),
            hamlib_command_in_flight: c.hamlib_command_in_flight.clone(),
            rig_conn_state: c.rig_conn_state.clone(),
            audio_output_alive: c.audio_output_alive.clone(),
            ptt_off_unconfirmed: c.ptt_off_unconfirmed.clone(),
            clock_skew: c.clock_skew.clone(),
            degraded_summary: c.degraded_summary.clone(),
            audit: c.audit_log(),
            last_tx_mute_causes: RefCell::new(None),
        }
    }

    /// The current readiness. `rf_no_decode` / `input_silent` are the relay's
    /// own `RfNoDecodeMonitor` edge state.
    pub(crate) fn snapshot(&self, rf_no_decode: bool, input_silent: bool) -> StationReadiness {
        let causes = tx::tx_mute_causes(
            &self.tx_policy,
            &self.tx_restart_inhibit,
            &self.hamlib_command_loop_ready,
            &self.hamlib_pending_frequency,
            &self.hamlib_pending_split,
            &self.hamlib_command_in_flight,
            &self.rig_conn_state,
            &self.audio_output_alive,
        );
        let mut last = self.last_tx_mute_causes.borrow_mut();
        if last.as_ref() != Some(&causes) {
            debug!("Station readiness: {}", tx_verdict(&causes));
            *last = Some(causes.clone());
        }
        let tx_mute_causes = causes.into_iter().map(tui_tx_mute_cause).collect();
        let supervisor = self
            .degraded_summary
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let degraded = compose_degraded(supervisor, self.audit.health().unavailable);
        StationReadiness {
            tx_mute_causes,
            ptt_off_unconfirmed: self.ptt_off_unconfirmed.load(Ordering::Acquire),
            clock: tui_clock_state(self.clock_skew.status()),
            degraded,
            rf_no_decode,
            input_silent,
        }
    }
}

/// PAN-104: the strip's degraded text — the supervisor's summary, with the
/// audit-health phrase appended (`; `) while an audit failure episode is
/// open. Composed per snapshot so neither source overwrites the other, and
/// audit health is never a TX mute cause.
fn compose_degraded(supervisor: Option<String>, audit_unavailable: bool) -> Option<String> {
    use pancetta_agent::audit::UNAVAILABLE_MESSAGE;
    match (supervisor, audit_unavailable) {
        (summary, false) => summary,
        (None, true) => Some(UNAVAILABLE_MESSAGE.to_string()),
        (Some(summary), true) => Some(format!("{summary}; {UNAVAILABLE_MESSAGE}")),
    }
}

/// The TX verdict in the gate's own words, for the log.
fn tx_verdict(causes: &[TxMuteCause]) -> String {
    if causes.is_empty() {
        "TX open".to_string()
    } else {
        let reasons: Vec<&str> = causes.iter().map(|c| c.as_reason()).collect();
        format!("TX held: {}", reasons.join("; "))
    }
}

/// The TUI's mirror of a coordinator TX-mute cause.
fn tui_tx_mute_cause(cause: TxMuteCause) -> TuiTxMuteCause {
    match cause {
        TxMuteCause::RigRestarting => TuiTxMuteCause::RigRestarting,
        TxMuteCause::HamlibLoopNotReady => TuiTxMuteCause::RigNotReady,
        TxMuteCause::RigDisconnected => TuiTxMuteCause::RigDisconnected,
        TxMuteCause::AudioOutputDead => TuiTxMuteCause::AudioOutputDead,
        TxMuteCause::RigStatePending => TuiTxMuteCause::RigStatePending,
        TxMuteCause::RigCommandInFlight => TuiTxMuteCause::RigCommandInFlight,
        TxMuteCause::PolicyDisabled => TuiTxMuteCause::PolicyDisabled,
    }
}

/// The TUI's mirror of the clock-skew monitor's status.
fn tui_clock_state(status: ClockStatus) -> ClockState {
    match status {
        ClockStatus::NotMonitored => ClockState::NotMonitored,
        ClockStatus::Pending => ClockState::Pending,
        ClockStatus::InSpec => ClockState::InSpec,
        ClockStatus::Skewed => ClockState::Skewed,
        ClockStatus::Unreachable => ClockState::Unreachable,
        ClockStatus::Rechecking => ClockState::Rechecking,
    }
}

/// Push-on-change bookkeeping for the relay's readiness snapshots.
#[derive(Debug, Default)]
pub(crate) struct ReadinessPublisher {
    last: Option<StationReadiness>,
}

impl ReadinessPublisher {
    /// `(send, ptt_cleared)`: `send` on the first call or any change;
    /// `ptt_cleared` when `ptt_off_unconfirmed` went `true → false` since
    /// the previous call.
    pub(crate) fn observe(&mut self, next: &StationReadiness) -> (bool, bool) {
        let send = self.last.as_ref() != Some(next);
        let ptt_cleared = self
            .last
            .as_ref()
            .is_some_and(|last| last.ptt_off_unconfirmed && !next.ptt_off_unconfirmed);
        if send {
            self.last = Some(next.clone());
        }
        (send, ptt_cleared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::hamlib::RigConnState;

    fn sources() -> ReadinessSources {
        ReadinessSources {
            tx_policy: Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8())),
            tx_restart_inhibit: Arc::new(AtomicU32::new(0)),
            hamlib_command_loop_ready: Arc::new(AtomicBool::new(true)),
            hamlib_pending_frequency: Arc::new(Mutex::new(HashMap::new())),
            hamlib_pending_split: Arc::new(Mutex::new(None)),
            hamlib_command_in_flight: Arc::new(AtomicU32::new(0)),
            rig_conn_state: Arc::new(AtomicU8::new(RigConnState::Connected.as_u8())),
            audio_output_alive: Arc::new(AtomicBool::new(true)),
            ptt_off_unconfirmed: Arc::new(AtomicBool::new(false)),
            clock_skew: ClockSkewShared::new(),
            degraded_summary: Arc::new(Mutex::new(None)),
            // Never written by these tests, so it stays healthy.
            audit: pancetta_agent::audit::AuditLog::new(
                std::env::temp_dir().join("pancetta-readiness-unused-audit.log"),
            ),
            last_tx_mute_causes: RefCell::new(None),
        }
    }

    /// An audit log whose writes fail: its parent directory does not exist.
    fn failing_audit() -> pancetta_agent::audit::AuditLog {
        let log = pancetta_agent::audit::AuditLog::new(
            std::env::temp_dir()
                .join(format!("pancetta-readiness-missing-{}", std::process::id()))
                .join("nope")
                .join("audit.log"),
        );
        assert!(log
            .append(&pancetta_agent::audit::AuditEvent {
                ts_unix_ms: 1,
                kind: pancetta_agent::audit::AuditKind::Armed,
                operator_callsign: None,
                detail: "x".into(),
            })
            .is_err());
        log
    }

    /// PAN-104: an audit failure shows as a degraded warning composed with
    /// (never replacing) the supervisor summary, and never mutes TX.
    #[test]
    fn audit_failure_composes_with_the_supervisor_summary() {
        let mut s = sources();
        s.audit = failing_audit();
        let snap = s.snapshot(false, false);
        assert_eq!(
            snap.degraded.as_deref(),
            Some("Remote audit unavailable; events may be missing")
        );
        assert!(
            snap.tx_mute_causes.is_empty(),
            "audit health is not a TX mute cause"
        );

        *s.degraded_summary.lock().unwrap() = Some("dx_cluster: telnet closed".into());
        assert_eq!(
            s.snapshot(false, false).degraded.as_deref(),
            Some("dx_cluster: telnet closed; Remote audit unavailable; events may be missing")
        );

        // Supervisor recovery alone leaves the audit phrase.
        *s.degraded_summary.lock().unwrap() = None;
        assert_eq!(
            s.snapshot(false, false).degraded.as_deref(),
            Some("Remote audit unavailable; events may be missing")
        );
    }

    #[test]
    fn compose_degraded_covers_each_recovery_order() {
        let sup = || Some("hamlib: gone".to_string());
        assert_eq!(compose_degraded(None, false), None);
        assert_eq!(compose_degraded(sup(), false), sup());
        assert_eq!(
            compose_degraded(None, true).as_deref(),
            Some(pancetta_agent::audit::UNAVAILABLE_MESSAGE)
        );
        assert_eq!(
            compose_degraded(sup(), true).as_deref(),
            Some("hamlib: gone; Remote audit unavailable; events may be missing")
        );
        // A recurring audit episode reads the same as the first.
        assert_eq!(compose_degraded(None, true), compose_degraded(None, true));
    }

    #[test]
    fn snapshot_maps_every_source() {
        let s = sources();
        assert_eq!(s.snapshot(false, false), StationReadiness::default());

        s.tx_policy
            .store(pancetta_core::TxPolicy::Disabled.as_u8(), Ordering::Release);
        s.ptt_off_unconfirmed.store(true, Ordering::Release);
        s.clock_skew.set_status(ClockStatus::Unreachable);
        *s.degraded_summary.lock().unwrap() = Some("dx_cluster: telnet closed".into());

        assert_eq!(
            s.snapshot(true, true),
            StationReadiness {
                tx_mute_causes: vec![TuiTxMuteCause::PolicyDisabled],
                ptt_off_unconfirmed: true,
                clock: ClockState::Unreachable,
                degraded: Some("dx_cluster: telnet closed".into()),
                rf_no_decode: true,
                input_silent: true,
            }
        );

        // The two relay-owned flags are independent.
        let snap = s.snapshot(true, false);
        assert!(snap.rf_no_decode && !snap.input_silent);
        let snap = s.snapshot(false, true);
        assert!(!snap.rf_no_decode && snap.input_silent);

        // A poisoned summary lock still reads (the relay must not die).
        let poison = s.degraded_summary.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("poison the degraded-summary lock");
        })
        .join();
        assert_eq!(
            s.snapshot(false, false).degraded.as_deref(),
            Some("dx_cluster: telnet closed")
        );
    }

    #[test]
    fn snapshot_converts_causes_in_gate_order() {
        let s = sources();
        s.tx_policy
            .store(pancetta_core::TxPolicy::Disabled.as_u8(), Ordering::Release);
        s.tx_restart_inhibit.store(1, Ordering::Release);
        s.hamlib_command_loop_ready.store(false, Ordering::Release);
        s.rig_conn_state
            .store(RigConnState::PollingFailed.as_u8(), Ordering::Release);
        s.audio_output_alive.store(false, Ordering::Release);
        *s.hamlib_pending_split.lock().unwrap() = Some(ComponentMessage::new(
            crate::message_bus::ComponentId::Hamlib,
            crate::message_bus::ComponentId::Hamlib,
            crate::message_bus::MessageType::RigControl(
                crate::message_bus::RigControlMessage::SetSplit {
                    enabled: true,
                    tx_frequency: 14_078_000,
                },
            ),
            std::time::Instant::now(),
        ));
        s.hamlib_command_in_flight.store(1, Ordering::Release);

        assert_eq!(
            s.snapshot(false, false).tx_mute_causes,
            TuiTxMuteCause::ALL.to_vec()
        );

        // A subset keeps the gate's relative order.
        let s = sources();
        s.tx_policy
            .store(pancetta_core::TxPolicy::Disabled.as_u8(), Ordering::Release);
        s.hamlib_command_loop_ready.store(false, Ordering::Release);
        s.audio_output_alive.store(false, Ordering::Release);
        assert_eq!(
            s.snapshot(false, false).tx_mute_causes,
            vec![
                TuiTxMuteCause::RigNotReady,
                TuiTxMuteCause::AudioOutputDead,
                TuiTxMuteCause::PolicyDisabled,
            ]
        );
    }

    #[test]
    fn tx_verdict_uses_the_gate_strings() {
        assert_eq!(tx_verdict(&[]), "TX open");
        assert_eq!(
            tx_verdict(&[TxMuteCause::RigDisconnected, TxMuteCause::PolicyDisabled]),
            "TX held: rig is disconnected; TX policy is Disabled"
        );
    }

    #[test]
    fn clock_status_maps_to_tui_clock_state() {
        for (status, state) in [
            (ClockStatus::NotMonitored, ClockState::NotMonitored),
            (ClockStatus::Pending, ClockState::Pending),
            (ClockStatus::InSpec, ClockState::InSpec),
            (ClockStatus::Skewed, ClockState::Skewed),
            (ClockStatus::Unreachable, ClockState::Unreachable),
            (ClockStatus::Rechecking, ClockState::Rechecking),
        ] {
            assert_eq!(tui_clock_state(status), state, "{status:?}");
        }
    }

    #[test]
    fn publisher_sends_first_snapshot_then_only_on_change() {
        let mut publisher = ReadinessPublisher::default();
        let quiet = StationReadiness::default();
        assert_eq!(publisher.observe(&quiet), (true, false), "first snapshot");
        assert_eq!(publisher.observe(&quiet), (false, false), "unchanged");

        let skewed = StationReadiness {
            clock: ClockState::Skewed,
            ..StationReadiness::default()
        };
        assert_eq!(publisher.observe(&skewed), (true, false), "changed");
        assert_eq!(publisher.observe(&skewed), (false, false), "unchanged");
        assert_eq!(publisher.observe(&quiet), (true, false), "changed back");
    }

    #[test]
    fn publisher_reports_ptt_cleared_on_the_falling_edge_only() {
        let mut publisher = ReadinessPublisher::default();
        let latched = StationReadiness {
            ptt_off_unconfirmed: true,
            ..StationReadiness::default()
        };
        let clear = StationReadiness::default();

        // A latched first snapshot is a rising edge, not a clear.
        assert_eq!(publisher.observe(&latched), (true, false));
        assert_eq!(publisher.observe(&latched), (false, false));
        // The falling edge reports once.
        assert_eq!(publisher.observe(&clear), (true, true));
        assert_eq!(publisher.observe(&clear), (false, false));

        // A clear first snapshot is not a falling edge either.
        let mut publisher = ReadinessPublisher::default();
        assert_eq!(publisher.observe(&clear), (true, false));

        // Another field changing while still latched isn't a clear.
        let mut publisher = ReadinessPublisher::default();
        publisher.observe(&latched);
        let latched_and_skewed = StationReadiness {
            clock: ClockState::Skewed,
            ..latched.clone()
        };
        assert_eq!(publisher.observe(&latched_and_skewed), (true, false));
        assert_eq!(
            publisher.observe(&StationReadiness {
                clock: ClockState::Skewed,
                ..StationReadiness::default()
            }),
            (true, true)
        );
    }
}
