//! Coordinator-level robustness regression tests — C9, C19, C20.
//!
//! These exercise three operator-grounded failure modes from the QSO scenario
//! catalog (`docs/qso-scenario-catalog-2026-06-16.md`):
//!
//! - **C9** — band change mid-QSO: an active QSO can't complete on a new band,
//!   and its keep-call must NOT keep transmitting there. The operator dial
//!   move triggers a teardown of every active QSO; the cancelled QSOs leave the
//!   shared `active_tx_qsos` set, so the drop-stale-TX gate drops their queued
//!   TX. A tiny dial wobble must NOT tear anything down.
//! - **C19** — config hot-reload mid-QSO must not clobber the latched partner
//!   callsign / `tx_parity`. The real classifier decides which config sections
//!   are safe to apply live vs must be deferred while a QSO is active.
//! - **C20** — RF present but zero decodes over several slots → likely a wrong
//!   mode (FT8/FT4) or a bad clock. The real detector raises a warning after
//!   several RF-present/no-decode slots, and stays quiet on a genuinely quiet
//!   band.
//!
//! Each test exercises the **real production decision logic** re-exported from
//! the coordinator (`is_band_change`, `classify_config_reload`,
//! `RfNoDecodeMonitor`), and the C9 integration test additionally drives the
//! real `QsoManager` + the real `active_tx_qsos` / `tx_qso_is_live` gate so the
//! end-to-end "cancelled QSO → its TX is dropped" contract is asserted, not
//! just the trigger predicate.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use pancetta_config::Config;
use pancetta_core::slot::SlotParity;
use pancetta_lib::coordinator::{
    active_tx_qso_key, classify_config_reload, is_band_change, tx_qso_is_live,
    ConfigReloadApplicability, RfNoDecodeMonitor,
};
use pancetta_qso::{CallInitiation, QsoEvent, QsoManager, QsoManagerConfig};

// FT8 audio-band dial frequencies (Hz).
const FREQ_20M: u64 = 14_074_000;
const FREQ_40M: u64 = 7_074_000;

// ---------------------------------------------------------------------------
// C9 — band change mid-QSO: graceful teardown, no stale keep-call TX
// ---------------------------------------------------------------------------

/// Mirror of the coordinator's `active_tx_qsos` populater
/// (`coordinator/qso.rs`): insert a qso_id on a `StateChanged` into any active
/// state; remove it on a `StateChanged → Failed` or a `QsoFailed`. This is the
/// exact rule a cancelled (band-change-torn-down) QSO trips to leave the set.
fn drain_into_active_set(
    rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>,
    active: &Arc<RwLock<HashSet<String>>>,
) {
    while let Ok(ev) = rx.try_recv() {
        match ev {
            QsoEvent::StateChanged {
                qso_id, new_state, ..
            } => {
                let key = active_tx_qso_key(&qso_id.to_string());
                if new_state.is_active() {
                    active.write().unwrap().insert(key);
                } else if matches!(new_state, pancetta_qso::QsoState::Failed { .. }) {
                    active.write().unwrap().remove(&key);
                }
            }
            QsoEvent::QsoFailed { qso_id, .. } => {
                let key = active_tx_qso_key(&qso_id.to_string());
                active.write().unwrap().remove(&key);
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn c9_band_change_tears_down_active_qsos_and_drops_their_tx() {
    let manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    let mut rx = manager.subscribe();
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    // We are on 20m and answer a CQ — an active QSO with a queued keep-call TX.
    let qso_id = manager
        .respond_to_cq_with(
            "DL1ABC".to_string(),
            1200.0,
            Some(SlotParity::Odd),
            CallInitiation::Auto,
        )
        .await
        .expect("respond_to_cq_with");
    let qso_key = qso_id.to_string();

    drain_into_active_set(&mut rx, &active);

    // The QSO is live → its TX would be allowed to key.
    assert!(
        tx_qso_is_live(Some(&qso_key), &active.read().unwrap()),
        "QSO should be live before the band change"
    );

    // Operator changes 20m → 40m. The coordinator's SetFrequency handler asks
    // `is_band_change` whether this is a real band change.
    assert!(
        is_band_change(FREQ_20M, FREQ_40M),
        "20m -> 40m must register as a band change"
    );

    // The band-change handler tears down every active QSO (the same loop the
    // production `QsoMessage::BandChanged` arm runs).
    let to_cancel = manager.get_active_qsos().await;
    assert_eq!(to_cancel.len(), 1, "exactly one active QSO to tear down");
    for (id, _) in to_cancel {
        manager.cancel_qso(id).await.expect("cancel_qso");
    }

    // The cancellation drove the QSO to Failed → the populater removes it from
    // the active set.
    drain_into_active_set(&mut rx, &active);

    // Contract: the now-cancelled QSO is no longer live, so the drop-stale-TX
    // gate drops any keep-call TX still queued for it — no stale TX on 40m.
    assert!(
        !tx_qso_is_live(Some(&qso_key), &active.read().unwrap()),
        "torn-down QSO must NOT be live after the band change (its keep-call TX is dropped)"
    );
    assert!(
        active.read().unwrap().is_empty(),
        "active_tx_qsos must be empty after band-change teardown"
    );
}

#[tokio::test]
async fn c9_small_dial_wobble_does_not_tear_down() {
    let manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    let mut rx = manager.subscribe();
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    let qso_id = manager
        .respond_to_cq_with(
            "JA1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
        )
        .await
        .expect("respond_to_cq_with");
    drain_into_active_set(&mut rx, &active);

    // A 1 kHz fine-tune within the 20m FT8 sub-band is NOT a band change.
    assert!(
        !is_band_change(FREQ_20M, FREQ_20M + 1_000),
        "a 1 kHz nudge inside the same band must not register as a band change"
    );

    // So the handler would NOT tear down — the QSO stays live.
    assert!(
        tx_qso_is_live(Some(&qso_id.to_string()), &active.read().unwrap()),
        "QSO must remain live across a tiny dial wobble"
    );
}

#[test]
fn c9_is_band_change_predicate_matrix() {
    // Startup / uninitialized: never fire (nothing to tear down).
    assert!(!is_band_change(0, FREQ_20M));
    // No move: not a change.
    assert!(!is_band_change(FREQ_20M, FREQ_20M));
    // Same band, small move: not a change.
    assert!(!is_band_change(FREQ_20M, FREQ_20M + 2_000));
    // Different ham bands: a change.
    assert!(is_band_change(FREQ_20M, FREQ_40M));
    assert!(is_band_change(FREQ_40M, 21_074_000)); // 40m -> 15m
                                                   // Out-of-band small wobble (both outside any ham band, < threshold): not.
    assert!(!is_band_change(5_000_000, 5_010_000));
    // Out-of-band large jump (>= 100 kHz threshold): a change.
    assert!(is_band_change(5_000_000, 5_200_000));
}

// ---------------------------------------------------------------------------
// C19 — config hot-reload must not clobber latched partner / parity
// ---------------------------------------------------------------------------

#[test]
fn c19_callsign_change_mid_qso_is_deferred() {
    let old = Config::default();
    let mut new = Config::default();
    new.station.callsign = "W1AW".to_string(); // changed our own call mid-QSO

    // QSO active → the latched-identity change MUST be deferred (never applied
    // to the running QSO, so the partner/sender-verification can't be clobbered).
    assert_eq!(
        classify_config_reload(&old, &new, /* qso_active */ true),
        ConfigReloadApplicability::DeferQsoLatched
    );
}

#[test]
fn c19_grid_change_mid_qso_is_deferred() {
    let old = Config::default();
    let mut new = Config::default();
    new.station.grid_square = "FN31".to_string();
    assert_eq!(
        classify_config_reload(&old, &new, true),
        ConfigReloadApplicability::DeferQsoLatched
    );
}

#[test]
fn c19_parity_change_mid_qso_is_deferred() {
    let old = Config::default();
    let mut new = Config::default();
    new.autonomous.slot_parity = pancetta_config::SlotParitySetting::Odd;
    // Changing the configured slot parity mid-QSO must not clobber a QSO's
    // already-latched tx_parity.
    assert_eq!(
        classify_config_reload(&old, &new, true),
        ConfigReloadApplicability::DeferQsoLatched
    );
}

#[test]
fn c19_latched_change_when_quiescent_is_safe() {
    let old = Config::default();
    let mut new = Config::default();
    new.station.callsign = "W1AW".to_string();
    // No QSO active → nothing to clobber; safe to pick up.
    assert_eq!(
        classify_config_reload(&old, &new, false),
        ConfigReloadApplicability::SafeQuiescent
    );
}

#[test]
fn c19_live_safe_section_change_applies_even_mid_qso() {
    // Clone from a single base (so the ONLY difference is the network toggle,
    // not the per-default metadata timestamp).
    let mut old = Config::default();
    old.metadata = None;
    let mut new = old.clone();
    // A non-latched, live-safe change (a network toggle) is safe to apply even
    // while a QSO is active — it can never touch latched QSO state.
    new.network.psk_reporter.enabled = !old.network.psk_reporter.enabled;
    assert_eq!(
        classify_config_reload(&old, &new, true),
        ConfigReloadApplicability::SafeLive
    );
}

#[test]
fn c19_no_change_is_noop() {
    // `Config::default()` stamps `metadata.last_modified = Utc::now()`, so two
    // fresh defaults are NOT byte-identical. Clone one and clear the metadata so
    // the two configs are genuinely identical — a true no-op reload.
    let mut old = Config::default();
    old.metadata = None;
    let new = old.clone();
    assert_eq!(
        classify_config_reload(&old, &new, true),
        ConfigReloadApplicability::NoChange
    );
}

// ---------------------------------------------------------------------------
// C20 — RF present but zero decodes (mode / clock fault)
// ---------------------------------------------------------------------------

const RF: f32 = RfNoDecodeMonitor::RF_PRESENT_RMS_FLOOR + 0.05; // clearly RF present
const QUIET: f32 = RfNoDecodeMonitor::RF_PRESENT_RMS_FLOOR / 4.0; // genuinely quiet band

#[test]
fn c20_rf_present_no_decodes_raises_warning() {
    let mut m = RfNoDecodeMonitor::new();
    // First observation seeds the baseline (no edge).
    assert_eq!(m.observe(0, 0, RF), None);

    let mut warned = false;
    // Each tick advances one DSP window, RF present, decodes flat at 0.
    for slot in 1..=RfNoDecodeMonitor::WARN_AFTER_SLOTS {
        let edge = m.observe(slot as u64, 0, RF);
        if slot < RfNoDecodeMonitor::WARN_AFTER_SLOTS {
            assert_eq!(edge, None, "must not warn before the slot threshold");
        } else {
            assert_eq!(edge, Some(true), "must warn at the slot threshold");
            warned = true;
        }
    }
    assert!(warned);
    assert!(m.warning_active());
}

#[test]
fn c20_quiet_band_never_warns() {
    let mut m = RfNoDecodeMonitor::new();
    assert_eq!(m.observe(0, 0, QUIET), None);
    // Many slots of a quiet band (RMS below the floor) and zero decodes: this
    // is normal, must never warn.
    for slot in 1..=(RfNoDecodeMonitor::WARN_AFTER_SLOTS * 3) {
        assert_eq!(
            m.observe(slot as u64, 0, QUIET),
            None,
            "a quiet band must never raise the RF/no-decode warning"
        );
    }
    assert!(!m.warning_active());
    assert_eq!(m.consecutive(), 0);
}

#[test]
fn c20_a_decode_resets_the_streak() {
    let mut m = RfNoDecodeMonitor::new();
    assert_eq!(m.observe(0, 0, RF), None);
    // A couple of RF-present/no-decode slots build the streak...
    assert_eq!(m.observe(1, 0, RF), None);
    assert_eq!(m.observe(2, 0, RF), None);
    assert!(m.consecutive() >= 2);
    // ...then we decode something: the streak resets, no warning.
    assert_eq!(m.observe(3, 5, RF), None);
    assert_eq!(m.consecutive(), 0);
    assert!(!m.warning_active());
}

#[test]
fn c20_warning_clears_when_decodes_resume() {
    let mut m = RfNoDecodeMonitor::new();
    assert_eq!(m.observe(0, 0, RF), None);
    // Drive into the warning state.
    let mut decodes = 0u64;
    for slot in 1..=RfNoDecodeMonitor::WARN_AFTER_SLOTS {
        m.observe(slot as u64, decodes, RF);
    }
    assert!(m.warning_active());

    // Decodes resume → warning clears on the falling edge.
    decodes += 3;
    let edge = m.observe(
        (RfNoDecodeMonitor::WARN_AFTER_SLOTS + 1) as u64,
        decodes,
        RF,
    );
    assert_eq!(edge, Some(false), "warning must clear when decodes resume");
    assert!(!m.warning_active());
}

#[test]
fn c20_no_new_window_is_ignored() {
    let mut m = RfNoDecodeMonitor::new();
    assert_eq!(m.observe(5, 0, RF), None); // seed
                                           // Same window count (no new slot ran): no judgement, streak unchanged.
    assert_eq!(m.observe(5, 0, RF), None);
    assert_eq!(m.consecutive(), 0);
}
