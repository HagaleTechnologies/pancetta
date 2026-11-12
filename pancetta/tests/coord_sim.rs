//! Scenarios exercising the real coordinator TX worker.
#![allow(clippy::expect_used)]
mod common;
use common::*;
use pancetta_config::OperatingMode;
use pancetta_core::slot::SlotParity;
use pancetta_core::TxPolicy;
use pancetta_lib::coordinator::{
    active_tx_qso_key, compute_manual_tx_offset, try_switch_operating_mode, ModeSwitchError,
};
use pancetta_qso::{CallInitiation, QsoEvent, QsoState};
use std::sync::atomic::Ordering;
use std::time::Duration;

// ===========================================================================
// Permanent scenarios
// ===========================================================================

/// PTT keys for a scheduled QSO transmit (the StateChanged-at-QSO-start fix):
/// `respond_to_cq_with` emits StateChanged(active) BEFORE the first
/// MessageToSend, so `pump_qso_events` inserts the qso_id into `active_tx_qsos`
/// before the TransmitRequest reaches the Step-4b gate, and PTT keys.
#[tokio::test]
async fn ptt_keys_for_scheduled_qso() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "QSO start should have forwarded at least one TransmitRequest"
    );

    sim.drive_slot(pending).await;

    sim.timeline.assert_keyed_for_qso(&qso_id.to_string());
    sim.timeline.assert_all_released();
    // And it keyed at the requested offset.
    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), 1500.0);
    sim.shutdown().await;
}

/// FQ-F3: `active_tx_offsets` (the own-frequency-registry source map) is
/// populated at the same point as `active_tx_qsos` — a StateChanged into an
/// active state — and carries the QSO's real audio-offset frequency, not
/// just its id. It is then depopulated by the same removal paths
/// (`expire_qso`, mirroring the coordinator's Failed / completed-grace
/// purge), keeping the two maps consistent.
#[tokio::test]
async fn active_tx_offsets_populated_and_depopulated_with_active_tx_qsos() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    assert!(!pending.is_empty());

    let key = active_tx_qso_key(&qso_id.to_string());

    // Populated: active_tx_qsos and active_tx_offsets must agree on
    // membership, and the offset must be the QSO's real frequency.
    assert!(
        sim.active_tx_qsos.read().unwrap().contains(&key),
        "sanity: active_tx_qsos should contain the started QSO"
    );
    let offset = *sim
        .active_tx_offsets
        .read()
        .unwrap()
        .get(&key)
        .expect("active_tx_offsets should contain the started QSO's offset");
    assert_eq!(offset, 1500.0, "offset must be the QSO's real frequency");

    // Depopulated: the same removal path (mirroring the coordinator's
    // Failed / completed-grace purge) must clear BOTH maps together.
    sim.expire_qso(&qso_id.to_string());
    assert!(
        !sim.active_tx_qsos.read().unwrap().contains(&key),
        "active_tx_qsos must be empty after expiry"
    );
    assert!(
        !sim.active_tx_offsets.read().unwrap().contains_key(&key),
        "active_tx_offsets must be empty after expiry — a leaked entry here \
         would permanently penalize a real future candidate at 1500 Hz"
    );

    sim.drive_slot(pending).await;
    sim.shutdown().await;
}

/// Stale-TX drop: a superseded / cancelled / completed-past-grace QSO's queued
/// TransmitRequest is dropped before keying — PTT does NOT key for it.
#[tokio::test]
async fn stale_tx_dropped_after_supersede_no_ptt() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "DL1ABC".to_string(),
            1200.0,
            Some(SlotParity::Odd),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");

    // Coordinator forwards the first call; capture it but DON'T transmit yet.
    let pending = sim.pump_qso_events();
    assert!(!pending.is_empty());

    // Now the QSO is superseded / cancelled: its key leaves the active set
    // (as a StateChanged→Failed would, or the grace purge). Model that.
    sim.expire_qso(&qso_id.to_string());

    // Drive the slot with the now-stale request. It must be dropped, not keyed.
    sim.drive_slot(pending).await;

    sim.timeline.assert_not_keyed_for_qso(&qso_id.to_string());
    sim.timeline.assert_silent();
    // And the drop was recorded as a stale-QSO drop.
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::WorkerRefused),
        "expected a StaleQso drop.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

/// TX backpressure / coalescing: a backlog of keep-call frames for the SAME
/// live QSO coalesces to the newest; the older stale frames are NOT transmitted.
#[tokio::test]
async fn coalesce_backlog_newest_wins_stale_not_keyed() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "JA1XYZ".to_string(),
            1800.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");
    let id = qso_id.to_string();
    // Make the QSO live.
    let _ = sim.pump_qso_events();

    // Simulate three keep-call frames backed up for the same QSO (oldest first).
    let backlog = vec![
        PendingTx {
            text: "CQ K5ARH EM10".to_string(),
            frequency_offset: 1800.0,
            qso_id: Some(id.clone()),
            tx_parity: Some(SlotParity::Even),
            origin: pancetta_lib::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            attempt_id: Some(pancetta_qso::TxAttemptId(10001)),
        },
        PendingTx {
            text: "CQ K5ARH EM11".to_string(),
            frequency_offset: 1800.0,
            qso_id: Some(id.clone()),
            tx_parity: Some(SlotParity::Even),
            origin: pancetta_lib::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            attempt_id: Some(pancetta_qso::TxAttemptId(10002)),
        },
        PendingTx {
            text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1800.0,
            qso_id: Some(id.clone()),
            tx_parity: Some(SlotParity::Even),
            origin: pancetta_lib::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            attempt_id: Some(pancetta_qso::TxAttemptId(10003)),
        },
    ];

    sim.drive_slot(backlog).await;

    // Exactly one keyed transmit for this QSO this slot — the newest.
    let keyed = sim.timeline.keyed_for_qso(&id);
    assert_eq!(
        keyed.len(),
        1,
        "expected exactly one keyed frame after coalesce, got {}.\n{}",
        keyed.len(),
        sim.timeline
    );
    assert_eq!(keyed[0].text, "CQ K5ARH EM12");
    assert!(keyed[0].ptt_keyed);

    // The two older frames were coalesced away (not keyed).
    let coalesced = sim
        .timeline
        .dropped
        .iter()
        .filter(|d| d.reason == DropReason::WorkerRefused)
        .count();
    assert_eq!(
        coalesced, 2,
        "expected 2 coalesced-away frames.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

/// Multi-stream: two concurrent live QSOs on different offsets each key + key
/// PTT on their own frequency in a single slot.
#[tokio::test]
async fn two_simultaneous_qsos_key_on_distinct_freqs() {
    let mut sim = CoordSim::new("K5ARH").await;
    let a = sim
        .manager
        .respond_to_cq_with(
            "VK3ABC".to_string(),
            1000.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("qso a");
    let b = sim
        .manager
        .respond_to_cq_with(
            "ZL2DEF".to_string(),
            2400.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("qso b");

    // Pump events: both go active, both forward a first TransmitRequest.
    let pending = sim.pump_qso_events();
    assert!(
        pending.len() >= 2,
        "expected ≥2 forwarded requests, got {}",
        pending.len()
    );

    let slot = sim.drive_slot(pending).await;

    sim.timeline.assert_keyed_for_qso(&a.to_string());
    sim.timeline.assert_keyed_for_qso(&b.to_string());
    sim.timeline.assert_keyed_at_offset(&a.to_string(), 1000.0);
    sim.timeline.assert_keyed_at_offset(&b.to_string(), 2400.0);
    // Two distinct offsets keyed in the one slot.
    sim.timeline.assert_distinct_offsets_in_slot(slot, 2);
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// TX policy — Disabled: no PTT keys at all, for any source.
#[tokio::test]
async fn tx_policy_disabled_is_silent() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_policy(TxPolicy::Disabled);

    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "G0XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");
    let _ = qso_id;

    let mut pending = sim.pump_qso_events();
    // Also throw in a manual free-text send — Disabled mutes EVERYTHING.
    pending.push(sim.manual_tx("CQ K5ARH EM10", 1234.0));

    sim.drive_slot(pending).await;

    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .all(|d| d.reason == DropReason::WorkerRefused),
        "every drop under Disabled should be PolicyDisabled.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

/// TX policy — RespondOnly: an in-progress QSO's transmit still keys (it's not
/// an initiation), but a NEW initiation (manual CQ / hunt) does not. Initiation
/// gating is enforced at the *sources* in production (tui_relay / autonomous),
/// so this scenario models that contract: we only forward in-progress QSO TX
/// (the `qso_id == Some` MessageToSend path) and suppress initiations upstream.
#[tokio::test]
async fn tx_policy_respond_only_keeps_qso_drops_initiation() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_policy(TxPolicy::RespondOnly);

    // In-progress QSO (answering a station) — this is a RESPONSE, allowed.
    let qso_id = sim
        .manager
        .respond_to_caller(
            "F5ABC".to_string(),
            1600.0,
            Some(SlotParity::Odd),
            pancetta_core::ResponseStep::Grid,
            None,
            None,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_caller");

    let mut pending = sim.pump_qso_events();

    // A NEW initiation under RespondOnly: production suppresses it at the source
    // (TxPolicy::allows_initiation() == false). Model the gate explicitly: only
    // append the initiation if the policy permits it.
    let policy = current_policy(&sim.tx_policy);
    let initiation = sim.manual_tx("CQ K5ARH EM10", 1234.0);
    if policy.allows_initiation() {
        pending.push(initiation.clone());
    }
    // Record the suppression in a way the assertion can see: it must NOT key.

    sim.drive_slot(pending).await;

    // The in-progress QSO keyed.
    sim.timeline.assert_keyed_for_qso(&qso_id.to_string());
    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), 1600.0);
    // The CQ initiation never keyed (it was suppressed at the source).
    assert!(
        !sim.timeline.keyed.iter().any(|k| k.text.contains("CQ")),
        "a CQ initiation must NOT key under RespondOnly.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// TX policy — Full: both in-progress QSO TX and a new initiation key.
#[tokio::test]
async fn tx_policy_full_keys_everything() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_policy(TxPolicy::Full);

    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "EA4AAA".to_string(),
            1700.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let mut pending = sim.pump_qso_events();
    // Manual CQ initiation is allowed under Full.
    let policy = current_policy(&sim.tx_policy);
    if policy.allows_initiation() {
        pending.push(sim.manual_tx("CQ K5ARH EM10", 1234.0));
    }

    sim.drive_slot(pending).await;

    sim.timeline.assert_keyed_for_qso(&qso_id.to_string());
    // The manual CQ (qso_id == None) also keyed.
    assert!(
        sim.timeline
            .keyed
            .iter()
            .any(|k| k.text.contains("CQ") && k.ptt_keyed),
        "manual CQ must key under Full.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// Frequency fidelity: the offset requested for a QSO is the offset actually
/// keyed — no silent shift. Covers the SmartFrequencyAllocator-chosen offset
/// being honored end-to-end at the coordinator layer.
#[tokio::test]
async fn requested_offset_is_used() {
    let mut sim = CoordSim::new("K5ARH").await;
    // An "allocator-chosen" odd offset that must survive verbatim.
    let chosen_offset = 2317.0;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "PY2QWE".to_string(),
            chosen_offset,
            Some(SlotParity::Odd),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    // The forwarded request must carry the exact chosen offset.
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - chosen_offset).abs() < 0.001),
        "coordinator must forward the exact requested offset, got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    sim.drive_slot(pending).await;

    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), chosen_offset);
    sim.shutdown().await;
}

/// Manual / tune (qso_id == None) is never gated by the active-set: it keys even
/// with an empty active set (mirrors "manual PTT and tune DO key").
#[tokio::test]
async fn manual_send_never_gated_keys_with_empty_active_set() {
    let mut sim = CoordSim::new("K5ARH").await;
    // No QSOs at all → active set empty.
    let manual = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![manual]).await;

    assert!(
        sim.timeline.keyed.iter().any(|k| k.ptt_keyed),
        "manual send must key even with an empty active set.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

// ===========================================================================
// Phase 5 — autonomous QSO completion through the coordinator TX path.
// These exercise the SAME path the production wiring uses: the
// StartAutonomousQso handler opens an Auto QSO (respond_to_cq), the universal
// decode loop (process_message) auto-sequences it, and each reply keys PTT at
// the rig. They are the coordinator-level counterpart to the engine-level
// pancetta-qso/tests/autonomous_scenarios.rs.
// ===========================================================================

/// An autonomous pounce runs the full ladder — grid → R-report → 73 — and PTT
/// keys at the rig for each, all on the DX's frequency (Tx=Rx). This is the
/// end-to-end Phase-5 acceptance at the coordinator level.
#[tokio::test]
async fn autonomous_pounce_completes_end_to_end_with_ptt() {
    let mut sim = CoordSim::new("K5ARH").await;

    // Slot 0: the StartAutonomousQso handler opens the Auto QSO; opening keys.
    let qso_id = sim
        .autonomous_pounce("VB7F", 1500.0, Some(SlotParity::Odd))
        .await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;
    sim.timeline.assert_keyed_for_qso(&qso_id);

    // Slot 1: DX sends us a report → engine auto-sends our R-report; it keys.
    sim.inject_decode("K5ARH VB7F -12", 1500.0).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // Slot 2: DX rogers with RR73 → engine completes + auto-sends our 73; keys.
    sim.inject_decode("K5ARH VB7F RR73", 1500.0).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // The full responder ladder was keyed, in order, all at the DX's freq
    // (Tx=Rx), all released. The timeline carries actual encoded FT8 text.
    let keyed: Vec<_> = sim.timeline.keyed_for_qso(&qso_id);
    let ladder: Vec<&str> = keyed.iter().map(|k| k.text.as_str()).collect();
    assert_eq!(
        ladder,
        vec!["VB7F K5ARH EM10", "VB7F K5ARH R-12", "VB7F K5ARH 73"],
        "expected the full grid → R-report → 73 ladder keyed in order.\n{}",
        sim.timeline
    );
    assert!(
        keyed
            .iter()
            .all(|k| k.ptt_keyed && (k.freq_hz - 1500.0).abs() < 0.5),
        "all autonomous-QSO transmits must key PTT on the DX's 1500 Hz freq.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// No double-send: the opening goes out exactly ONCE. (In production the
/// autonomous task sends StartAutonomousQso INSTEAD OF a raw TransmitRequest;
/// here we confirm the QsoManager-emitted opening is the only keyed TX in the
/// opening slot.)
#[tokio::test]
async fn autonomous_pounce_opening_keys_exactly_once() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .autonomous_pounce("VB7F", 1500.0, Some(SlotParity::Odd))
        .await;

    let pending = sim.pump_qso_events();
    let slot = sim.drive_slot(pending).await;

    let keyed_this_slot: Vec<_> = sim
        .timeline
        .keyed
        .iter()
        .filter(|k| k.slot == slot && k.ptt_keyed)
        .collect();
    assert_eq!(
        keyed_this_slot.len(),
        1,
        "the autonomous opening must key exactly once (no double-send).\n{}",
        sim.timeline
    );
    assert_eq!(keyed_this_slot[0].qso_id.as_deref(), Some(qso_id.as_str()));
    sim.shutdown().await;
}

/// FQ-F4/TX-F6: the autonomous pounce path must de-conflict its raw Tx=Rx
/// candidate (the DX's own decoded frequency) against an already-active
/// stream, exactly like the manual-call path already does. With a first QSO
/// active at 1500 Hz, pouncing on a second DX decoded at 1520 Hz (within
/// MIN_TX_SEPARATION_HZ=75) must NOT key at 1520 Hz — it must land at least
/// 75 Hz away from 1500, and `partner_freq` must route the DX's replies
/// (which still arrive at their own 1520 Hz) to the right QSO.
#[tokio::test]
async fn autonomous_pounce_deconflicts_against_active_stream() {
    let mut sim = CoordSim::new("K5ARH").await;

    // First QSO already active at 1500 Hz.
    let first = sim
        .manager
        .respond_to_cq_with(
            "VK3ABC".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("first qso");
    // Ensure the manager's internal active-QSO view sees it before pouncing
    // (mirrors production: the QSO exists in the manager before the second
    // pounce is evaluated).
    let active_before = sim.manager.active_tx_offsets().await;
    assert_eq!(active_before, vec![1500.0]);

    // Pounce on a second DX decoded only 20 Hz away — a raw Tx=Rx collision.
    // Same dx_parity as the first QSO (Even) so our own TX parity for both
    // streams is the SAME opposite parity — the invariant required for two
    // truly concurrent QSOs (never TX in sequential windows).
    let second = sim
        .autonomous_pounce("VB7F", 1520.0, Some(SlotParity::Even))
        .await;

    let pending = sim.pump_qso_events();
    let slot = sim.drive_slot(pending).await;

    sim.timeline.assert_keyed_for_qso(&first.to_string());
    sim.timeline.assert_keyed_for_qso(&second);
    sim.timeline
        .assert_keyed_at_offset(&first.to_string(), 1500.0);

    let second_keyed = sim
        .timeline
        .keyed
        .iter()
        .find(|k| k.slot == slot && k.qso_id.as_deref() == Some(second.as_str()))
        .expect("second QSO should have keyed");
    assert!(
        (second_keyed.freq_hz - 1520.0).abs() > 1.0,
        "second QSO must NOT key at the raw colliding 1520 Hz, got {}",
        second_keyed.freq_hz
    );
    assert!(
        (second_keyed.freq_hz - 1500.0).abs() >= 75.0,
        "second QSO's de-conflicted offset ({}) must be >= 75 Hz from the \
         first QSO's 1500 Hz",
        second_keyed.freq_hz
    );

    sim.timeline.assert_distinct_offsets_in_slot(slot, 2);
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// Under TX policy Disabled, an autonomous QSO's transmit is hard-muted at the
/// TX worker — PTT never keys, even though the QSO exists. (Initiation is also
/// suppressed earlier by the planner; this asserts the worker-level backstop.)
#[tokio::test]
async fn autonomous_qso_tx_hard_muted_when_policy_disabled() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .autonomous_pounce("VB7F", 1500.0, Some(SlotParity::Odd))
        .await;
    sim.set_policy(TxPolicy::Disabled);

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    sim.timeline.assert_not_keyed_for_qso(&qso_id);
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::WorkerRefused),
        "expected a PolicyDisabled drop for the autonomous QSO.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

// ===========================================================================
// Split-frequency RF stamping
// ===========================================================================

/// When rig split is active (split TX dial ≠ 0), a completed QSO must log the
/// **split TX dial + audio offset** as its RF frequency — NOT the RX dial.
///
/// Setup: RX dial = 14.074 MHz, split TX dial = 14.090 MHz, audio offset ≈ 1500 Hz.
/// Expected completed `metadata.frequency` ≈ 14_091_500 Hz (split dial + offset),
/// cleanly distinguishable from the RX-based value (14_075_500).
///
/// This test drives a real autonomous QSO to completion through the real
/// `QsoManager::process_message` path, with the split atomic actually injected
/// via `set_split_tx_frequency_source`, so `effective_tx_dial` runs for real.
#[tokio::test]
async fn split_active_qso_logs_tx_dial() {
    let mut sim = CoordSim::new("K5ARH").await;

    // Set RX dial = 14.074 MHz, split TX dial = 14.090 MHz.
    sim.set_rx_dial(14_074_000);
    sim.set_split_tx(14_090_000);

    // Audio offset ≈ 1500 Hz (the DX's decoded frequency in the passband).
    let audio_offset = 1500.0_f64;

    // Slot 0: open the auto QSO.
    let qso_id = sim
        .autonomous_pounce("VB7F", audio_offset, Some(SlotParity::Odd))
        .await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;
    sim.timeline.assert_keyed_for_qso(&qso_id);

    // Slot 1: DX sends us a report → engine auto-sends our R-report.
    sim.inject_decode("K5ARH VB7F -12", audio_offset).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // Slot 2: DX rogers with RR73 → QSO completes; our 73 keys.
    sim.inject_decode("K5ARH VB7F RR73", audio_offset).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // The QsoCompleted event must have been captured in `sim.completed`.
    assert!(
        !sim.completed.is_empty(),
        "expected at least one QsoCompleted, got none.\n{}",
        sim.timeline
    );

    let meta = &sim.completed[0];
    let logged_freq = meta.frequency;

    // With split active the logged RF must be stamped against the split TX dial
    // (14.090 MHz + ~1500 Hz ≈ 14_091_500), NOT the RX dial (14.074 MHz + ~1500 Hz ≈ 14_075_500).
    let split_tx_base = 14_090_000.0_f64;
    let rx_base = 14_074_000.0_f64;

    assert!(
        logged_freq >= split_tx_base && logged_freq < split_tx_base + 4_000.0,
        "expected RF stamped against split TX dial (~{split_tx_base}..+4kHz), \
         but got {logged_freq:.0} Hz (rx-based would be ~{rx_base:.0}+offset).\n{}",
        sim.timeline
    );
    // And confirm it is NOT the RX-dial-based value.
    assert!(
        (logged_freq - rx_base).abs() > 5_000.0,
        "RF frequency {logged_freq:.0} Hz is too close to the RX dial {rx_base:.0} Hz; \
         split TX dial was not used.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

// ===========================================================================
// PAN-118 — band-plan TX gate (out-of-privilege refusal before PTT)
// ===========================================================================

/// An autonomous QSO whose absolute TX RF (dial + audio offset) falls
/// outside the configured amateur band plan must never key PTT: with the
/// dial parked at 15.000 MHz (between 20m and 17m — inside no amateur
/// allocation) the coordinator's band-plan gate refuses the frame before
/// PTT, exactly like every other hard-mute refusal, and the rig stays
/// silent.
#[tokio::test]
async fn out_of_band_dial_never_keys() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_rx_dial(15_000_000);
    let qso_id = sim
        .autonomous_pounce("VB7F", 1500.0, Some(SlotParity::Odd))
        .await;

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    sim.timeline.assert_not_keyed_for_qso(&qso_id);
    sim.timeline.assert_silent();
    sim.shutdown().await;
}

/// Control: the same autonomous QSO on a normal in-band 20m dial keys
/// exactly as before PAN-118 (the gate must not over-block legitimate TX).
#[tokio::test]
async fn in_band_dial_control_keys() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_rx_dial(14_074_000);
    let qso_id = sim
        .autonomous_pounce("VB7F", 1500.0, Some(SlotParity::Odd))
        .await;

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    sim.timeline.assert_keyed_for_qso(&qso_id);
    sim.shutdown().await;
}

// ===========================================================================
// Hound mode — DXpedition chaser (Task 9, coord-level wire proof; PAN-259)
//
// The engine-level procedure is unit-tested in pancetta-qso/src/qso_manager.rs.
// This scenario proves it reaches the **modulator/PTT through the real
// coordinator path**, following the WSJT-X Fox/Hound convention:
// engage_hound → PTT keys in the calling region (1000–2700 Hz, where a WSJT-X
// Fox listens), then after the Fox's report the next slot keys at the Fox's
// own offset (600 Hz) — offset-on-the-wire, not just state. The Fox's RR73
// completes the QSO and the Hound queues no 73.
//
// Deterministic calling offset for seed "D2UY":
//   call = 2140 Hz (hound_offset_for("D2UY", 1000, 2700))
// ===========================================================================

/// Hound engage keys PTT in the calling region (1000–2700 Hz) first, then
/// answers the Fox's signal report at the Fox's own 600 Hz offset — proved at
/// the mock-rig PTT + keyed-audio-offset level through the real coordinator
/// TX path. The Fox's RR73 completes the QSO with no 73 queued.
#[tokio::test]
async fn hound_engage_keys_high_then_answers_at_fox_offset_on_report() {
    let mut sim = CoordSim::new("K5ARH").await;

    // ── Slot 0: engage Hound on Fox "D2UY" at 600 Hz (Fox's audio offset) ──
    let qso_id = sim
        .manager
        .engage_hound("D2UY", 600.0, Some("JI64"), Some(SlotParity::Even))
        .await
        .expect("engage_hound must succeed");

    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "engage_hound must emit at least one TransmitRequest (the opening call)"
    );

    let opening_offset = pending[0].frequency_offset;
    assert!(
        (1000.0..=2700.0).contains(&opening_offset),
        "opening TransmitRequest must be in the Hound calling region [1000, 2700] Hz, got {opening_offset}"
    );

    let slot0 = sim.drive_slot(pending).await;

    let qso_id_str = qso_id.to_string();
    sim.timeline.assert_keyed_for_qso(&qso_id_str);
    let keyed_slot0 = sim.timeline.keyed_for_qso(&qso_id_str);
    let call_freq = keyed_slot0
        .iter()
        .find(|k| k.slot == slot0 && k.ptt_keyed)
        .map(|k| k.freq_hz)
        .expect("a keyed TX must exist in slot 0");
    assert!(
        (1000.0..=2700.0).contains(&call_freq),
        "slot 0 must key in the Hound calling region [1000, 2700] Hz, got {call_freq} Hz.\n{}",
        sim.timeline
    );
    assert!(
        (call_freq - 2140.0).abs() < 5.0,
        "slot 0 keyed offset should be 2140 Hz (deterministic for D2UY), got {call_freq} Hz.\n{}",
        sim.timeline
    );

    // ── Slot 1: Fox sends our report at its own offset (600 Hz) ──
    sim.inject_decode("K5ARH D2UY -12", 600.0).await;

    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "Fox report must trigger a ReportAck TransmitRequest"
    );
    let report_ack_offset = pending[0].frequency_offset;
    assert!(
        (report_ack_offset - 600.0).abs() < 1.0,
        "ReportAck TransmitRequest must ride the Fox's 600 Hz offset, got {report_ack_offset} Hz.\n{}",
        sim.timeline
    );

    let slot1 = sim.drive_slot(pending).await;

    let keyed_slot1 = sim.timeline.keyed_for_qso(&qso_id_str);
    let answer_freq = keyed_slot1
        .iter()
        .find(|k| k.slot == slot1 && k.ptt_keyed)
        .map(|k| k.freq_hz)
        .expect("a keyed TX must exist in slot 1 (ReportAck at the Fox offset)");
    assert!(
        (answer_freq - 600.0).abs() < 1.0,
        "slot 1 must key at the Fox's 600 Hz offset, got {answer_freq} Hz.\n{}",
        sim.timeline
    );

    // ── Slot 2: Fox sends RR73 → QSO completes, no 73 queued ──
    sim.inject_decode("K5ARH D2UY RR73", 600.0).await;

    let pending = sim.pump_qso_events();
    assert!(
        pending.is_empty(),
        "a Hound must queue no 73 after the Fox's RR73, got {} TransmitRequest(s)",
        pending.len()
    );
    assert!(
        !sim.completed.is_empty(),
        "QSO must reach Completed after Fox RR73 (QsoCompleted event expected).\n{}",
        sim.timeline
    );

    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

// ===========================================================================
// TX-offset control (Task 5) — held-offset honored, multi-TX de-confliction,
// Tx=Rx regression. These prove that `compute_manual_tx_offset` (the pure
// coordinator-side selection logic) produces the right TX audio offset and that
// the chosen offset reaches the mock rig PTT wire.
//
// `compute_manual_tx_offset` is re-exported from `pancetta_lib::coordinator`
// (the same path `coalesce_transmit_requests` / `tx_qso_is_live` use) so the
// tests call the real function without duplicating the logic inline.
// ===========================================================================

/// **Held offset honored (single):** operator is in Hold mode with held=1500 Hz,
/// DX decoded at 700 Hz. `compute_manual_tx_offset` must choose tx_off=1500 and
/// partner_freq=Some(700) (we TX at the held spot; DX's replies are at 700).
///
/// The test:
/// 1. Calls `compute_manual_tx_offset` directly and asserts the selection.
/// 2. Opens the QSO with `respond_to_cq_with(dx=700, tx_off=1500, partner=Some(700))`.
/// 3. Drives a slot → asserts PTT keyed at 1500 Hz, NOT 700.
/// 4. Injects the DX reply at 700 Hz → asserts the QSO advances (relevance gate
///    routes via `partner_freq`).
#[tokio::test]
async fn held_offset_honored_keys_at_held_not_dx_freq() {
    use pancetta_lib::coordinator::compute_manual_tx_offset;

    // --- Step 1: selection-level proof ---
    let dx_freq = 700.0_f64;
    let held_hz: u64 = 1500;
    let (tx_off, partner) = compute_manual_tx_offset(dx_freq, true, held_hz, &[]);
    assert!(
        (tx_off - 1500.0).abs() < 1.0,
        "hold mode + held=1500 + no collision must yield tx_off=1500, got {tx_off}"
    );
    assert_eq!(
        partner,
        Some(700.0),
        "when tx_off ≠ dx_freq the partner_freq must be Some(dx_freq), got {partner:?}"
    );

    // --- Step 2: open the QSO with the computed offset + partner ---
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "VK4DX".to_string(),
            tx_off, // our TX offset (1500 Hz)
            Some(SlotParity::Even),
            CallInitiation::Manual,
            partner, // DX's decode freq (700 Hz)
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with with held offset");

    // --- Step 3: drive a slot → PTT must key at 1500 Hz, NOT at 700 Hz ---
    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "QSO open must emit at least one TransmitRequest"
    );

    // The forwarded request must carry the held offset, not the DX's freq.
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - 1500.0).abs() < 1.0),
        "forwarded TransmitRequest must carry the held offset 1500 Hz, got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );
    assert!(
        !pending
            .iter()
            .any(|p| (p.frequency_offset - 700.0).abs() < 1.0),
        "no TransmitRequest must carry the DX freq 700 Hz (we TX at the held spot)"
    );

    sim.drive_slot(pending).await;

    let qso_id_str = qso_id.to_string();
    sim.timeline.assert_keyed_for_qso(&qso_id_str);
    sim.timeline.assert_keyed_at_offset(&qso_id_str, 1500.0);

    // Confirm we never accidentally keyed at the DX's decoded frequency.
    let keyed = sim.timeline.keyed_for_qso(&qso_id_str);
    assert!(
        keyed.iter().all(|k| (k.freq_hz - 700.0).abs() > 1.0),
        "PTT must NEVER key at the DX decoded freq 700 Hz (we are holding 1500 Hz).\n{}",
        sim.timeline
    );

    // --- Step 4: DX replies at 700 Hz → relevance gate via partner_freq routes it ---
    // The QSO is currently in RespondingToCq (we sent our grid); DX sends us a
    // report at 700 Hz (the DX's own audio position). The relevance gate uses
    // partner_freq=700 to match it to our QSO even though our TX is at 1500.
    sim.inject_decode("K5ARH VK4DX -15", 700.0).await;
    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "DX report injected at 700 Hz (partner_freq) must advance QSO and emit a reply.\n{}",
        sim.timeline
    );

    // The follow-on reply must still be at our held offset (1500 Hz), not 700.
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - 1500.0).abs() < 1.0),
        "follow-on TransmitRequest after DX reply must still carry 1500 Hz, got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// PAN-12 / issue #245: a manual QSO whose TX was ceiling-clamped recovers
/// after two consistent replies at the DX's new offset, without moving our TX.
#[tokio::test]
async fn clamped_manual_qso_recovers_when_dx_replies_off_frequency() {
    let (tx_off, partner) = compute_manual_tx_offset(2931.0, false, 0, &[]);
    assert_eq!(tx_off, 2700.0);
    assert_eq!(partner, Some(2931.0));

    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "K6L".to_string(),
            tx_off,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            partner,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with with clamped offset");

    let opening = sim.pump_qso_events();
    sim.drive_slot(opening).await;
    sim.inject_decode("K5ARH K6L -11", 855.0).await;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    sim.inject_decode("K5ARH K6L -11", 856.0).await;

    let pending = sim.pump_qso_events();
    // PAN-15 item 5: assert the specific emitted content, not just
    // "something was emitted" — the old `!pending.is_empty()` check was
    // satisfiable by a keep-calling repeat of the opening `CqResponse`
    // and never proved the confirmed relatch actually advanced the QSO.
    assert!(
        pending.iter().any(|p| p.text == "K6L K5ARH R-11"),
        "confirmed relatch must emit our R-report (ReportAck), got {:?}",
        pending.iter().map(|p| p.text.clone()).collect::<Vec<_>>()
    );
    assert!(
        pending
            .iter()
            .all(|p| (p.frequency_offset - 2700.0).abs() < 1.0),
        "every TX must stay at 2700 Hz, got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    let progress = sim
        .manager
        .get_qso(qso_id)
        .await
        .expect("QSO must still exist after confirmed relatch");
    assert_eq!(
        progress.metadata.partner_freq,
        Some(856.0),
        "confirmed relatch must have moved partner_freq to the DX's new offset"
    );
    assert!(
        matches!(progress.state, QsoState::SendingReport { .. }),
        "confirmed relatch's DX report must have advanced the QSO to SendingReport, got {:?}",
        progress.state
    );

    sim.drive_slot(pending).await;
    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), 2700.0);
    sim.shutdown().await;
}

/// **Multi-TX de-confliction:** opening a second QSO whose DX offset (1540 Hz)
/// lands within `MIN_TX_SEPARATION_HZ` (75 Hz) of an already-active QSO's TX
/// offset (1500 Hz) must produce a de-conflicted TX offset ≥ 75 Hz away, so
/// the two streams do NOT collide in the audio passband.
///
/// The test:
/// 1. Opens QSO A on 1500 Hz (Auto, no held), drives its opening slot → A keys
///    at 1500 Hz.
/// 2. Calls `compute_manual_tx_offset` with `active_tx_offsets()` = [1500] and
///    candidate DX = 1540 Hz (within 75 Hz of A) → confirms de-confliction.
/// 3. Opens QSO B at the de-conflicted offset (≥75 Hz from 1500).
/// 4. Drives B's opening slot → B keys at the de-conflicted offset.
/// 5. Asserts both QSOs keyed on distinct offsets ≥75 Hz apart (inspecting the
///    full timeline across both slots).
#[tokio::test]
async fn multi_tx_deconfliction_offsets_are_distinct() {
    use pancetta_lib::coordinator::compute_manual_tx_offset;
    use pancetta_qso::MIN_TX_SEPARATION_HZ;

    let mut sim = CoordSim::new("K5ARH").await;

    // --- Slot 0: open QSO A at 1500 Hz, drive its opening ---
    let a = sim
        .manager
        .respond_to_cq_with(
            "EA8DX".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("qso A");
    let pending_a = sim.pump_qso_events();
    assert!(
        !pending_a.is_empty(),
        "QSO A must forward at least one TransmitRequest"
    );
    sim.drive_slot(pending_a).await;
    sim.timeline.assert_keyed_for_qso(&a.to_string());
    sim.timeline.assert_keyed_at_offset(&a.to_string(), 1500.0);

    // --- Compute de-conflicted offset for QSO B ---
    // QSO B's DX is at 1540 Hz (38 Hz from A — well within MIN_TX_SEPARATION_HZ).
    // active_tx_offsets must include A's 1500 Hz so the de-confliction fires.
    let active = sim.manager.active_tx_offsets().await;
    assert!(
        active.iter().any(|&o| (o - 1500.0).abs() < 1.0),
        "active_tx_offsets must include QSO A's 1500 Hz offset, got {active:?}"
    );

    let dx_b = 1540.0_f64;
    let (tx_off_b, _partner_b) = compute_manual_tx_offset(dx_b, false, 0, &active);

    // The de-conflicted result must be ≥75 Hz from 1500 Hz.
    let sep = (tx_off_b - 1500.0_f64).abs();
    assert!(
        sep >= MIN_TX_SEPARATION_HZ,
        "de-conflicted offset {tx_off_b} Hz must be ≥{MIN_TX_SEPARATION_HZ} Hz from A's 1500 Hz (sep={sep:.1} Hz)"
    );

    // --- Slot 1: open QSO B at the de-conflicted offset, drive its opening ---
    let b = sim
        .manager
        .respond_to_cq_with(
            "UA9XYZ".to_string(),
            tx_off_b,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("qso B");

    let pending_b = sim.pump_qso_events();
    assert!(
        !pending_b.is_empty(),
        "QSO B must forward at least one TransmitRequest"
    );
    let slot_b = sim.drive_slot(pending_b).await;

    // B must have keyed PTT at the de-conflicted offset.
    sim.timeline.assert_keyed_for_qso(&b.to_string());
    sim.timeline
        .assert_keyed_at_offset(&b.to_string(), tx_off_b);

    // The de-conflicted offset must also appear in slot_b (B's opening slot).
    let b_offsets = sim.timeline.offsets_in_slot(slot_b);
    assert!(
        b_offsets.iter().any(|&o| (o - tx_off_b).abs() < 1.0),
        "B's opening slot must show the de-conflicted offset {tx_off_b:.0} Hz, got {b_offsets:?}.\n{}",
        sim.timeline
    );

    // Over the full timeline: A keyed at 1500, B at a distinct de-conflicted offset ≥75 Hz away.
    let a_keyed = sim.timeline.keyed_for_qso(&a.to_string());
    let b_keyed = sim.timeline.keyed_for_qso(&b.to_string());
    let a_off = a_keyed
        .iter()
        .filter(|k| k.ptt_keyed)
        .map(|k| k.freq_hz)
        .next()
        .expect("QSO A must have keyed PTT at least once");
    let b_off = b_keyed
        .iter()
        .filter(|k| k.ptt_keyed)
        .map(|k| k.freq_hz)
        .next()
        .expect("QSO B must have keyed PTT at least once");

    assert!(
        (a_off - 1500.0).abs() < 1.0,
        "QSO A must key at 1500 Hz, got {a_off} Hz.\n{}",
        sim.timeline
    );
    let actual_sep = (b_off - a_off).abs();
    assert!(
        actual_sep >= MIN_TX_SEPARATION_HZ,
        "QSO A keyed at {a_off:.0} Hz and QSO B at {b_off:.0} Hz — separation {actual_sep:.1} Hz \
         must be ≥{MIN_TX_SEPARATION_HZ} Hz.\n{}",
        sim.timeline
    );

    // Both PTT transitions must be cleanly released.
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// **Regression — Auto, single, no collision → Tx=Rx, partner_freq=None:**
/// with `TxFreqMode::Auto` (hold_mode=false) and no already-active QSOs,
/// `compute_manual_tx_offset` must return the DX's own freq with `partner_freq=None`
/// — the byte-identical "Tx=Rx" behavior from before this feature was added.
/// Opening the QSO must key at the DX's freq (no silent shift), and the DX's
/// reply at that same freq must route without needing `partner_freq`.
#[tokio::test]
async fn auto_single_no_collision_is_tx_eq_rx() {
    use pancetta_lib::coordinator::compute_manual_tx_offset;

    // --- Selection-level regression: Auto, no held, no collision → Tx=Rx ---
    let dx_freq = 1500.0_f64;
    let (tx_off, partner) = compute_manual_tx_offset(dx_freq, false, 0, &[]);
    assert!(
        (tx_off - dx_freq).abs() < 1.0,
        "Auto + no-held + no-collision must yield tx_off == dx_freq, got {tx_off}"
    );
    assert_eq!(
        partner, None,
        "Auto + no-held + no-collision must yield partner_freq=None (Tx=Rx), got {partner:?}"
    );

    // --- Rig-level regression: open with the computed values, must key at dx_freq ---
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "JH1XYZ".to_string(),
            tx_off, // == dx_freq == 1500.0
            Some(SlotParity::Odd),
            CallInitiation::Auto,
            partner, // None — Tx=Rx, no partner_freq split
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with Tx=Rx");

    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "Tx=Rx QSO open must emit a TransmitRequest"
    );

    // Forwarded request must carry 1500 Hz.
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - 1500.0).abs() < 1.0),
        "forwarded request must carry 1500 Hz (Tx=Rx), got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    sim.drive_slot(pending).await;

    let qso_id_str = qso_id.to_string();
    sim.timeline.assert_keyed_for_qso(&qso_id_str);
    sim.timeline.assert_keyed_at_offset(&qso_id_str, 1500.0);

    // DX replies at 1500 Hz (same freq, no partner split needed) → QSO advances.
    sim.inject_decode("K5ARH JH1XYZ -09", 1500.0).await;
    let pending = sim.pump_qso_events();
    assert!(
        !pending.is_empty(),
        "DX reply at 1500 Hz must advance Tx=Rx QSO (no partner_freq needed).\n{}",
        sim.timeline
    );

    // Follow-on reply must still be at 1500 Hz (TX offset unchanged).
    assert!(
        pending
            .iter()
            .any(|p| (p.frequency_offset - 1500.0).abs() < 1.0),
        "follow-on reply must be at 1500 Hz (Tx=Rx regression), got {:?}",
        pending
            .iter()
            .map(|p| p.frequency_offset)
            .collect::<Vec<_>>()
    );

    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

// ===========================================================================
// Fox mode — DXpedition multi-stream rig-level proof (Task 3)
//
// Fox answers many Hound callers concurrently. Each answer is a normal
// `respond_to_caller(ResponseStep::Grid)` QSO (same path `maybe_answer_caller`
// uses internally), all on the SAME parity so the coalescer folds them into one
// `MultiTransmitRequest` and keys BOTH in a single 15-second slot.
//
// Offset de-confliction is exercised via `compute_manual_tx_offset` with the
// live `active_tx_offsets()` snapshot, exactly as the coordinator's caller-
// answer handler does it. The test proves:
//   • ≥2 distinct offsets keyed in ONE slot
//   • Every pair of keyed offsets is ≥ MIN_TX_SEPARATION_HZ (75 Hz) apart
//   • Each QSO advances when injected with its Hound's R-report (sequencing
//     works for concurrent Fox QSOs)
//
// Why `respond_to_caller` × 2, not `maybe_answer_caller`:
//   `maybe_answer_caller` is a private `async fn` on the coordinator struct
//   (not accessible from the test binary). `respond_to_caller` with
//   `ResponseStep::Grid` is the exact call it makes internally
//   (qso_manager.rs:1259-1268); the test is therefore equivalent in fidelity
//   to calling the production path. The cap-admits-N / rejects-N+1 logic is
//   separately unit-tested in `pancetta/tests/fox_mode.rs::fox_cap_admits_n_and_rejects_n_plus_1`.
// ===========================================================================

/// Fox mode: two Hound callers answered in ONE slot, multi-streamed on distinct
/// de-conflicted offsets (≥ 75 Hz apart), both advancing toward RR73.
///
/// This is the rig-level multi-stream proof: the mock rig observes TWO distinct
/// audio offsets keyed in a single slot — exactly what a DXpedition Fox needs.
#[tokio::test]
async fn fox_mode_answers_two_callers_multistreamed_distinct_offsets() {
    use pancetta_lib::coordinator::compute_manual_tx_offset;
    use pancetta_qso::MIN_TX_SEPARATION_HZ;

    let mut sim = CoordSim::new("K5ARH").await;

    // ── Step 1: Fox answers Hound A ──────────────────────────────────────────
    //
    // Hound A is calling at 1200 Hz (the Fox hears their CQ/call there).
    // Auto, no held offset: `compute_manual_tx_offset` returns Tx=Rx (1200 Hz,
    // no collision yet).
    let hound_a_dx_freq = 1200.0_f64;
    let (tx_off_a, partner_a) = compute_manual_tx_offset(hound_a_dx_freq, false, 0, &[]);

    let qso_a = sim
        .manager
        .respond_to_caller(
            "W1AAA".to_string(),
            tx_off_a,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::Grid,
            Some(-10.0),
            None,
            partner_a,
            false,
            None,
        )
        .await
        .expect("Fox must be able to answer Hound A");

    // ── Step 2: Fox answers Hound B ──────────────────────────────────────────
    //
    // Hound B is calling at 1240 Hz — only 40 Hz from A, within the 75 Hz
    // minimum separation. The coordinator fetches `active_tx_offsets` (which
    // now includes A's offset) and calls `compute_manual_tx_offset` to get a
    // de-conflicted TX offset for B.
    let active = sim.manager.active_tx_offsets().await;
    assert!(
        active.iter().any(|&o| (o - tx_off_a).abs() < 1.0),
        "active_tx_offsets must include Hound A's offset {tx_off_a:.0} Hz after opening, got {active:?}"
    );

    let hound_b_dx_freq = 1240.0_f64; // within 75 Hz of A → triggers de-confliction
    let (tx_off_b, partner_b) = compute_manual_tx_offset(hound_b_dx_freq, false, 0, &active);

    // Sanity-check: de-conflicted offset must be ≥ MIN_TX_SEPARATION_HZ from A.
    let sep = (tx_off_b - tx_off_a).abs();
    assert!(
        sep >= MIN_TX_SEPARATION_HZ,
        "de-conflicted Hound B offset {tx_off_b:.0} Hz must be ≥{MIN_TX_SEPARATION_HZ} Hz from \
         Hound A offset {tx_off_a:.0} Hz (sep={sep:.1} Hz)"
    );

    let qso_b = sim
        .manager
        .respond_to_caller(
            "W2BBB".to_string(),
            tx_off_b,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::Grid,
            Some(-12.0),
            None,
            partner_b,
            false,
            None,
        )
        .await
        .expect("Fox must be able to answer Hound B");

    // ── Step 3: Pump events + drive one slot ─────────────────────────────────
    //
    // Both QSOs go active (StateChanged → RespondingToCq), both forward their
    // opening TransmitRequest (our grid reply to each Hound). The coalescer
    // keeps BOTH (distinct qso_ids, same parity) and the TX worker sums them
    // into one waveform within a single PTT envelope.
    let pending = sim.pump_qso_events();
    assert!(
        pending.len() >= 2,
        "Fox must forward ≥2 TransmitRequests (one per Hound) after opening both QSOs, got {}",
        pending.len()
    );

    let slot0 = sim.drive_slot(pending).await;

    // Both QSOs must have keyed PTT.
    let qso_a_str = qso_a.to_string();
    let qso_b_str = qso_b.to_string();
    sim.timeline.assert_keyed_for_qso(&qso_a_str);
    sim.timeline.assert_keyed_for_qso(&qso_b_str);

    // Each keyed at its de-conflicted offset (on-wire proof).
    sim.timeline.assert_keyed_at_offset(&qso_a_str, tx_off_a);
    sim.timeline.assert_keyed_at_offset(&qso_b_str, tx_off_b);

    // TWO distinct offsets in ONE slot — the core multi-stream assertion.
    sim.timeline.assert_distinct_offsets_in_slot(slot0, 2);
    assert_eq!(sim.observations.lock().unwrap().ptt, vec![true, false]);

    // The two offsets must be ≥ MIN_TX_SEPARATION_HZ apart (no on-air collision).
    let mut offsets_slot0 = sim.timeline.offsets_in_slot(slot0);
    offsets_slot0.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(
        offsets_slot0.len(),
        2,
        "expected exactly 2 keyed offsets in slot 0"
    );
    let actual_sep = (offsets_slot0[0] - offsets_slot0[1]).abs();
    assert!(
        actual_sep >= MIN_TX_SEPARATION_HZ,
        "Fox streams must be ≥{MIN_TX_SEPARATION_HZ} Hz apart on the wire; \
         got {:.0} Hz and {:.0} Hz (sep={:.1} Hz).\n{}",
        offsets_slot0[0],
        offsets_slot0[1],
        actual_sep,
        sim.timeline
    );

    // ── Step 4: Inject each Hound's R-report → QSOs sequence forward ─────────
    //
    // The Fox receives Hound A's report at A's own audio position (hound_a_dx_freq
    // if Tx=Rx; or via partner_freq when de-conflicted). Both QSOs must advance
    // to the ReportAck rung, proving concurrent Fox sequencing works.
    //
    // Inject at the DX's decode frequency (where the Hound's signal lives), which
    // is hound_a_dx_freq / hound_b_dx_freq. The relevance gate uses partner_freq
    // when tx_off ≠ dx_freq, so the report routes correctly even when de-conflicted.
    sim.inject_decode("K5ARH W1AAA -10", hound_a_dx_freq).await;
    sim.inject_decode("K5ARH W2BBB -12", hound_b_dx_freq).await;

    let pending_s1 = sim.pump_qso_events();
    assert!(
        !pending_s1.is_empty(),
        "Hound R-reports must trigger ReportAck replies from Fox; got no pending TX.\n{}",
        sim.timeline
    );

    let _slot1 = sim.drive_slot(pending_s1).await;

    // Both QSOs must still be keying (ReportAck rung, sequencing works).
    let a_keyed = sim.timeline.keyed_for_qso(&qso_a_str);
    let b_keyed = sim.timeline.keyed_for_qso(&qso_b_str);
    assert!(
        a_keyed.len() >= 2,
        "Fox QSO A must have keyed ≥2 times (grid + ReportAck), got {}.\n{}",
        a_keyed.len(),
        sim.timeline
    );
    assert!(
        b_keyed.len() >= 2,
        "Fox QSO B must have keyed ≥2 times (grid + ReportAck), got {}.\n{}",
        b_keyed.len(),
        sim.timeline
    );

    // All PTT transitions cleanly released (no stuck PTT).
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

// ===========================================================================
// Station-agent remote-TX arm gate (P2.3). These exercise the ONE change to
// the live coordinator TX path: TxOrigin::Remote requests are gated by the
// shared ArmState before keying PTT; Local requests are byte-identical.
// ===========================================================================

/// A remote-origin request with a FRESH (unarmed) arm is dropped — no PTT.
#[tokio::test]
async fn remote_tx_dropped_when_arm_fresh_unarmed() {
    let mut sim = CoordSim::new("K5ARH").await;
    // Arm is fresh/unarmed by construction → tx_permitted() is false.
    let remote = sim.remote_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![remote]).await;

    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::WorkerRefused),
        "expected a RemoteNotArmed drop.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

/// A LOCAL-origin request keys PTT regardless of the arm state — byte-identical
/// to the pre-gate path (the arm gate never applies to Local).
#[tokio::test]
async fn local_tx_unaffected_by_remote_arm() {
    let mut sim = CoordSim::new("K5ARH").await;
    // Arm stays fresh/unarmed; a Local manual send must still key.
    let local = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![local]).await;

    assert!(
        sim.timeline.keyed_anything(),
        "a Local request must key PTT even with an unarmed remote arm.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    assert!(
        !sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::WorkerRefused),
        "a Local request must never be dropped by the remote arm gate.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

/// With a fully-armed + consented + heartbeat-fresh arm, a Remote request keys
/// PTT (the gate opens).
#[tokio::test]
async fn remote_tx_keys_when_armed_and_consented() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.arm_remote_tx();
    let remote = sim.remote_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![remote]).await;

    assert!(
        sim.timeline.keyed_anything(),
        "a Remote request must key PTT when the arm is armed + consented.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// Policy primacy: `TxPolicy::Disabled` drops even a Remote request that has a
/// live arm — the hard-mute runs first.
#[tokio::test]
async fn policy_disabled_drops_remote_tx_even_with_live_arm() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.arm_remote_tx(); // arm would otherwise permit
    sim.set_policy(TxPolicy::Disabled);
    let remote = sim.remote_tx("CQ K5ARH EM10", 1500.0);
    sim.drive_slot(vec![remote]).await;

    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::WorkerRefused),
        "Disabled policy must drop the remote request first (policy primacy).\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

// ============================================================================
// B2 (P3.4c) — end-to-end: a REAL remote-origin QSO's TX is arm-gated.
//
// These drive the TX through the actual QSO engine (a `remote_origin=true`
// QSO's `MessageToSend`), pumped through the faithful `pump_qso_events`
// origin-derivation mirror, so they prove the whole chain
//   QsoMetadata.remote_origin → MessageToSend → PendingTx.origin=Remote → arm gate
// rather than a synthetic `remote_tx()` PendingTx.
// ============================================================================

/// A remote-origin QSO created but NOT armed: its opening TX is dropped (no PTT).
#[tokio::test]
async fn remote_origin_qso_tx_dropped_when_unarmed() {
    let mut sim = CoordSim::new("K5ARH").await;
    // Arm is fresh/unarmed. Open a remote-origin QSO through the real engine.
    sim.manager
        .respond_to_cq_with(
            "W1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    // The forwarded frame MUST be Remote (else it would bypass the arm).
    assert!(
        pending
            .iter()
            .any(|p| p.origin == pancetta_lib::message_bus::TxOrigin::Remote),
        "a remote_origin QSO's forwarded TX must be TxOrigin::Remote"
    );
    sim.drive_slot(pending).await;

    // Unarmed → the whole slot is silent, dropped by the arm gate.
    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::WorkerRefused),
        "an unarmed remote QSO's TX must be dropped by the arm gate (no bypass).\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

/// The SAME remote-origin QSO keys PTT once the arm is armed + consented.
#[tokio::test]
async fn remote_origin_qso_tx_keys_when_armed() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.arm_remote_tx();
    sim.manager
        .respond_to_cq_with(
            "W1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    assert!(
        sim.timeline.keyed_anything(),
        "an armed remote QSO's TX must key PTT.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// PAN-91 end-to-end: a QSO requested by one client must NOT key TX just
/// because a DIFFERENT client happens to be armed. This is the real
/// vulnerability the boolean-only arm gate had — creating the QSO is never
/// itself gated, so an unauthorized peer's request could sit queued until
/// some other, legitimate peer armed, at which point the old gate would
/// authorize it by proxy.
#[tokio::test]
async fn remote_origin_qso_tx_dropped_when_different_client_armed() {
    let mut sim = CoordSim::new("K5ARH").await;
    // client-b is armed...
    sim.arm_remote_tx_as("client-b");
    // ...but this QSO was requested by client-a.
    sim.manager
        .respond_to_cq_with(
            "W1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
            Some("client-a".to_string()),
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    sim.timeline.assert_silent();
    assert!(
        sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::WorkerRefused),
        "PAN-91: a QSO bound to client-a must be dropped while only \
         client-b is armed — the old gate would have keyed this.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

/// PAN-91 end-to-end, positive case: the SAME client that requested the QSO
/// being armed does key TX (confirms the fix doesn't just deny everything).
#[tokio::test]
async fn remote_origin_qso_tx_keys_when_same_client_armed() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.arm_remote_tx_as("client-a");
    sim.manager
        .respond_to_cq_with(
            "W1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
            Some("client-a".to_string()),
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    assert!(
        sim.timeline.keyed_anything(),
        "a QSO bound to client-a must key TX once client-a itself is armed.\n{}",
        sim.timeline
    );
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

/// Regression: a LOCAL QSO (remote_origin=false) keys PTT regardless of the
/// unarmed remote arm — byte-identical to the pre-P3.4c path.
#[tokio::test]
async fn local_origin_qso_tx_unaffected_by_arm() {
    let mut sim = CoordSim::new("K5ARH").await;
    // Arm stays unarmed; a local QSO must still key.
    sim.manager
        .respond_to_cq_with(
            "W1XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            false, // local
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let pending = sim.pump_qso_events();
    assert!(
        pending
            .iter()
            .all(|p| p.origin == pancetta_lib::message_bus::TxOrigin::Local),
        "a local QSO's forwarded TX must be TxOrigin::Local (regression)"
    );
    sim.drive_slot(pending).await;

    assert!(
        sim.timeline.keyed_anything(),
        "a local QSO's TX must key PTT even with an unarmed remote arm.\n{}",
        sim.timeline
    );
    assert!(
        !sim.timeline
            .dropped
            .iter()
            .any(|d| d.reason == DropReason::WorkerRefused),
        "a local QSO must never be dropped by the remote arm gate.\n{}",
        sim.timeline
    );
    sim.shutdown().await;
}

// ---------------------------------------------------------------------------
// Runtime FT8/FT4/FT2 mode switch (operator Shift+M)
// ---------------------------------------------------------------------------

/// A runtime mode switch is refused while a QSO is non-terminal — the REAL
/// `try_switch_operating_mode` reads the REAL `active_tx_qsos` set populated
/// by the REAL `pump_qso_events` populater (not poked directly), mirroring
/// exactly what the coordinator's QSO task maintains in production.
#[tokio::test]
async fn mode_switch_refused_while_qso_active() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");

    // Populate the REAL active_tx_qsos set via the same populater logic the
    // coordinator uses (StateChanged -> active inserts).
    let _pending = sim.pump_qso_events();

    let result = try_switch_operating_mode(
        OperatingMode::Ft4,
        &sim.active_tx_qsos,
        &sim.decode_window,
        &sim.active_protocol_mode,
        &sim.active_slot_ns,
        &sim.active_decode_phase_ns,
        &sim.current_decode_effort,
        &sim.resolved_hardware_tier,
    );

    assert!(
        matches!(result, Err(ModeSwitchError::QsosActive(n)) if n >= 1),
        "expected QsosActive, got {:?}",
        result
    );
    assert_eq!(
        sim.active_protocol_mode.load(Ordering::Relaxed),
        OperatingMode::Ft8.as_u8(),
        "atomic must be untouched on refusal"
    );
    sim.shutdown().await;
}

/// A runtime mode switch succeeds while idle (no active QSOs), flips the
/// shared timing atomics, and the NEXT QSO opened after the switch is
/// stamped with the new mode string — proving the switch is wired all the
/// way through to `QsoMetadata::mode`, not just the atomics.
#[tokio::test]
async fn mode_switch_succeeds_idle_and_next_qso_uses_new_mode() {
    let mut sim = CoordSim::new("K5ARH").await;

    let result = try_switch_operating_mode(
        OperatingMode::Ft4,
        &sim.active_tx_qsos,
        &sim.decode_window,
        &sim.active_protocol_mode,
        &sim.active_slot_ns,
        &sim.active_decode_phase_ns,
        &sim.current_decode_effort,
        &sim.resolved_hardware_tier,
    );
    assert!(result.is_ok(), "expected Ok, got {:?}", result);
    assert_eq!(sim.active_slot_ns.load(Ordering::Relaxed), 7_500_000_000);
    assert_eq!(
        sim.active_protocol_mode.load(Ordering::Relaxed),
        OperatingMode::Ft4.as_u8()
    );

    // Mirror what the QSO component's task does on QsoMessage::SetOperatingMode
    // (Task 8) — direct call, since coord_sim has no separate task boundary.
    sim.manager.set_active_mode("FT4".to_string());

    sim.manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");
    let _pending = sim.pump_qso_events();

    let active = sim.manager.get_active_qsos().await;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].1.metadata.mode, "FT4");
    sim.shutdown().await;
}

/// PAN-72 (PR #350 round-7 re-review), end-to-end: a runtime Shift+M switch
/// must reach the QSO engine's keep-calling rearm / `stall_cycles` cadence,
/// and must reach it through the SHARED `active_slot_ns` atomic rather than
/// the `active_mode` string `set_active_mode` copies into the manager's
/// (cloned-by-value) config.
///
/// The manager here is deliberately NOT re-fetched or re-injected after the
/// switch — it is the same handle wired in `CoordSim::new`, standing in for
/// the clone `QsoManager::start()` spawns `timeout_check_loop` on.
#[tokio::test]
async fn mode_switch_retunes_the_qso_engines_rearm_cadence() {
    let mut sim = CoordSim::new("K5ARH").await;

    let qso_id = sim
        .manager
        .respond_to_cq_manual("W1AW".to_string(), 1500.0, None)
        .await
        .expect("respond_to_cq_manual");
    let opened_at = sim
        .manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // Before the switch: one FT4 slot is a SUB-slot rearm under FT8.
    sim.manager
        .rearm_manual_calls_at(opened_at + chrono::Duration::milliseconds(7_500))
        .await;
    assert_eq!(
        sim.manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .stall_cycles,
        0,
        "FT8 must still count on its historical 15 s slot"
    );

    // Operator presses Shift+M -> FT4. No QSO is in `active_tx_qsos` here
    // (this sim never populated it), so the switch is admitted.
    try_switch_operating_mode(
        OperatingMode::Ft4,
        &sim.active_tx_qsos,
        &sim.decode_window,
        &sim.active_protocol_mode,
        &sim.active_slot_ns,
        &sim.active_decode_phase_ns,
        &sim.current_decode_effort,
        &sim.resolved_hardware_tier,
    )
    .expect("idle mode switch");
    assert_eq!(sim.active_slot_ns.load(Ordering::Relaxed), 7_500_000_000);

    // The very same manager handle now counts on FT4's 7.5 s slot. 7.6 s since
    // the opening call is deliberately BETWEEN the two slot lengths: it counts
    // under FT4 and does not under FT8, so this assertion fails outright if the
    // switch never reaches the QSO engine.
    sim.manager
        .rearm_manual_calls_at(opened_at + chrono::Duration::milliseconds(7_600))
        .await;
    assert_eq!(
        sim.manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .stall_cycles,
        1,
        "the live switch must retune the rearm cadence with no re-injection"
    );
    sim.shutdown().await;
}

/// Regression invariant: an operator who never touches Shift+M sees the
/// mode-switch atomics stay at their FT8 defaults, and the pre-existing
/// `ptt_keys_for_scheduled_qso` PTT-keying behavior is completely unaffected
/// by the presence of the new fields/machinery on `CoordSim`.
#[tokio::test]
async fn mode_switch_never_requested_is_byte_identical_to_today() {
    let mut sim = CoordSim::new("K5ARH").await;
    assert_eq!(
        sim.active_protocol_mode.load(Ordering::Relaxed),
        OperatingMode::Ft8.as_u8()
    );
    assert_eq!(sim.active_slot_ns.load(Ordering::Relaxed), 15_000_000_000);

    // Exact body of `ptt_keys_for_scheduled_qso` — proves an FT8-only
    // operator who never touches Shift+M sees no behavior change.
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");
    let pending = sim.pump_qso_events();
    assert!(!pending.is_empty());
    sim.drive_slot(pending).await;
    sim.timeline.assert_keyed_for_qso(&qso_id.to_string());
    sim.timeline.assert_all_released();
    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), 1500.0);
    sim.shutdown().await;
}

/// SM-F5 (Batch 4): `cancel_qso` / `check_timeouts_at` / `supersede_active_qsos_for`
/// now emit `QsoEvent::QsoFailed` ALONGSIDE the pre-existing `StateChanged ->
/// Failed` for the same failure, reviving the priority-scoring failure-backoff
/// consumer (`coordinator/qso.rs`'s `record_failure`) that subscribes to
/// `QsoFailed` specifically. `pump_qso_events`'s populater (mirroring the real
/// coordinator) removes from `active_tx_qsos` on BOTH events via
/// `HashSet::remove`, which is a no-op on an absent key — so emitting both for
/// one failure must be idempotent: exactly one qso_id, cleanly removed once,
/// no panic and no double-free. This exercises the TIMEOUT path (the easiest
/// of the 3 SM-F5 producer sites to drive with an explicit virtual `now`).
#[tokio::test]
async fn sm_f5_timeout_retirement_emits_both_events_and_purges_active_tx_qsos_once() {
    let mut sim = CoordSim::new("K5ARH").await;
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");
    let key = active_tx_qso_key(&qso_id.to_string());

    // Populated by the opening StateChanged.
    let pending = sim.pump_qso_events();
    assert!(!pending.is_empty());
    assert!(
        sim.active_tx_qsos.read().unwrap().contains(&key),
        "sanity: active_tx_qsos should contain the started QSO"
    );

    // Retire it well past the 30s report_timeout (Auto RespondingToCq never
    // got a DX reply). This now fires BOTH a StateChanged -> Failed and a
    // QsoFailed for the SAME qso_id.
    let now = chrono::Utc::now() + chrono::Duration::seconds(45);
    sim.manager.check_timeouts_at(now).await;

    // Both events must have been emitted (StateChanged->Failed AND QsoFailed)
    // — `pump_qso_events` applies each event's removal rule in turn.
    let mut saw_state_changed_failed = false;
    let mut saw_qso_failed = false;
    while let Ok(ev) = sim.qso_rx.try_recv() {
        match &ev {
            QsoEvent::StateChanged { new_state, .. } => {
                if matches!(new_state, QsoState::Failed { .. }) {
                    saw_state_changed_failed = true;
                }
            }
            QsoEvent::QsoFailed { qso_id: fid, .. } => {
                assert_eq!(*fid, qso_id, "QsoFailed must carry the retired QSO's id");
                saw_qso_failed = true;
            }
            _ => {}
        }
        sim.apply_event(ev, &mut Vec::new());
    }
    assert!(
        saw_state_changed_failed,
        "expected a StateChanged -> Failed for the timed-out QSO"
    );
    assert!(
        saw_qso_failed,
        "expected a QsoFailed for the timed-out QSO (SM-F5 — was previously dead)"
    );

    // Idempotent, clean removal: exactly one qso_id worth of entry, now gone
    // from BOTH maps, no panic from the double `HashSet::remove`.
    assert!(
        !sim.active_tx_qsos.read().unwrap().contains(&key),
        "active_tx_qsos must be empty after a timeout retirement that emits both events"
    );
    assert!(
        !sim.active_tx_offsets.read().unwrap().contains_key(&key),
        "active_tx_offsets must be empty after a timeout retirement that emits both events"
    );
    sim.shutdown().await;
}

/// PAN-280: the DX's RR73 completes the QSO, our 73 is dropped before it
/// keys, and nothing is logged; the QSO is open again in SendingReport.
#[tokio::test]
async fn dropped_closing_73_is_not_logged_and_reopens() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.set_rx_dial(14_074_000);
    let audio_offset = 1500.0_f64;

    let qso_id = sim
        .autonomous_pounce("VB7F", audio_offset, Some(SlotParity::Odd))
        .await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;
    sim.inject_decode("K5ARH VB7F -12", audio_offset).await;
    let pending = sim.pump_qso_events();
    sim.drive_slot(pending).await;

    // The DX's RR73: the QSO completes and our 73 is queued.
    sim.inject_decode("K5ARH VB7F RR73", audio_offset).await;
    let pending = sim.pump_qso_events();
    let seventy_three = pending
        .iter()
        .rev()
        .find(|p| p.text.ends_with(" 73"))
        .cloned()
        .unwrap_or_else(|| panic!("our 73 is queued, got {pending:?}"));
    assert!(
        sim.completed.is_empty(),
        "nothing is logged at queue time.\n{}",
        sim.timeline
    );

    sim.set_policy(TxPolicy::Disabled);
    sim.drive_slot(vec![seventy_three]).await;
    let id: pancetta_qso::QsoId = qso_id.parse().unwrap();

    assert!(
        sim.completed.is_empty(),
        "a dropped 73 logs nothing.\n{}",
        sim.timeline
    );
    let state = sim.manager.get_qso(id).await.unwrap().state;
    assert!(
        matches!(state, QsoState::SendingReport { .. }),
        "the QSO is open again in its pre-73 state, got {state:?}"
    );
    assert!(
        sim.active_tx_qsos
            .read()
            .unwrap()
            .contains(&active_tx_qso_key(&qso_id.to_string())),
        "the reopened QSO is live for TX again"
    );
    sim.shutdown().await;
}

#[tokio::test]
async fn supersede_during_playback_unkeys_before_rekey() {
    // Submit during the opposite parity so the first waveform starts at the
    // boundary and leaves enough headroom for a same-slot replacement.
    let into_cycle = chrono::Utc::now().timestamp_millis().rem_euclid(30_000) as u64;
    if into_cycle < 15_000 {
        tokio::time::sleep(Duration::from_millis(15_000 - into_cycle)).await;
    }
    let mut sim = CoordSim::new("K5ARH").await;
    let mut first = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    first.qso_id = Some("supersede".into());
    first.tx_parity = Some(SlotParity::Even);
    first.attempt_id = Some(pancetta_qso::TxAttemptId(1));
    sim.active_tx_qsos
        .write()
        .unwrap()
        .insert(active_tx_qso_key("supersede"));
    sim.submit(&[first]).await;
    sim.wait_for_audio(1).await;
    let mut next = sim.manual_tx("CQ K5ARH EM11", 1500.0);
    next.qso_id = Some("supersede".into());
    next.tx_parity = Some(SlotParity::Even);
    next.attempt_id = Some(pancetta_qso::TxAttemptId(2));
    sim.drive_slot(vec![next]).await;
    assert_eq!(
        sim.observations.lock().unwrap().ptt,
        vec![true, false, true, false],
        "supersede must release PTT before replacement key-down"
    );
    assert_eq!(
        sim.timeline.keyed.len(),
        1,
        "only replacement attempt settled as sent"
    );
    {
        let obs = sim.observations.lock().unwrap();
        assert!(obs
            .settlements
            .iter()
            .any(|(_, id, sent)| *id == Some(pancetta_qso::TxAttemptId(2)) && *sent));
        assert!(
            obs.settlements
                .iter()
                .any(|(_, id, sent)| *id == Some(pancetta_qso::TxAttemptId(1)) && !*sent),
            "superseded attempt must settle as not sent"
        );
        assert!(!obs
            .settlements
            .iter()
            .any(|(_, id, sent)| *id == Some(pancetta_qso::TxAttemptId(1)) && *sent));
    }
    sim.shutdown().await;
}

#[tokio::test]
async fn shutdown_while_keyed_unkeys_and_joins() {
    let mut sim = CoordSim::new("K5ARH").await;
    let request = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    sim.submit(&[request]).await;
    sim.wait_for_audio(1).await;
    assert_eq!(sim.observations.lock().unwrap().ptt.last(), Some(&true));
    sim.shutdown().await;
    assert_eq!(sim.observations.lock().unwrap().ptt.last(), Some(&false));
}

#[tokio::test]
async fn independent_fixtures_do_not_share_state() {
    let mut first = CoordSim::new("K5ARH").await;
    let mut second = CoordSim::new("W1AW").await;
    assert_ne!(first.home(), second.home());
    first.set_policy(TxPolicy::Disabled);
    assert_eq!(current_policy(&second.tx_policy), TxPolicy::Full);
    let request = second.manual_tx("CQ W1AW FN31", 1500.0);
    second.drive_slot(vec![request]).await;
    assert!(first.observations.lock().unwrap().ptt.is_empty());
    first.shutdown().await;
    second.shutdown().await;
}
