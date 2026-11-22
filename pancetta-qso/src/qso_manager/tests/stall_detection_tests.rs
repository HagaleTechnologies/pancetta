// PAN-72. Formerly the inline module `pan72_stall_detection_tests`.
//! PAN-72: silence-driven stall detection + Switch/Revert event emission.
//!
//! Replaces the old identical/differing-frame `DX_STUCK_REPEAT_THRESHOLD`
//! hop (see the deleted `stuck_dx_tests` cases) with a single counter
//! (`QsoMetadata::stall_cycles`) incremented solely by
//! `QsoManager::rearm_manual_calls_at` on each real re-send that feeds a
//! not-yet-counted own-parity TX slot — i.e. driven by *silence* (the DX
//! not responding on our transmit slots), not by inspecting incoming
//! frame content, and once per real TX opportunity rather than once per
//! protocol slot (PAN-128). A genuine forward advance resets the counter
//! and records the current offset as
//! `QsoMetadata::last_known_good_offset_hz`. When the counter trips
//! `TimeoutConfig::qso_stall_switch_after` (Auto TX-freq mode only), the
//! manager emits `QsoEvent::TxOffsetActionNeeded`: `Switch` if we're
//! still on the known-good offset (or none is recorded yet), `Revert` if
//! we're on some other (previously-switched) offset.
use super::*;
use jiff::SignedDuration;
use pancetta_core::time::SignedDurationExt as _;
use pancetta_core::time::UtcTimestamp;

fn clocked_manager(config: QsoManagerConfig) -> QsoManager {
    QsoManager::with_clock_for_test(config, PinnedClock::new().source())
}

#[derive(Clone)]
struct PinnedClock(Arc<std::sync::Mutex<UtcTimestamp>>);
impl PinnedClock {
    fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(
            pancetta_core::time::slot_grid_origin(),
        )))
    }
    fn now(&self) -> UtcTimestamp {
        *self.0.lock().unwrap()
    }
    fn advance(&self, by: SignedDuration) {
        *self.0.lock().unwrap() += by;
    }
    fn source(&self) -> pancetta_core::time::UtcClock {
        let clock = self.clone();
        Arc::new(move || clock.now())
    }
}

async fn time_on_answer(manager: &QsoManager, bare: bool, to: &str) {
    let message = if bare {
        MessageType::SignalReport {
            to_station: to.into(),
            from_station: DX.into(),
            report: -10,
        }
    } else {
        MessageType::CqResponse {
            calling_station: to.into(),
            responding_station: DX.into(),
            grid: Some("FN31".into()),
        }
    };
    manager
        .process_message(message, format!("{to} {DX} FN31"), FREQ, Some(-10.0))
        .await
        .unwrap();
}

async fn check_completed_time_on(bare: bool) {
    let clock = PinnedClock::new();
    let manager = QsoManager::with_clock_for_test(test_config(), clock.source());
    let id = manager.start_cq(FREQ, None, false, None).await.unwrap();
    let created = clock.now();
    clock.advance(SignedDuration::from_mins(4));
    let answered = clock.now();
    time_on_answer(&manager, bare, OUR).await;
    clock.advance(SignedDuration::from_secs(30));
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -12,
            },
            format!("{OUR} {DX} R-12"),
            FREQ,
            Some(-11.0),
        )
        .await
        .unwrap();
    clock.advance(SignedDuration::from_secs(30));
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{OUR} {DX} 73"),
            FREQ,
            Some(-11.0),
        )
        .await
        .unwrap();
    let qso = manager.get_qso(id).await.unwrap();
    assert!(matches!(qso.state, QsoState::Completed { .. }));
    assert_eq!(qso.metadata.start_time, created);
    assert_eq!(qso.metadata.first_answer_at, Some(answered));
    assert_eq!(
        crate::adif::AdifProcessor::new()
            .qso_to_adif(&qso.metadata, None)
            .qso_date,
        answered
    );
}

#[tokio::test]
async fn cqer_time_on_is_first_answer_not_cq_start() {
    check_completed_time_on(false).await;
}

#[tokio::test]
async fn cqer_time_on_bare_report_answer() {
    check_completed_time_on(true).await;
}

#[tokio::test]
async fn cqer_time_on_rejected_and_repeated_answers() {
    let clock = PinnedClock::new();
    let manager = QsoManager::with_clock_for_test(test_config(), clock.source());
    let id = manager.start_cq(FREQ, None, false, None).await.unwrap();
    clock.advance(SignedDuration::from_mins(4));
    time_on_answer(&manager, false, "W9XYZ").await;
    let qso = manager.get_qso(id).await.unwrap();
    assert!(matches!(qso.state, QsoState::CallingCq { .. }));
    assert_eq!(qso.metadata.first_answer_at, None);
    time_on_answer(&manager, false, OUR).await;
    let answered = clock.now();
    clock.advance(SignedDuration::from_secs(30));
    time_on_answer(&manager, false, OUR).await;
    assert_eq!(
        manager.get_qso(id).await.unwrap().metadata.first_answer_at,
        Some(answered)
    );
}

#[tokio::test]
async fn manual_cq_time_on_rolls_over_utc_midnight() {
    let clock = PinnedClock::new();
    clock.advance(SignedDuration::from_hours(23) + SignedDuration::from_mins(58));
    let manager = QsoManager::with_clock_for_test(test_config(), clock.source());
    let id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let created = clock.now();
    clock.advance(SignedDuration::from_mins(4));
    time_on_answer(&manager, false, OUR).await;
    let metadata = manager.get_qso(id).await.unwrap().metadata;
    assert_eq!(metadata.start_time, created);
    assert_eq!(metadata.first_answer_at, Some(clock.now()));
    let adif = crate::adif::AdifProcessor::new().qso_to_adif(&metadata, None);
    assert_eq!(adif.qso_date, clock.now());
    assert_ne!(adif.qso_date.to_civil().date(), created.to_civil().date());
}

#[tokio::test]
async fn manual_cq_contest_time_survives_adif_and_index_round_trip() {
    use crate::contest::{
        ledger,
        scoring::{ContestPeriod, ContestScoringContext, KSQP_FT_2026},
    };
    let clock = PinnedClock::new();
    let created = clock.now();
    let context = ContestScoringContext::new(
        "PAN-307-test",
        "us-state-qso-party",
        KSQP_FT_2026,
        "TX",
        vec![ContestPeriod {
            start: created,
            end: created + SignedDuration::from_hours(1),
        }],
    )
    .unwrap();
    let mut config = test_config();
    config.contest_scoring = Some(context.clone());
    let manager = QsoManager::with_clock_for_test(config, clock.source());
    let id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    clock.advance(SignedDuration::from_mins(4) + SignedDuration::from_millis(123));
    let answered = clock.now();
    time_on_answer(&manager, false, OUR).await;
    for message in [
        MessageType::ReportAck {
            to_station: OUR.into(),
            from_station: DX.into(),
            report: -12,
        },
        MessageType::SeventyThree {
            to_station: OUR.into(),
            from_station: DX.into(),
        },
    ] {
        clock.advance(SignedDuration::from_secs(30));
        manager
            .process_message(message, format!("{OUR} {DX}"), FREQ, Some(-11.0))
            .await
            .unwrap();
    }
    let qso = manager.get_qso(id).await.unwrap();
    assert!(matches!(qso.state, QsoState::Completed { .. }));
    assert_eq!(qso.metadata.start_time, created);
    let db = crate::async_database::QsoDatabase::new_in_memory()
        .await
        .unwrap();
    db.insert_qso(&qso).await.unwrap();
    let indexed = db.contest_ledger(&context, OUR).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("qsos.adi");
    db.export_to_adif(&path).await.unwrap();
    let processor = crate::adif::AdifProcessor::new();
    let file = processor
        .parse_string(&std::fs::read_to_string(path).unwrap())
        .unwrap();
    let record = processor.record_to_qso(&file.records[0]).unwrap();
    let imported = processor.adif_to_qso(&record);
    let durable = ledger::fact(&imported, &context, OUR).unwrap();
    assert_eq!(indexed.len(), 1);
    assert_eq!(indexed[0].start, answered);
    assert_eq!(durable, indexed[0]);
    assert_eq!(
        ledger::decode(&record.additional_fields)
            .unwrap()
            .unwrap()
            .contact_start,
        Some(answered)
    );
}

#[tokio::test]
async fn responding_qso_time_on_unchanged() {
    let clock = PinnedClock::new();
    let manager = QsoManager::with_clock_for_test(test_config(), clock.source());
    let id = manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            None,
            CallInitiation::Manual,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    clock.advance(SignedDuration::from_secs(30));
    time_on_answer(&manager, true, OUR).await;
    let metadata = manager.get_qso(id).await.unwrap().metadata;
    assert_eq!(metadata.first_answer_at, None);
    assert_eq!(
        crate::adif::AdifProcessor::new()
            .qso_to_adif(&metadata, None)
            .qso_date,
        metadata.start_time
    );
}

#[tokio::test]
async fn creation_reads_the_injected_clock() {
    let clock = PinnedClock::new();
    let first = QsoManager::with_clock_for_test(test_config(), clock.source());
    let id = first
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let a = first.get_qso(id).await.unwrap().metadata;
    clock.advance(SignedDuration::from_secs(30));
    let second = QsoManager::with_clock_for_test(test_config(), clock.source());
    let id = second
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let b = second.get_qso(id).await.unwrap().metadata;
    assert_eq!((b.start_time - a.start_time).as_millis_i64(), 30_000);
    assert_eq!(a.tx_parity, b.tx_parity);
    assert_eq!(b.start_time, clock.now());
}

#[tokio::test]
async fn stall_module_is_replay_deterministic() {
    async fn replay() -> (Vec<String>, UtcTimestamp, Option<UtcTimestamp>) {
        let mut config = test_config();
        config.timeouts.qso_stall_switch_after = 2;
        let manager = manager_auto(config);
        let mut rx = manager.subscribe();
        let id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let opened = manager.get_qso(id).await.unwrap().metadata.start_time;
        stall_on_two_own_parity_slots(&manager, id, opened).await;
        confirm_last_frame_aired(&manager, id, FREQ).await;
        let events = drain(&mut rx);
        assert!(events
            .iter()
            .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })));
        // UUIDs identify independent contacts; compare all event payloads after normalizing that identity.
        let events = events
            .iter()
            .map(|e| format!("{e:?}").replace(&id.to_string(), "QSO"))
            .collect();
        let metadata = manager.get_qso(id).await.unwrap().metadata;
        (events, metadata.start_time, metadata.last_call_at)
    }
    assert_eq!(replay().await, replay().await);
}

const OUR: &str = "K5ARH";
const DX: &str = "K9ZZ";
const FREQ: f64 = 1500.0;

fn test_config() -> QsoManagerConfig {
    QsoManagerConfig {
        our_callsign: OUR.into(),
        our_grid: Some("EM12".into()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: default_active_mode(),
    }
}

/// Manager in Auto TX-freq mode (autonomous offset actions enabled).
fn manager_auto(config: QsoManagerConfig) -> QsoManager {
    let mut m = clocked_manager(config);
    m.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    )));
    m
}

fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

/// The origin a STALL-detected relocation commits with, built from the
/// QSO's live `advance_generation` so the staleness guard passes for the
/// reason it exists rather than by accident. The operator's `u` nudge uses
/// [`OffsetRelocationOrigin::OperatorForced`] directly.
async fn stall_origin(manager: &QsoManager, qso_id: QsoId) -> OffsetRelocationOrigin {
    OffsetRelocationOrigin::StallDetected {
        raised_at_generation: manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .advance_generation,
    }
}

/// PAN-182: report the QSO's latest keep-call frame aired at
/// `frequency_hz` — what the TX worker's `TxFrameSent` does — releasing a
/// stall switch held on it, as must happen before the drain can commit.
async fn confirm_last_frame_aired(manager: &QsoManager, qso_id: QsoId, frequency_hz: f64) {
    let text = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();
    assert!(
        manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &UnsentAttemptPayload {
                    frequency_hz,
                    remote_binding: Some((false, None)),
                },
                trigger_slot_played_out(manager, qso_id),
            )
            .await,
        "precondition: the stall switch was held on this frame"
    );
}

/// When the pending trigger's own TX slot has played out — the earliest
/// a `TxFrameSent` for it can carry — or far in the future when no
/// trigger is recorded.
fn trigger_slot_played_out(manager: &QsoManager, qso_id: QsoId) -> UtcTimestamp {
    manager
        .keep_call_ledger
        .lock()
        .unwrap()
        .get(&qso_id)
        .and_then(|entry| entry.pending_stall_trigger.as_ref())
        .and_then(|trigger| trigger.tx_slot)
        .map_or(manager.now_utc() + SignedDuration::from_hours(24), |slot| {
            slot + SignedDuration::from_secs(13)
        })
}

/// PAN-239: two silent rearms on two consecutive own-parity FT8 TX
/// slots, each 1 s in — inside the 1.7 s current-slot acceptance window
/// — so each feeds its own slot and PAN-128 counts both. Anchored to the
/// QSO's own slot grid rather than `opened_at`, whose phase is the real
/// wall clock's: the rearms resolve the same way on every run.
async fn stall_on_two_own_parity_slots(
    manager: &QsoManager,
    qso_id: QsoId,
    opened_at: UtcTimestamp,
) {
    let tx_parity = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .tx_parity
        .expect("manual QSOs latch a TX parity at creation");
    let own_slot = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), tx_parity);
    for k in [0i64, 2] {
        manager
            .rearm_manual_calls_at(own_slot + SignedDuration::from_secs(1 + 15 * k))
            .await;
    }
}

/// DX sends us a plain signal report — a genuine forward advance from
/// `RespondingToCq` to `SendingReport` (their_report: None rung).
async fn send_report(manager: &QsoManager, report: i8, text: &str) {
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report,
            },
            text.to_string(),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
}

/// Two silent rearm cycles (threshold = 2) on an Auto, tx_freq_mode=Auto
/// QSO must increment `stall_cycles` and, on the threshold-hitting cycle,
/// emit `TxOffsetActionNeeded::Switch` (no known-good offset recorded
/// yet, so we're "on" it vacuously).
///
/// Uses a Manual-initiated QSO (`respond_to_cq_manual`) so the rearm
/// loop's `manual_call_max_calls` bound (25, not the 2-call
/// `AUTO_RESEND_MAX_CALLS` cap for `CallInitiation::Auto` QSOs) doesn't
/// retire the QSO's resend eligibility before two rearm cycles complete
/// — "Auto" here refers to `TxFreqMode`, not `CallInitiation`, mirroring
/// the old (deleted) `stuck_dx_tests::manager_auto` +
/// `respond_to_cq_manual` combination.
#[tokio::test]
async fn silence_increments_stall_cycles_via_rearm_and_emits_switch_at_threshold() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // First rearm tick: one slot after the initial call, no DX response.
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "one silent rearm cycle must bump stall_cycles to 1"
    );
    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })),
        "no action expected before the threshold, got {events:?}"
    );

    // Second rearm tick, two slots after the first so it feeds a
    // DISTINCT own-parity TX slot (PAN-128): threshold (2) hit ->
    // Switch.
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(45))
        .await;

    let mut saw_switch = false;
    for event in drain(&mut rx) {
        if let QsoEvent::TxOffsetActionNeeded {
            qso_id: id,
            action: OffsetAction::Switch { avoid_hz },
            ..
        } = event
        {
            assert_eq!(id, qso_id);
            assert!(
                (avoid_hz - FREQ).abs() < f64::EPSILON,
                "Switch must avoid the current (held) offset"
            );
            saw_switch = true;
        }
    }
    assert!(
        saw_switch,
        "expected a Switch action after 2 silent rearm cycles"
    );
    // PAN-72 (Codex round 7 on PR #350, finding 2): raising the action is
    // NOT relocating. The streak survives emission and is cleared only by
    // a successful `apply_tx_offset_switch` (or a real DX advance) — see
    // `threshold_streak_survives_a_refused_relocation_and_clears_on_commit`
    // below for why that matters.
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "stall evidence must survive emission — the coordinator's commit \
         can still be refused, and the reset belongs there"
    );
}

/// PAN-72 (Codex round 7 on PR #350, finding 2): the threshold streak must
/// outlive a relocation that never happens, and die only when one does.
///
/// PAN-79 taught `apply_tx_offset_switch` to REFUSE a no-op relocation
/// (`OffsetActionNoOp`) precisely so a crowded band — every candidate
/// excluded, the allocator handing `avoid_hz` straight back — would not
/// clear valid stall evidence. But the threshold-tripping path had already
/// zeroed `stall_cycles` at emission time, one slot earlier, so there was
/// nothing left for that refusal to preserve: recovery was pushed out by
/// another full `qso_stall_switch_after` window even if a slot opened up
/// immediately afterward.
#[tokio::test]
async fn threshold_streak_survives_a_refused_relocation_and_clears_on_commit() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    for slot in [1i64, 3] {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * slot))
            .await;
    }
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "precondition: two silent cycles tripped the threshold"
    );

    // The coordinator's drain resolves the Switch, but every candidate is
    // excluded so the allocator returns the QSO's own offset unchanged —
    // exactly the input `apply_tx_offset_switch` refuses.
    let refused = manager
        .apply_tx_offset_switch(qso_id, FREQ, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(refused, Err(QsoManagerError::OffsetActionNoOp { .. })),
        "precondition: a same-offset relocation must be refused, got {refused:?}"
    );
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "a refused relocation must leave the accumulated stall evidence \
         intact — the QSO is still stalled and must be able to retry the \
         moment a slot frees up, not after another full threshold window"
    );

    // PAN-182: the drain commits a stall switch only once its trigger
    // frame (the slot-3 rearm's) has been reported aired.
    confirm_last_frame_aired(&manager, qso_id, FREQ).await;

    // A relocation that actually commits is the thing that clears it.
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("committing a real relocation");
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "a committed relocation is what resets the streak"
    );
}

/// PAN-75 (Codex review): the rearm that trips the threshold raises
/// its action before the TX worker can report the frame unsent. Rolling
/// that attempt back must make the queued action stale, or the drain
/// still moves the QSO on a frame that never aired.
#[tokio::test]
async fn rolling_back_the_threshold_attempt_revokes_its_queued_stall_action() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 1;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    let rearm_at = opened_at + SignedDuration::from_secs(15);
    manager.rearm_manual_calls_at(rearm_at).await;
    let raised_at_generation = drain(&mut rx)
        .into_iter()
        .find_map(|e| match e {
            QsoEvent::TxOffsetActionNeeded {
                raised_at_generation,
                ..
            } => Some(raised_at_generation),
            _ => None,
        })
        .expect("precondition: the threshold-tripping rearm raised an action");
    let text = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();

    assert!(matches!(
        manager
            .roll_back_unsent_attempt_at(qso_id, &text, rearm_at + SignedDuration::from_secs(1))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0
    );

    let result = manager
        .apply_tx_offset_switch(
            qso_id,
            FREQ + 300.0,
            OffsetRelocationOrigin::StallDetected {
                raised_at_generation,
            },
        )
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionStallRolledBack { .. })
        ),
        "a stall action whose threshold attempt was rolled back must be refused, got {result:?}"
    );
    assert!(result.unwrap_err().is_expected_offset_action_refusal());
    assert!(
        (manager.get_qso(qso_id).await.unwrap().metadata.frequency - FREQ).abs() < f64::EPSILON,
        "the QSO must stay on its offset"
    );

    // An operator nudge carries no stall evidence and still commits.
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("an operator nudge is not revoked by a stall rollback");
}

/// PAN-75 (Codex review on PR #418): an offset move followed by a
/// same-text rearm leaves two emissions that differ only in offset. A TX
/// pivot airs the fresher one in the older one's slot, so the rollback for
/// the replaced frame must undo the pre-move emission and keep the one
/// that aired, with its stall count and its timeline entry at the new
/// offset.
#[tokio::test]
async fn pivot_rollback_undoes_the_replaced_offset_not_the_frame_that_aired() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    let replaced_at = opened_at + SignedDuration::from_secs(15);
    manager.rearm_manual_calls_at(replaced_at).await;
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("precondition: the move commits");
    let aired_at = opened_at + SignedDuration::from_secs(30);
    manager.rearm_manual_calls_at(aired_at).await;
    let before = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(before.metadata.call_count, 3);
    assert_eq!(
        before.metadata.stall_cycles, 1,
        "precondition: one rearm since the move"
    );
    let text = before.messages.last().unwrap().raw_text.clone();

    let replaced = UnsentAttemptPayload {
        frequency_hz: FREQ,
        remote_binding: Some((false, None)),
    };
    assert_eq!(
        manager
            .roll_back_replaced_attempt_at(
                qso_id,
                &text,
                Some(&replaced),
                None,
                aired_at + SignedDuration::from_secs(1),
            )
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 2 }
    );
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.stall_cycles, 1,
        "the frame that aired after the move still counts"
    );
    assert_eq!(after.metadata.last_call_at, Some(aired_at));
    let sent: Vec<_> = after
        .messages
        .iter()
        .filter(|m| m.direction == MessageDirection::Sent)
        .collect();
    assert!(
        sent.iter().all(|m| m.timestamp != replaced_at),
        "the replaced emission's Sent entry is removed"
    );
    let last = sent.last().unwrap();
    assert_eq!(last.timestamp, aired_at);
    assert!((last.frequency - (FREQ + 300.0)).abs() < f64::EPSILON);

    // The opening call at FREQ has no ledger entry. A pivot that replaced
    // it must not fall back to the same-text frame that aired.
    assert_eq!(
        manager
            .roll_back_replaced_attempt_at(
                qso_id,
                &text,
                Some(&replaced),
                None,
                aired_at + SignedDuration::from_secs(2),
            )
            .await,
        UnsentAttemptRollback::NoMatchingAttempt
    );
    let unchanged = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(unchanged.metadata.call_count, 2);
    assert_eq!(unchanged.metadata.stall_cycles, 1);
    assert_eq!(unchanged.metadata.last_call_at, Some(aired_at));
}

/// A REMOTE-origin QSO whose keep-calling frames the station-agent
/// armed-TX gate is dropping (PAN-72; Codex round 7 on PR #350, finding 3).
///
/// `remote_origin` QSOs emit every frame as `TxOrigin::Remote`, and the TX
/// worker checks `remote_tx_permitted` (the `ArmState` gate) immediately
/// before PTT — an INDEPENDENT gate from `TxPolicy`. An arm expiry or an
/// explicit remote disarm leaves the policy at `Full`, so round 2's mute
/// check sees nothing and the rearm cycles counted as DX silence, moving
/// the QSO off a known-good offset on the strength of frames that never
/// left the rig.
async fn remote_qso_with_arm(manager: &QsoManager) -> (QsoId, Arc<std::sync::atomic::AtomicBool>) {
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let qso_id = manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            None,
            CallInitiation::Manual,
            None,
            true, // remote_origin
            None,
        )
        .await
        .unwrap();
    (qso_id, armed)
}

/// Like [`remote_qso_with_arm`], but the QSO is bound to an explicit
/// client keyId (PAN-91 review follow-up: exercises the identity-bound
/// stall-evidence predicate, not just the boolean one).
async fn remote_qso_bound_to(manager: &QsoManager, client_key_id: &str) -> QsoId {
    manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            None,
            CallInitiation::Manual,
            None,
            true, // remote_origin
            Some(client_key_id.to_string()),
        )
        .await
        .unwrap()
}

/// PAN-91 review follow-up: a QSO bound to client-a must not count a
/// rearm cycle as reaching the air just because client-b is currently
/// armed — the stall-evidence predicate must check the SAME client
/// identity the TX worker's actual gate binds to, not just "is anyone
/// armed".
#[tokio::test]
async fn a_qso_bound_to_a_different_client_than_the_one_armed_does_not_count_as_reaching_the_air() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let mut manager = manager_auto(config);
    let qso_id = remote_qso_bound_to(&manager, "client-a").await;
    // Simulate the real gate: only "client-b" is armed.
    manager.set_remote_tx_permitted_source(Arc::new(|client_key_id: Option<&str>| {
        client_key_id == Some("client-b")
    }));
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    for slot in 1..=4 {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * slot))
            .await;
    }

    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "client-a's frame is denied by the identity-bound gate even \
         though client-b is armed — it never reaches the air, so this \
         must not count as stall evidence"
    );
}

#[tokio::test]
async fn a_disarmed_remote_qso_does_not_count_rearm_cycles_as_stalls() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let mut manager = manager_auto(config);
    // TX policy stays FULL throughout — this is precisely the case round
    // 2's `tx_policy` check cannot see.
    let (qso_id, armed) = remote_qso_with_arm(&manager).await;
    let armed_for_source = Arc::clone(&armed);
    manager.set_remote_tx_permitted_source(Arc::new(move |_| {
        armed_for_source.load(std::sync::atomic::Ordering::Relaxed)
    }));
    let mut rx = manager.subscribe();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // The arm expires (or the remote operator disarms) before the first
    // rearm slot. Four cycles — twice the switch threshold.
    armed.store(false, std::sync::atomic::Ordering::Relaxed);
    for slot in 1..=4 {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * slot))
            .await;
    }

    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "a rearmed frame the armed-TX gate dropped never went on the air, \
         so the DX's silence is not evidence of a stall"
    );
    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })),
        "no offset action may be raised on the strength of silence the \
         local arm gate caused, got {events:?}"
    );
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.frequency,
        FREQ,
        "the QSO must still be on its original offset"
    );

    // Re-arming resumes normal stall accounting from where it left off.
    armed.store(true, std::sync::atomic::Ordering::Relaxed);
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * 5))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "the first genuinely transmitted cycle after the re-arm counts"
    );
}

/// Control for the test above: a LOCAL QSO must never consult the
/// armed-TX predicate. The gate only applies to `TxOrigin::Remote`
/// frames, so a disarmed station still transmits (and still stalls)
/// normally on local/autonomous QSOs — if this regressed, a disarmed
/// remote session would silently freeze local adaptive recovery.
#[tokio::test]
async fn a_disarmed_arm_gate_does_not_suppress_local_qso_stall_counting() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let mut manager = manager_auto(config);
    manager.set_remote_tx_permitted_source(Arc::new(|_| false));
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;

    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "a LOCAL QSO's frames are never armed-TX gated, so the arm state \
         must not affect its stall accounting"
    );
}

/// PAN-72 Fix E (Codex round 10, thread on `qso_manager.rs:6335`): a
/// Hamlib-hard-muted rearm (a Hamlib restart, the command loop not yet
/// ready, an undelivered pending frequency/split command, or an
/// in-flight Hamlib command -- `pancetta::coordinator::tx::
/// tx_hard_mute_reason`) never reaches the air either, exactly like the
/// `TxPolicy`/remote-arm cases above, and must not be counted as stall
/// evidence.
#[tokio::test]
async fn a_hamlib_hard_muted_rearm_does_not_count_as_a_stall_cycle() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let mut manager = manager_auto(config);
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let muted_for_source = Arc::clone(&muted);
    manager.set_hamlib_hard_muted_source(Arc::new(move || {
        muted_for_source.load(std::sync::atomic::Ordering::Relaxed)
    }));
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // Four muted cycles -- twice the switch threshold -- must not tick
    // stall_cycles at all.
    for slot in 1..=4 {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * slot))
            .await;
    }
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "a rearmed frame the Hamlib hard-mute dropped never went on the \
         air, so the DX's silence is not evidence of a stall"
    );
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.frequency,
        FREQ,
        "the QSO must still be on its original offset"
    );

    // Once the mute clears, stall accounting resumes from zero.
    muted.store(false, std::sync::atomic::Ordering::Relaxed);
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * 5))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "the first genuinely transmitted cycle after the mute clears counts"
    );
}

/// The never-injected default (`Arc::new(|| false)`, i.e. "not muted")
/// must leave every existing stall_cycles-related test passing
/// unmodified -- the FT8/no-Hamlib-wiring-byte-identical check for this
/// fix specifically.
#[tokio::test]
async fn the_default_hamlib_hard_muted_source_never_suppresses_stall_counting() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config); // no set_hamlib_hard_muted_source call
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;

    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "with no Hamlib-mute source injected, stall counting must behave \
         exactly as before this fix"
    );
}

/// PAN-72 (Codex round 7 on PR #350, finding 5): the rearm/stall cadence
/// is one slot of the ACTIVE MODE, not a hardcoded 15 s.
///
/// `qso_stall_switch_after` is documented in slots. Under FT4's 7.5 s slot
/// the old gate advanced the counter only every OTHER real slot, so a
/// threshold of 2 took ~30 s (4 slots) instead of 2 — long enough for the
/// ordinary QSO watchdog to win first.
#[tokio::test]
async fn ft4_counts_stalls_on_its_own_75_second_slot_cadence() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    config.active_mode = "FT4".to_string();
    let mut manager = manager_auto(config);
    // Round-7 re-review: the cadence comes from the coordinator's shared
    // `active_slot_ns` atomic, not the `active_mode` string above (which
    // only stamps `QsoMetadata::mode`). This is FT4's slot in ns.
    manager.set_active_slot_ns_source(Arc::new(std::sync::atomic::AtomicI64::new(7_500_000_000)));
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // Two rearms two FT4 slots (2 × 7.5 s) apart, each feeding a
    // distinct own-parity FT4 TX slot (PAN-128).
    for slot in [1i64, 3] {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_millis(7_500 * slot))
            .await;
    }

    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "two FT4 slots must count as two stall cycles — the old 15 s gate \
         counted zero"
    );
}

/// FT8 stays byte-identical (AGENTS.md invariant): the same two 15 s
/// cycles count exactly as before, and a sub-slot rearm still does not.
#[tokio::test]
async fn ft8_slot_cadence_is_unchanged_at_fifteen_seconds() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    assert_eq!(
        config.active_mode, "FT8",
        "precondition: the default active mode is FT8"
    );
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // 14.999 s: still inside the slot, must not count.
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_millis(14_999))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "a sub-slot rearm must not count under FT8"
    );

    // Exactly one slot: counts.
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1
    );
}

/// The ns → ms conversion must agree with `pancetta_ft8`'s
/// `ProtocolParams::{ft8,ft4,ft2}().slot_ns()` (`cycle_duration * 1e9`),
/// spelled as literals only because `pancetta-qso` does not depend on that
/// crate outside the optional `sim` feature.
#[test]
fn slot_millis_match_the_ft8_crates_protocol_params() {
    assert_eq!(rearm_slot_millis_from_ns(15_000_000_000), 15_000);
    assert_eq!(rearm_slot_millis_from_ns(7_500_000_000), 7_500);
    // FT2's 3.2 s: `(3.2f64 * 1e9) as i64` is exactly 3_200_000_000, so
    // the truncating divide is exact too (no 3_199 ms surprise).
    assert_eq!(rearm_slot_millis_from_ns(3_200_000_000), 3_200);
    // Defensive: a nonsensical / sub-millisecond value falls back to the
    // FT8 slot — never to a shorter one.
    assert_eq!(rearm_slot_millis_from_ns(0), 15_000);
    assert_eq!(rearm_slot_millis_from_ns(-1), 15_000);
    assert_eq!(rearm_slot_millis_from_ns(999_999), 15_000);
}

/// The never-injected default is FT8's 15 s slot, so every existing caller
/// (unit tests, the sim harness, any embedder that skips the setter) keeps
/// byte-identical FT8 behavior.
#[test]
fn the_default_slot_source_is_exactly_the_ft8_slot() {
    let manager = clocked_manager(test_config());
    assert_eq!(manager.rearm_slot_millis(), 15_000);
}

/// PAN-72 round-8 redesign (fix 1): `PRE_SWITCH_OFFSET_GRACE` used to be a
/// hardcoded 30 s literal that only matched FT8's 15 s slot by
/// coincidence. It must instead be derived from the SAME `active_slot_ns`
/// atomic `rearm_slot_millis_from_ns` already converts, as two slot
/// durations, for every protocol.
#[test]
fn pre_switch_offset_grace_scales_with_the_active_slot() {
    assert_eq!(
        pre_switch_offset_grace_from_slot_ns(15_000_000_000),
        SignedDuration::from_millis(30_000)
    );
    assert_eq!(
        pre_switch_offset_grace_from_slot_ns(7_500_000_000),
        SignedDuration::from_millis(15_000)
    );
    assert_eq!(
        pre_switch_offset_grace_from_slot_ns(3_200_000_000),
        SignedDuration::from_millis(6_400)
    );
}

/// The never-injected default (FT8's 15 s slot) must still produce
/// today's 30 s grace, so every existing caller keeps byte-identical
/// behavior.
#[test]
fn the_default_pre_switch_offset_grace_is_unchanged() {
    let manager = clocked_manager(test_config());
    assert_eq!(
        manager.pre_switch_offset_grace(),
        SignedDuration::from_millis(30_000)
    );
}

/// Injecting the coordinator's `active_slot_ns` atomic (FT4's 7.5 s in ns)
/// drives the rearm/stall cadence — and does so WITHOUT touching
/// `config.active_mode`, proving the atomic, not the cloned config, is the
/// source of truth.
#[tokio::test]
async fn injected_active_slot_ns_drives_the_rearm_cadence() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    assert_eq!(
        config.active_mode, "FT8",
        "precondition: the cloned config still says FT8"
    );
    let mut manager = manager_auto(config);
    manager.set_active_slot_ns_source(Arc::new(std::sync::atomic::AtomicI64::new(7_500_000_000)));

    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    for slot in [1i64, 3] {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_millis(7_500 * slot))
            .await;
    }

    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "the injected FT4 slot atomic must set the cadence even though the \
         config string still reads FT8"
    );
}

/// The whole point of using an `Arc` atomic rather than a config field: a
/// mode switch that happens AFTER `start()` took its clone is observed by
/// the already-running rearm loop, in BOTH directions.
#[tokio::test]
async fn a_live_slot_change_after_the_clone_is_observed_by_the_rearm_loop() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 4;
    let mut manager = manager_auto(config);
    let slot_ns = Arc::new(std::sync::atomic::AtomicI64::new(15_000_000_000));
    manager.set_active_slot_ns_source(Arc::clone(&slot_ns));
    // Exactly what `start()` does: the rearm/stall loop runs on a CLONE.
    let rearm_loop = manager.clone();

    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // FT8 → FT4 while the loop is already running.
    slot_ns.store(7_500_000_000, std::sync::atomic::Ordering::Relaxed);
    rearm_loop
        .rearm_manual_calls_at(opened_at + SignedDuration::from_millis(7_500))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "the running loop must pick up the live switch INTO FT4"
    );

    // FT4 → FT8 while the loop is already running: another 7.5 s is now a
    // sub-slot rearm and must not count.
    slot_ns.store(15_000_000_000, std::sync::atomic::Ordering::Relaxed);
    rearm_loop
        .rearm_manual_calls_at(opened_at + SignedDuration::from_millis(15_000))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "the running loop must pick up the live switch BACK to FT8"
    );

    // A full FT8 slot after the last counted call does count again.
    // 37.5 s (not one FT8 slot sharp) so the own-parity FT8 slot this
    // keys can never collide with the slot the FT4 rearm booked: a
    // mode switch changes the slot grid, and PAN-128's ledger dedups
    // by slot key, so an exactly-one-slot rearm can land on the same
    // key across the switch for some wall-clock phases.
    rearm_loop
        .rearm_manual_calls_at(opened_at + SignedDuration::from_millis(37_500))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2
    );
}

/// PAN-72 (PR #350 round 7 re-review): the REGRESSION the shared atomic
/// fixes. `QsoManager::start()` spawns `timeout_check_loop` on a `clone()`,
/// and `Clone` copies `config` BY VALUE. The coordinator's Shift+M handler
/// (`QsoMessage::SetOperatingMode` → `set_active_mode`) mutates only its
/// OWN binding, so a runtime FT4 → FT8 switch left the running rearm loop's
/// cloned `config.active_mode` stuck at `"FT4"` and it kept counting stalls
/// on a 7.5 s cadence for a QSO that was by then genuinely FT8 — a real
/// deviation from FT8's historical behavior (AGENTS.md's byte-identity
/// invariant).
#[tokio::test]
async fn a_runtime_switch_back_to_ft8_restores_the_fifteen_second_cadence() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    config.active_mode = "FT4".to_string();
    let mut manager = manager_auto(config);
    // Exactly what `start()` does: the rearm/stall loop runs on a CLONE.
    let rearm_loop = manager.clone();
    // Operator presses Shift+M, FT4 → FT8. The coordinator calls this on
    // its own binding only.
    manager.set_active_mode("FT8".to_string());

    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // One FT4 slot after the opening call. Under FT8 (the mode we are now
    // actually operating) this is a SUB-slot rearm and must not count.
    rearm_loop
        .rearm_manual_calls_at(opened_at + SignedDuration::from_millis(7_500))
        .await;

    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "after a runtime switch to FT8 the running rearm loop must use the \
         15 s FT8 slot, not the stale cloned config's 7.5 s FT4 slot"
    );
}

/// PAN-72 (Codex round 2 on PR #350, finding 5): while `TxPolicy` is
/// `Disabled` the coordinator's hard TX mute blocks every frame this loop
/// re-emits. The keep-calling loop still re-arms each slot, but nothing
/// goes on the air — so the DX's continued silence is evidence of nothing,
/// and counting it used to walk an established QSO off its known-good
/// offset after four muted cycles without a single transmission.
#[tokio::test]
async fn a_muted_tx_path_does_not_count_rearm_cycles_as_stalls() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let mut manager = manager_auto(config);
    let policy = Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxPolicy::Disabled.as_u8(),
    ));
    manager.set_tx_policy_source(Arc::clone(&policy));
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // Four muted rearm cycles — twice the switch threshold.
    for slot in 1..=4 {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * slot))
            .await;
    }

    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "a rearm cycle the TX mute swallowed must not count as a silent \
         on-air cycle"
    );
    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })),
        "no offset action may be raised while TX is muted, got {events:?}"
    );
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.frequency,
        FREQ,
        "the QSO must still be on its original offset"
    );

    // Re-enabling TX resumes normal stall accounting from where it was.
    policy.store(
        pancetta_core::TxPolicy::Full.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * 5))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "the first genuinely transmitted cycle after the mute lifts counts"
    );
}

/// A genuine forward advance resets `stall_cycles` to 0 and records the
/// QSO's current offset as `last_known_good_offset_hz`.
#[tokio::test]
async fn forward_advance_resets_stall_cycles_and_records_known_good_offset() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // One silent rearm cycle: stall_cycles -> 1 (below threshold).
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1
    );

    // DX genuinely advances the QSO: RespondingToCq -> SendingReport.
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.stall_cycles, 0,
        "forward advance must reset stall_cycles"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(after.metadata.frequency),
        "forward advance must record the current offset as known-good"
    );
}

/// A QSO that stalls a second time on an offset it was already switched
/// to (i.e. currently NOT on its recorded known-good offset) must emit
/// `Revert { target_hz }` back to the known-good offset, not another
/// `Switch`.
#[tokio::test]
async fn second_stall_on_switched_offset_reverts_to_known_good() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();

    // Advance once to establish last_known_good_offset_hz = Some(FREQ).
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    let after_advance = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after_advance.metadata.stall_cycles, 0);
    assert_eq!(after_advance.metadata.last_known_good_offset_hz, Some(FREQ));
    let entered_at = after_advance.metadata.last_call_at.unwrap();
    let _ = drain(&mut rx);

    // Two silent rearm cycles (two slots apart, so each feeds a
    // distinct own-parity TX slot — PAN-128) on the (still known-good)
    // offset -> Switch.
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(15))
        .await;
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(45))
        .await;
    let first_action = drain(&mut rx).into_iter().find_map(|e| match e {
        QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
        _ => None,
    });
    assert_eq!(
        first_action,
        Some(OffsetAction::Switch { avoid_hz: FREQ }),
        "first stall (still on known-good offset) must Switch"
    );

    // Resolve the Switch the way the coordinator's drain really does —
    // through `apply_tx_offset_switch`, the sole external mutator — so
    // the ping-pong below is driven by the production commit path rather
    // than a hand-poked metadata field. `FREQ + 300` is comfortably
    // inside TX_OFFSET_MIN_HZ..=TX_OFFSET_MAX_HZ, so the clamp is a no-op
    // here.
    let new_freq = FREQ + 300.0;
    let applied = manager
        .apply_tx_offset_switch(qso_id, new_freq, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("committing the resolved Switch");
    assert_eq!(applied, new_freq);

    // Two more silent rearm cycles (again two slots apart) on the NEW
    // (non-known-good) offset -> Revert back to the known-good offset,
    // not another Switch.
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(75))
        .await;
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(105))
        .await;
    let second_action = drain(&mut rx).into_iter().find_map(|e| match e {
        QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
        _ => None,
    });
    assert_eq!(
        second_action,
        Some(OffsetAction::Revert { target_hz: FREQ }),
        "second stall (now off the known-good offset) must Revert, not Switch"
    );
}

/// PAN-72 (Codex round 1 on PR #350, finding 2): the CQer's real
/// `CallingCq -> WaitingForReport` advance — a caller finally answering
/// our CQ — must reset `stall_cycles` and record the offset as
/// known-good, exactly like the Caller flow's
/// `RespondingToCq -> SendingReport`.
///
/// Before the fix `dx_frame_advanced` was computed from `ladder_rank`,
/// which ranks neither `CallingCq` nor `WaitingForReport`, so the streak
/// accumulated over unanswered CQs carried straight into the established
/// exchange.
#[tokio::test]
async fn cqer_flow_advance_resets_stall_cycles_and_records_known_good_offset() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 4;
    let manager = manager_auto(config);
    let qso_id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // Three unanswered CQ slots (rearms two slots apart, so each feeds
    // a distinct own-parity TX slot — PAN-128): stall_cycles -> 3
    // (still below the default-shaped threshold of 4).
    for i in [1i64, 3, 5] {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * i))
            .await;
    }
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        3,
        "precondition: three unanswered CQ slots accumulated a streak"
    );

    // A caller finally answers: CallingCq -> WaitingForReport. This is a
    // genuine forward advance and must clear the accumulated streak.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.into(),
                responding_station: DX.into(),
                grid: Some("FN31".into()),
            },
            format!("{OUR} {DX} FN31"),
            FREQ,
            Some(-9.0),
        )
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(after.state, QsoState::WaitingForReport { .. }),
        "precondition: the answer must advance the CQer to WaitingForReport, got {:?}",
        after.state
    );
    assert_eq!(
        after.metadata.stall_cycles, 0,
        "a caller answering our CQ is forward progress — the unanswered-CQ \
         streak must not carry into the established exchange"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(after.metadata.frequency),
        "the offset the caller demonstrably heard us on must be recorded \
         as known-good so a later stall Reverts here instead of Switching blind"
    );
}

/// PAN-72 (Codex round 1 on PR #350, finding 5): after the Hound's
/// procedure-mandated QSY into the response region, the POST-QSY offset —
/// not the pre-QSY low calling offset — must be the recorded known-good.
///
/// The QSY is driven by the very same `RespondingToCq -> SendingReport`
/// advance that records known-good, so without the re-anchor a later
/// `SendingReport` stall would read "we are off our known-good offset"
/// and emit `Revert` straight back to the low calling offset, undoing the
/// QSY and putting our R+report where the Fox is no longer listening.
#[tokio::test]
async fn hound_qsy_reanchors_the_known_good_offset_so_a_stall_never_reverts_it() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();

    // A Hound QSO calls the Fox low (calling region) with the Fox's own
    // frequency latched as `partner_freq`.
    let fox_rx_hz = 300.0;
    let qso_id = manager
        .engage_hound(DX, fox_rx_hz, None, None)
        .await
        .unwrap();
    let before = manager.get_qso(qso_id).await.unwrap();
    let calling_offset = before.metadata.frequency;
    assert!(
        calling_offset <= config_call_max(),
        "precondition: the Hound calls inside the low calling region, got {calling_offset}"
    );

    // The Fox answers with a signal report: RespondingToCq ->
    // SendingReport, which triggers the one-shot QSY.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -12,
            },
            format!("{OUR} {DX} -12"),
            fox_rx_hz,
            Some(-12.0),
        )
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert!(
        after.metadata.hound_qsyed,
        "precondition: the Fox's report must trigger the Hound QSY"
    );
    let qsyed = after.metadata.frequency;
    assert!(
        (qsyed - calling_offset).abs() > f64::EPSILON,
        "precondition: the QSY must actually move us out of the calling region"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(qsyed),
        "the known-good anchor must follow the QSY into the response region"
    );

    // Now the Fox goes silent. Two stalled `SendingReport` rearm cycles
    // (two slots apart, feeding distinct own-parity TX slots — PAN-128)
    // must produce a Switch (we're still on our known-good offset), NOT
    // a Revert to the abandoned low calling offset.
    let entered_at = after.metadata.last_call_at.unwrap();
    let _ = drain(&mut rx);
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(15))
        .await;
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(45))
        .await;
    let action = drain(&mut rx).into_iter().find_map(|e| match e {
        QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
        _ => None,
    });
    assert_eq!(
        action,
        Some(OffsetAction::Switch { avoid_hz: qsyed }),
        "a stalled Hound must search within the response region, never \
         Revert to the pre-QSY calling offset"
    );
}

/// The default Hound calling-region ceiling, for the precondition above.
fn config_call_max() -> f64 {
    HoundRegions::default().call_max_hz
}

/// PAN-72 (Codex round 4 on PR #350, finding 2): the Hound-region
/// constraint must be revalidated INSIDE the commit, not merely used to
/// steer the coordinator's earlier resolution.
///
/// The drain reads the QSO (and therefore its Hound phase) with `get_qso`,
/// resolves against that snapshot, and only then awaits
/// `apply_tx_offset_switch`. For an operator-forced `u` nudge there is
/// deliberately no `raised_at_generation`, so the advance guard cannot
/// catch a QSY that lands in that window either. This test drives exactly
/// that sequence: resolve a calling-region offset, let the Fox's report
/// perform the QSY, then commit the now-stale offset.
#[tokio::test]
async fn a_hound_qsy_across_the_commit_window_refuses_the_stale_calling_offset() {
    let manager = manager_auto(test_config());
    let fox_rx_hz = 300.0;
    let qso_id = manager
        .engage_hound(DX, fox_rx_hz, None, None)
        .await
        .unwrap();

    // What the coordinator's pre-resolution `get_qso` would have seen: a
    // pre-QSY Hound, pinned to the low calling region. Pick a different
    // in-region offset — a legitimate calling-region relocation.
    let before = manager.get_qso(qso_id).await.unwrap();
    assert!(!before.metadata.hound_qsyed, "precondition: pre-QSY");
    let regions = HoundRegions::default();
    let resolved_hz = (before.metadata.frequency + 100.0).min(regions.call_max_hz);
    assert!(
        (resolved_hz - before.metadata.frequency).abs() > TX_OFFSET_NOOP_TOLERANCE_HZ
            && resolved_hz >= regions.call_min_hz
            && resolved_hz <= regions.call_max_hz,
        "precondition: the resolved offset is a real move inside the calling region"
    );

    // ...and now the Fox answers, before the awaited commit runs: the
    // mandatory calling-region -> response-region QSY fires.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -12,
            },
            format!("{OUR} {DX} -12"),
            fox_rx_hz,
            Some(-12.0),
        )
        .await
        .unwrap();
    let qsyed_hz = manager.get_qso(qso_id).await.unwrap().metadata.frequency;
    assert!(
        qsyed_hz >= regions.response_min_hz,
        "precondition: the QSY moved us into the response region, got {qsyed_hz}"
    );

    // Operator-forced (no generation token), exactly as the `u` nudge is.
    let err = manager
        .apply_tx_offset_switch(qso_id, resolved_hz, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect_err("a calling-region offset must not commit onto a QSY'd Hound");
    assert!(
        matches!(
            err,
            QsoManagerError::OffsetActionOutsideHoundRegion { qso_id: id, .. } if id == qso_id
        ),
        "expected OffsetActionOutsideHoundRegion, got {err:?}"
    );
    assert!(
        err.is_expected_offset_action_refusal(),
        "a QSY racing the commit is an ordinary race, not a fault"
    );
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.frequency,
        qsyed_hz,
        "the refused action must leave the procedure-mandated QSY intact"
    );
}

/// PAN-259: after the Fox's report the Hound is pinned to the Fox's own
/// offset. Any relocation away from it, even one inside the response
/// envelope, is refused, and the pinned window is a single point.
#[tokio::test]
async fn a_hound_switch_after_the_fox_report_is_pinned_to_the_fox_offset() {
    let manager = manager_auto(test_config());
    let fox_rx_hz = 600.0;
    let qso_id = manager
        .engage_hound(DX, fox_rx_hz, None, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -12,
            },
            format!("{OUR} {DX} -12"),
            fox_rx_hz,
            Some(-12.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.frequency, 600.0);
    assert_eq!(
        hound_switch_range_hz(&progress, manager.config()),
        Some((600.0, 600.0)),
        "the post-report window is the Fox offset alone"
    );

    let target = 800.0;
    let err = manager
        .apply_tx_offset_switch(qso_id, target, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect_err("a nudge off the Fox offset must not commit");
    assert!(
        matches!(
            err,
            QsoManagerError::OffsetActionOutsideHoundRegion { qso_id: id, .. } if id == qso_id
        ),
        "expected OffsetActionOutsideHoundRegion, got {err:?}"
    );
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.frequency,
        600.0,
        "the refused nudge must leave the Hound on the Fox offset"
    );
}

/// PAN-72 (Codex round 1 on PR #350, finding 3): a queued action drained
/// after the QSO completed must mutate nothing.
///
/// Completed entries stay in the map (and stay in the coordinator's
/// active snapshots for a 45-second trailing-73 grace window), so the
/// lookup still succeeds. `QsoState::set_frequency` refuses terminal
/// states, but `metadata.frequency` had no such guard — the write went
/// through, the state kept the real logged offset, and the drain then
/// mirrored the phantom offset into `active_tx_offsets` while the queued
/// final frame still went out on the original one.
#[tokio::test]
async fn apply_tx_offset_switch_refuses_a_terminal_qso_without_mutating_it() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    // The DX closes early with 73: SendingReport -> Completed.
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{OUR} {DX} 73"),
            FREQ,
            Some(-7.0),
        )
        .await
        .unwrap();

    let before = manager.get_qso(qso_id).await.unwrap();
    assert!(
        before.state.is_terminal(),
        "precondition: the QSO must be terminal, got {:?}",
        before.state
    );

    let err = manager
        .apply_tx_offset_switch(qso_id, FREQ + 400.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect_err("a terminal QSO must refuse an offset action");
    assert!(
        matches!(err, QsoManagerError::QsoNotActive { qso_id: id } if id == qso_id),
        "expected QsoNotActive, got {err:?}"
    );
    assert!(
        err.is_expected_offset_action_refusal(),
        "a post-completion action is an expected race, not a fault"
    );

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, before.metadata.frequency,
        "metadata.frequency must not be rewritten on a terminal QSO — the \
         coordinator mirrors this value into active_tx_offsets"
    );
    assert_eq!(
        after.state.frequency(),
        before.state.frequency(),
        "the logged offset is the historical record and must not move"
    );
}

/// PAN-72 (Codex round 1 on PR #350, finding 8): the coordinator drains
/// its request mailbox only once per 15-second slot, so the DX can send
/// an advancing frame between an action being raised and committed. The
/// advance already reset `stall_cycles` and marked the CURRENT offset
/// known-good, so applying the stale request would move the QSO off the
/// offset that just demonstrably worked.
#[tokio::test]
async fn apply_tx_offset_switch_discards_a_request_the_qso_has_advanced_past() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let raised_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .advance_generation;
    assert_eq!(raised_at, 0, "a fresh QSO starts at generation 0");

    // The DX comes back before the once-per-slot drain runs.
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    let advanced = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        advanced.metadata.advance_generation,
        raised_at + 1,
        "a forward advance must bump the staleness token"
    );
    assert_eq!(
        advanced.metadata.last_known_good_offset_hz,
        Some(FREQ),
        "precondition: the current offset is now the known-good one"
    );

    let err = manager
        .apply_tx_offset_switch(
            qso_id,
            FREQ + 400.0,
            OffsetRelocationOrigin::StallDetected {
                raised_at_generation: raised_at,
            },
        )
        .await
        .expect_err("a superseded request must be discarded");
    assert!(
        matches!(
            err,
            QsoManagerError::OffsetActionStale { qso_id: id, raised_at: r, current: c }
                if id == qso_id && r == raised_at && c == raised_at + 1
        ),
        "expected OffsetActionStale, got {err:?}"
    );
    assert!(err.is_expected_offset_action_refusal());
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.frequency,
        FREQ,
        "the QSO must stay on the offset the DX just answered on"
    );

    // A request raised at the CURRENT generation still commits normally,
    // and an operator-forced nudge (`None`) is never staleness-checked.
    let applied = manager
        .apply_tx_offset_switch(
            qso_id,
            FREQ + 400.0,
            OffsetRelocationOrigin::StallDetected {
                raised_at_generation: raised_at + 1,
            },
        )
        .await
        .expect("a current-generation request must still commit");
    assert_eq!(applied, FREQ + 400.0);
    let forced = manager
        .apply_tx_offset_switch(qso_id, FREQ + 500.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("an operator-forced nudge is never stale");
    assert_eq!(forced, FREQ + 500.0);
}

/// PAN-131 (Codex round 13 on PR #350, P2): the operator pressed `u` while
/// the QSO was rearm-eligible, then the DX's R-report moved it to
/// `WaitingForConfirmation` before the once-per-slot drain committed the
/// nudge. Nothing retransmits from that state, so committing would move
/// the offset with no frame ever sent on it. The nudge carries no
/// generation, so guard 2 cannot see the advance; guard 2a must refuse it.
#[tokio::test]
async fn an_operator_nudge_is_superseded_when_the_dx_advances_to_waiting_for_confirmation() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    // The DX skips its plain report and sends R-07 before the drain runs.
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{OUR} {DX} R-07"),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    let before = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(before.state, QsoState::WaitingForConfirmation { .. }),
        "precondition: the R-report must advance the QSO to \
         WaitingForConfirmation, got {:?}",
        before.state
    );
    let mut rx = manager.subscribe();
    let _ = drain(&mut rx);

    let err = manager
        .apply_tx_offset_switch(qso_id, FREQ + 400.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect_err("a nudge the DX's progress superseded must not commit");
    assert!(
        matches!(err, QsoManagerError::OffsetActionNudgeSuperseded { qso_id: id } if id == qso_id),
        "expected OffsetActionNudgeSuperseded, got {err:?}"
    );
    assert!(
        err.is_expected_offset_action_refusal(),
        "DX progress overtaking a queued nudge is an expected race, not a fault"
    );

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, before.metadata.frequency,
        "the QSO must stay on the offset it is actually transmitting on"
    );
    assert_eq!(after.state.frequency(), before.state.frequency());
    assert_eq!(after.metadata.partner_freq, before.metadata.partner_freq);
    let events = drain(&mut rx);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            QsoEvent::TxOffsetApplied { .. } | QsoEvent::MessageToSend { .. }
        )),
        "a refused nudge must neither announce a move nor send a frame, got {events:?}"
    );
}

/// PAN-131: the second path into the same gap. An autonomous self-CQ is
/// nudge-selectable while it is `CallingCq` (`auto_calling_cq_ids`), and
/// a caller answering moves it to `WaitingForReport` with
/// `CallInitiation::Auto` kept. `cq_retransmit` only re-sends `CallingCq`,
/// and `rearm_manual_calls_at` only re-sends a Manual `WaitingForReport`,
/// so a nudge committed here would send nothing at the new offset.
#[tokio::test]
async fn an_operator_nudge_is_superseded_when_an_auto_self_cq_is_answered() {
    let manager = manager_auto(test_config());
    let qso_id = manager.start_cq(FREQ, None, false, None).await.unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.into(),
                responding_station: DX.into(),
                grid: Some("FN42".into()),
            },
            format!("{OUR} {DX} FN42"),
            FREQ,
            Some(-9.0),
        )
        .await
        .unwrap();
    let before = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(before.state, QsoState::WaitingForReport { .. }),
        "precondition: the answer must advance the self-CQ to \
         WaitingForReport, got {:?}",
        before.state
    );
    assert_eq!(
        before.metadata.initiated_by,
        CallInitiation::Auto,
        "precondition: the answered self-CQ stays autonomous"
    );

    let err = manager
        .apply_tx_offset_switch(qso_id, FREQ + 400.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect_err("nothing retransmits an Auto WaitingForReport, so the nudge must not commit");
    assert!(
        matches!(err, QsoManagerError::OffsetActionNudgeSuperseded { qso_id: id } if id == qso_id),
        "expected OffsetActionNudgeSuperseded, got {err:?}"
    );
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after.metadata.frequency, before.metadata.frequency);
    assert_eq!(after.state.frequency(), before.state.frequency());
}

/// PAN-131, the control case: an advance that keeps the QSO inside the
/// nudge-eligible states must NOT supersede the nudge. `RespondingToCq` →
/// `SendingReport` bumps `advance_generation` exactly like the advance
/// above, which is why the fix revalidates the state at commit time rather
/// than giving the nudge a generation token — a token would refuse this
/// nudge too, though the next rearm sends at the new offset.
#[tokio::test]
async fn an_operator_nudge_survives_an_advance_that_stays_retransmittable() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    let advanced = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(advanced.state, QsoState::SendingReport { .. }),
        "precondition: the DX's report must advance the QSO to SendingReport, got {:?}",
        advanced.state
    );

    let applied = manager
        .apply_tx_offset_switch(qso_id, FREQ + 400.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("a nudge on a still-retransmittable QSO must commit");
    assert_eq!(applied, FREQ + 400.0);

    let mut rx = manager.subscribe();
    manager
        .rearm_manual_calls_at(manager.now_utc() + SignedDuration::from_secs(60))
        .await;
    let sent_at: Vec<f64> = drain(&mut rx)
        .into_iter()
        .filter_map(|e| match e {
            QsoEvent::MessageToSend {
                qso_id: id,
                frequency,
                ..
            } if id == qso_id => Some(frequency),
            _ => None,
        })
        .collect();
    assert!(
        sent_at.contains(&(FREQ + 400.0)),
        "the next rearm must send at the nudged offset, got {sent_at:?}"
    );
}

/// PAN-79 / PAN-72 (Codex round 3 on PR #350, finding 2): a resolution that
/// lands back on the QSO's CURRENT offset relocated nothing, so it must not
/// be committed as a successful move.
///
/// The allocator returns `avoid_hz` unchanged when every candidate is
/// excluded — its documented "no valid relocation" signal — and the drain
/// hands that straight to this method. Treating it as a success cleared
/// `stall_cycles`, destroying valid accumulated stall evidence (an operator
/// `u` nudge on a QSO with a partial streak is the live case), delaying the
/// automatic recovery by another full `qso_stall_switch_after` window, and
/// emitting a `TxOffsetApplied` that claims a move that never happened.
#[tokio::test]
async fn apply_tx_offset_switch_refuses_a_no_op_and_keeps_the_stall_evidence() {
    let manager = manager_auto(test_config());
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    // A partial (below-threshold) stall streak, exactly what an operator
    // `u` nudge lands on top of.
    {
        let mut qsos = manager.qsos.write().await;
        qsos.get_mut(&qso_id).unwrap().metadata.stall_cycles = 3;
    }
    let _ = drain(&mut rx);

    let err = manager
        .apply_tx_offset_switch(qso_id, FREQ, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect_err("a resolution equal to the current offset relocates nothing");
    assert!(
        matches!(
            err,
            QsoManagerError::OffsetActionNoOp { qso_id: id, offset_hz }
                if id == qso_id && offset_hz == FREQ
        ),
        "expected OffsetActionNoOp, got {err:?}"
    );
    assert!(
        err.is_expected_offset_action_refusal(),
        "an exhausted candidate space is an expected outcome, not a fault — \
         the drain must log+skip it, not warn"
    );
    assert!(
        !matches!(
            err,
            QsoManagerError::QsoNotActive { .. } | QsoManagerError::OffsetActionStale { .. }
        ),
        "a no-op must stay distinguishable from the staleness refusals"
    );

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.stall_cycles, 3,
        "the accumulated stall evidence must survive a no-op relocation, or \
         automatic recovery is delayed by another full threshold"
    );
    assert_eq!(after.metadata.frequency, FREQ);
    assert_eq!(
        after.metadata.partner_freq, None,
        "a no-op must not manufacture a Tx/Rx split"
    );
    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::TxOffsetApplied { .. })),
        "a no-op must not announce a move that never happened, got {events:?}"
    );

    // A float-noise-sized "move" is the same no-op (the 1.0 Hz tolerance
    // this method already uses for its partner_freq latch)...
    let err = manager
        .apply_tx_offset_switch(qso_id, FREQ + 0.5, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect_err("a sub-Hz move is still a no-op");
    assert!(matches!(err, QsoManagerError::OffsetActionNoOp { .. }));

    // ...while a real relocation still commits, and clears the streak.
    let applied = manager
        .apply_tx_offset_switch(qso_id, FREQ + 400.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("a real move must still commit");
    assert_eq!(applied, FREQ + 400.0);
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after.metadata.stall_cycles, 0);
}

/// In the default Hold mode, `stall_cycles` may still tick (cheap
/// tracking) but `TxOffsetActionNeeded` must never be emitted, no matter
/// how many silent rearm cycles elapse.
#[tokio::test]
async fn hold_mode_never_emits_tx_offset_action_needed() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = clocked_manager(config); // default Hold
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // Well past the threshold (2) worth of silent rearm cycles.
    for i in 1..=6i64 {
        manager
            .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15 * i))
            .await;
    }

    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })),
        "Hold mode must never emit TxOffsetActionNeeded, got {events:?}"
    );
}

// ── partner_freq bookkeeping across an offset switch ──────────────────
//
// `QsoState::frequency()` is dual-purposed: it is our TX offset AND —
// whenever `metadata.partner_freq` is `None` — the RX-side baseline every
// routing/relevance and drift gate keys on
// (`metadata.partner_freq.unwrap_or(qso_freq)`). A stall switch moves
// only OUR side, so once it writes the new offset into the state it must
// also latch `partner_freq` to the offset it moved AWAY from — exactly
// the bookkeeping `compute_manual_tx_offset` already performs for the
// analogous "our TX diverges from the DX's frequency" case.

/// The DX's real replies keep arriving at the PRE-switch offset. They must
/// still route to this QSO after the switch; re-baselining the relevance
/// gate onto our new TX offset silently orphans every one of them (the
/// 400 Hz move here is far outside `ESTABLISHED_FREQ_TOLERANCE_HZ`).
#[tokio::test]
async fn stall_switch_keeps_routing_dx_replies_at_the_old_offset() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let before = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        before.metadata.partner_freq, None,
        "precondition: the ordinary Tx=Rx path opens with partner_freq = None"
    );

    let new_offset = FREQ + 400.0;
    let applied = manager
        .apply_tx_offset_switch(qso_id, new_offset, stall_origin(&manager, qso_id).await)
        .await
        .unwrap();
    assert_eq!(applied, new_offset);

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, new_offset,
        "our TX offset must move to the switched-to value"
    );
    assert_eq!(after.state.frequency(), Some(new_offset));
    assert_eq!(
        after.metadata.partner_freq,
        Some(FREQ),
        "the switch must latch where we still expect to HEAR the DX (the \
         offset we moved away from), or every RX-side gate re-baselines \
         onto our own new TX offset"
    );

    let report = MessageType::SignalReport {
        to_station: OUR.into(),
        from_station: DX.into(),
        report: -11,
    };
    assert!(
        manager.is_message_relevant(&after.state, &after.metadata, &report, FREQ, false),
        "the DX's reply at the pre-switch offset must stay relevant"
    );
    assert_eq!(
        manager.find_qsos_for_message(&report, FREQ).await,
        vec![qso_id],
        "the DX's reply at the pre-switch offset must still route to this QSO"
    );
}

/// The drift half of the same regression: with the RX baseline wrongly
/// pinned to our new TX offset, the DX's own (entirely unchanged)
/// frequency reads as a drift candidate, and two sightings >=5s apart
/// relatch `metadata.frequency` and the state frequency straight back onto
/// the offset the switch existed to escape.
#[tokio::test]
async fn stall_switch_does_not_relatch_back_onto_the_abandoned_offset() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let new_offset = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, new_offset, stall_origin(&manager, qso_id).await)
        .await
        .unwrap();

    let report = MessageType::SignalReport {
        to_station: OUR.into(),
        from_station: DX.into(),
        report: -11,
    };
    let t0 = manager.now_utc();
    manager
        .maybe_confirm_frequency_drift_at(&report, FREQ, t0)
        .await;
    assert_eq!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .pending_freq_drift,
        None,
        "a DX sitting exactly where we left it is not a drift candidate"
    );
    manager
        .maybe_confirm_frequency_drift_at(&report, FREQ, t0 + SignedDuration::from_secs(6))
        .await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, new_offset,
        "the switch must survive: no relatch back onto the abandoned offset"
    );
    assert_eq!(after.state.frequency(), Some(new_offset));
    assert_eq!(after.metadata.partner_freq, Some(FREQ));
}

/// A QSO that already diverged (Hound, an offset hold, a collision nudge,
/// a passband clamp) already carries the correct "where we hear the DX".
/// A stall switch is purely a TX-side move, so it must not clobber it.
#[tokio::test]
async fn stall_switch_preserves_an_existing_partner_freq() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            None,
            CallInitiation::Manual,
            Some(2400.0),
            false,
            None,
        )
        .await
        .unwrap();

    manager
        .apply_tx_offset_switch(qso_id, FREQ + 400.0, stall_origin(&manager, qso_id).await)
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.partner_freq,
        Some(2400.0),
        "an already-latched partner_freq is where we hear the DX; a TX-side \
         switch must leave it untouched"
    );
    assert_eq!(after.metadata.frequency, FREQ + 400.0);
}

/// A no-op switch (a Revert onto the offset we are already sitting on)
/// must not manufacture a `partner_freq` — the QSO is still plainly Tx=Rx,
/// and inventing a split would change nothing but add a field the drift
/// gate then treats as a deliberate divergence.
///
/// Since PAN-79 / round 3 finding 2 such a switch is refused outright
/// (`OffsetActionNoOp`) rather than committed as a mutation-free success,
/// which subsumes this guarantee — asserted here as well so the
/// partner_freq contract stays pinned at its own level.
#[tokio::test]
async fn no_op_switch_leaves_partner_freq_none() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();

    let err = manager
        .apply_tx_offset_switch(qso_id, FREQ, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect_err("a no-op switch is refused, not committed");
    assert!(matches!(err, QsoManagerError::OffsetActionNoOp { .. }));

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.partner_freq, None,
        "a switch that changes nothing must leave the Tx=Rx path untouched"
    );
    assert_eq!(after.metadata.frequency, FREQ);
}

/// A manual CQ QSO that nobody has answered yet has NO DX to keep pointing
/// at: `their_callsign()` is `None`, and the offset we move away from is
/// our own abandoned CQ frequency, not a partner's. Latching it into
/// `partner_freq` re-points every RX-side gate at an offset no one is
/// transmitting on — and because a pre-establishment QSO is judged with
/// the TIGHT `FREQ_TOLERANCE_HZ` (15 Hz), not the 100 Hz established
/// bound, a caller answering us at the offset we are ACTUALLY calling on
/// is rejected forever and a duplicate QSO object gets spawned in its
/// place. The latch must therefore be gated on an established DX identity,
/// the same discriminator `is_message_relevant`/`classify_relevance`
/// already use to choose between the two tolerances.
#[tokio::test]
async fn cq_stall_switch_leaves_partner_freq_none_and_keeps_answering_callers_routing() {
    const CALLER: &str = "W9XYZ";

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // Two silent rearm cycles (two slots apart, feeding distinct
    // own-parity TX slots — PAN-128): nobody has come back to our CQ,
    // so the stall detector trips and asks for a Switch off this
    // offset.
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(45))
        .await;
    let action = drain(&mut rx).into_iter().find_map(|e| match e {
        QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
        _ => None,
    });
    assert_eq!(
        action,
        Some(OffsetAction::Switch { avoid_hz: FREQ }),
        "a silent CQ must stall into a Switch off its current offset"
    );

    // The coordinator commits the resolved Switch: we are now CALLING CQ
    // on the new offset.
    let new_offset = FREQ + 400.0;
    let applied = manager
        .apply_tx_offset_switch(qso_id, new_offset, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    assert_eq!(applied, new_offset);

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after.metadata.frequency, new_offset);
    assert_eq!(after.state.frequency(), Some(new_offset));
    assert!(
        after.state.their_callsign().is_none(),
        "precondition: an unanswered CQ has no DX identity yet"
    );
    assert_eq!(
        after.metadata.partner_freq, None,
        "there is no DX at the abandoned CQ offset to point the RX gates \
         at — latching one strands the QSO at a frequency nobody uses"
    );

    // A caller answers us where we are actually calling: the new offset.
    let answer = MessageType::CqResponse {
        calling_station: OUR.into(),
        responding_station: CALLER.into(),
        grid: Some("FN31".into()),
    };
    assert!(
        manager.is_message_relevant(&after.state, &after.metadata, &answer, new_offset, false),
        "an answer at the offset we are calling on must stay relevant"
    );
    assert_eq!(
        manager.find_qsos_for_message(&answer, new_offset).await,
        vec![qso_id],
        "an answer at the offset we are calling on must route to this CQ, \
         not spawn a duplicate QSO"
    );
}

/// PAN-72 (Codex round 4 on PR #350, finding 3): the resend that CROSSES
/// the stall threshold goes out on the PRE-switch offset — the same
/// `rearm_manual_calls_at` pass emits it and raises the action — and the
/// coordinator's once-per-slot drain then moves the QSO. A caller who
/// answers that transmission is answering the offset we have just left.
///
/// For an unanswered manual CQ that is fatal: `CallingCq` now carries the
/// NEW offset, round 1's fix deliberately leaves `partner_freq` `None`
/// (there is no DX at the abandoned offset to point the RX gates at), and
/// the pre-establishment gate is only 15 Hz wide — so the answer is
/// rejected outright and `maybe_answer_caller` spawns a duplicate QSO in
/// its place.
///
/// This drives the real sequence rather than the pieces: rearm to the
/// threshold, commit the switch the threshold raised, then deliver the
/// caller's answer at the offset the triggering resend actually used.
#[tokio::test]
async fn a_caller_answering_the_pre_switch_resend_still_routes_to_the_cq() {
    const CALLER: &str = "W9XYZ";

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // Two silent rearm cycles, two slots apart so the second feeds a
    // distinct own-parity TX slot (PAN-128). The SECOND one both
    // re-emits our CQ at FREQ (the frame a caller can answer) and trips
    // the stall threshold.
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(45))
        .await;
    let events = drain(&mut rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            QsoEvent::MessageToSend { frequency, .. } if (*frequency - FREQ).abs() < f64::EPSILON
        )),
        "precondition: the threshold-crossing rearm re-sends our CQ on the \
         PRE-switch offset, {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            QsoEvent::TxOffsetActionNeeded {
                action: OffsetAction::Switch { avoid_hz },
                ..
            } if (*avoid_hz - FREQ).abs() < f64::EPSILON
        )),
        "precondition: the same pass raises the Switch, {events:?}"
    );

    // The threshold-crossing CQ airs (PAN-182 releases the held switch)
    // and the coordinator's drain commits the switch a moment later.
    confirm_last_frame_aired(&manager, qso_id, FREQ).await;
    let new_offset = FREQ + 400.0;
    assert_eq!(
        manager
            .apply_tx_offset_switch(qso_id, new_offset, stall_origin(&manager, qso_id).await)
            .await
            .unwrap(),
        new_offset
    );

    // ...and only now does the caller who heard our PRE-switch CQ answer,
    // on the offset that CQ actually went out on.
    let answer = MessageType::CqResponse {
        calling_station: OUR.into(),
        responding_station: CALLER.into(),
        grid: Some("FN31".into()),
    };
    assert_eq!(
        manager.find_qsos_for_message(&answer, FREQ).await,
        vec![qso_id],
        "an answer to the frame we transmitted on the pre-switch offset \
         must still route to this CQ, not be gated away into a duplicate QSO"
    );

    // And it must actually advance the QSO, not merely route.
    manager
        .process_message(answer, format!("{OUR} {CALLER} FN31"), FREQ, Some(-12.0))
        .await
        .unwrap();
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.state.their_callsign(),
        Some(CALLER),
        "the pre-switch answer must establish the QSO, got {:?}",
        after.state
    );
}

/// PAN-72 round-12 P1 fix (Codex round 12, thread on
/// `qso_manager.rs:4047`): the routing test above establishes the QSO but
/// never checks `partner_freq` — the piece this fix closes. An
/// unanswered `CallingCq`'s switch deliberately leaves `partner_freq`
/// `None` (there is no DX yet to latch it to), and the credit block
/// unconditionally clears `pre_switch_offset` once a genuine advance is
/// recognized. If it does not ALSO latch `partner_freq` to the offset the
/// caller actually answered on, the next `StateChanged` recomputes the
/// decoder hint from the QSO's brand-new TX offset instead — a caller who
/// has no way to know we moved keeps replying at the vacated offset, and
/// for a relocation bigger than the decoder's narrow window the rescue
/// stalls again immediately after succeeding.
#[tokio::test]
async fn an_advance_rescuing_the_pre_switch_cq_latches_the_callers_frequency() {
    const CALLER: &str = "W9XYZ";

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // Two silent rearm cycles on two consecutive own-parity TX slots,
    // each 1 s into the slot (PAN-239): anchored to the QSO's own slot
    // grid, not `opened_at`, so every run takes the same path whatever
    // wall-clock second it started in. The second rearm re-sends our CQ
    // at FREQ and trips the stall threshold.
    stall_on_two_own_parity_slots(&manager, qso_id, opened_at).await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "precondition: two silent rearm cycles on distinct own-parity \
         slots must trip the stall threshold"
    );

    // PAN-182: the threshold-tripping rearm armed a pending stall
    // trigger; the drain may commit the switch only once that frame's
    // outcome is known. Report it aired (what the TX worker's
    // `TxFrameSent` does) so the commit below is genuinely allowed.
    confirm_last_frame_aired(&manager, qso_id, FREQ).await;

    let new_offset = FREQ + 400.0;
    assert_eq!(
        manager
            .apply_tx_offset_switch(qso_id, new_offset, stall_origin(&manager, qso_id).await)
            .await
            .unwrap(),
        new_offset
    );
    let switched = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        switched.metadata.partner_freq, None,
        "precondition: an unanswered CallingCq's switch deliberately \
         leaves partner_freq unset — there is no DX to latch it to yet"
    );

    // The caller who heard our PRE-switch CQ answers, still on the
    // offset that CQ actually went out on — establishing the QSO.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.into(),
                responding_station: CALLER.into(),
                grid: Some("FN31".into()),
            },
            format!("{OUR} {CALLER} FN31"),
            FREQ,
            Some(-12.0),
        )
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(
            &after.state,
            QsoState::WaitingForReport { their_callsign, frequency, .. }
                if their_callsign == CALLER && *frequency == new_offset
        ),
        "the pre-switch answer must establish the QSO in WaitingForReport \
         carrying our (new) TX offset, got {:?}",
        after.state
    );
    assert!(
        after.metadata.pre_switch_offset.is_none(),
        "the establishing advance must consume the pre-switch grace in \
         the same breath — from here on the caller is judged by the plain \
         established relevance gate"
    );
    assert_eq!(
        after.metadata.frequency, new_offset,
        "precondition: we are now TX-ing on the new offset"
    );
    assert_eq!(
        after.state.frequency(),
        Some(new_offset),
        "the state's own TX frequency must sit on the new offset too"
    );
    assert_eq!(
        after.metadata.partner_freq,
        Some(FREQ),
        "the caller answered at {FREQ} Hz (the vacated offset) — \
         partner_freq must latch there instead of staying None, or the \
         decoder hint recentres onto our new TX offset instead of where \
         the caller actually is"
    );
    // Mirrors `coordinator::qso::decoder_hint_freq_for` (a different
    // crate, out of this fix's scope) without depending on it: that
    // helper resolves the decoder hint as
    // `partner_freq.or_else(|| state.frequency())`. With the latch, that
    // now resolves to the caller's real frequency rather than the
    // never-heard-from-there new TX offset.
    let decoder_hint = after.metadata.partner_freq.or(after.state.frequency());
    assert_eq!(
        decoder_hint,
        Some(FREQ),
        "the decoder hint must resolve to the caller's actual frequency \
         ({FREQ} Hz), not the new TX offset ({new_offset} Hz)"
    );

    // PAN-82: the grace is consumed, so the caller's NEXT frame is
    // judged by the established relevance gate alone — the latched
    // `partner_freq` (the vacated offset) is now the RX baseline. The
    // caller, who never moved, rogers our report with their R-report
    // still on FREQ. It must route to this QSO alone and advance it
    // immediately: no sleep, no extra drift sightings, and (absent the
    // latch above) no two-sighting `maybe_confirm_frequency_drift_at`
    // relatch to wait for. Without the latch this frame sat 400 Hz
    // outside the 100 Hz established gate and the exchange went deaf.
    let report = MessageType::ReportAck {
        from_station: CALLER.into(),
        to_station: OUR.into(),
        report: -7,
    };
    assert_eq!(
        manager.find_qsos_for_message(&report, FREQ).await,
        vec![qso_id],
        "after the grace is consumed, the caller's R-report at the \
         vacated offset must still route to this QSO via the latched \
         partner_freq baseline"
    );
    manager
        .process_message(report, format!("{OUR} {CALLER} R-07"), FREQ, Some(-15.0))
        .await
        .unwrap();

    let confirmed = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(
            &confirmed.state,
            QsoState::WaitingForConfirmation { their_callsign, .. } if their_callsign == CALLER
        ),
        "the caller's R-report must advance the rescued QSO to \
         WaitingForConfirmation, got {:?}",
        confirmed.state
    );
    assert_eq!(
        confirmed.metadata.partner_freq,
        Some(FREQ),
        "the advance must keep the latched RX baseline on the offset the \
         caller is actually replying from"
    );
    assert_eq!(
        confirmed.metadata.frequency, new_offset,
        "our TX offset must stay on the post-switch value — the rescue \
         must not undo the switch"
    );
    assert_eq!(
        confirmed.state.frequency(),
        Some(new_offset),
        "the state's own TX frequency must stay on the post-switch value"
    );
}

/// The pre-switch RX baseline is a bounded grace, not a permanent second
/// window: once `PRE_SWITCH_OFFSET_GRACE` lapses, an answer at the
/// abandoned offset is judged against the current offset again — otherwise
/// the QSO would keep accepting traffic at a frequency it no longer uses.
#[tokio::test]
async fn the_pre_switch_rx_baseline_lapses_with_its_grace() {
    const CALLER: &str = "W9XYZ";

    let manager = manager_auto(test_config());
    let qso_id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let new_offset = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, new_offset, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();

    let mut progress = manager.get_qso(qso_id).await.unwrap();
    let answer = MessageType::CqResponse {
        calling_station: OUR.into(),
        responding_station: CALLER.into(),
        grid: Some("FN31".into()),
    };
    assert!(
        manager.is_message_relevant(&progress.state, &progress.metadata, &answer, FREQ, false),
        "precondition: inside the grace the vacated offset is still accepted"
    );

    progress.metadata.pre_switch_offset = Some(crate::states::PreSwitchOffset {
        offset_hz: FREQ,
        left_at: manager.now_utc()
            - manager.pre_switch_offset_grace()
            - SignedDuration::from_secs(1),
        operator_forced: true,
    });
    assert!(
        !manager.is_message_relevant(&progress.state, &progress.metadata, &answer, FREQ, false),
        "a lapsed pre-switch baseline must stop accepting traffic at the \
         abandoned offset"
    );
    assert!(
        manager.is_message_relevant(
            &progress.state,
            &progress.metadata,
            &answer,
            new_offset,
            false
        ),
        "the current offset must keep matching regardless"
    );
}

/// The other half of round 4, finding 3: an ESTABLISHED QSO's reply to the
/// pre-switch frame does route (the switch latches `partner_freq` to the
/// vacated offset), but the advance it drives must NOT record the brand-new
/// offset — which has never been transmitted on — as `last_known_good`.
/// Doing so poisons the Revert target: a later stall would "revert" to the
/// offset it is already on, and the genuinely-proven one is lost. Since
/// PAN-132 this QSO's latched split routes through the round-6 provenance
/// rule instead of the decode-frequency check; this test pins that the
/// stall-triggered outcome is unchanged.
#[tokio::test]
async fn an_advance_answering_the_pre_switch_frame_credits_the_old_offset() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // Stall out on FREQ across two consecutive own-parity TX slots,
    // anchored to the QSO's own slot grid (PAN-239); the
    // threshold-crossing rearm re-sends there.
    stall_on_two_own_parity_slots(&manager, qso_id, opened_at).await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "precondition: two silent rearm cycles on distinct own-parity \
         slots must trip the stall threshold"
    );

    // PAN-182: release the trigger the threshold rearm armed before the
    // stall-origin switch may commit.
    confirm_last_frame_aired(&manager, qso_id, FREQ).await;

    let new_offset = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, new_offset, stall_origin(&manager, qso_id).await)
        .await
        .unwrap();

    // The DX answers the pre-switch frame — still transmitting where it
    // always was, which the switch latched as `partner_freq`.
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, new_offset,
        "precondition: the QSO is on the post-switch offset"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(FREQ),
        "the DX answered the frame we sent on {FREQ} Hz — that, not the \
         never-transmitted {new_offset} Hz, is the proven offset"
    );
    // PAN-72 round-12 P1 fix, regression: this QSO was ALREADY
    // established (`respond_to_cq_manual`) before the switch, so
    // `apply_tx_offset_switch` itself latched `partner_freq = Some(FREQ)`
    // at switch-commit time — `partner_freq` is non-`None` going into
    // the credit block above. The CallingCq-rescue latch must not touch
    // an already-established QSO's `partner_freq`; it must come out
    // exactly as the switch left it.
    assert_eq!(
        after.metadata.partner_freq,
        Some(FREQ),
        "an already-established QSO's partner_freq must be left exactly \
         as apply_tx_offset_switch latched it, untouched by the \
         CallingCq-rescue latch"
    );
}

/// PAN-72 (Codex round 5 on PR #350, finding 2): the mirror image of the
/// test above. Round 4 credited the vacated offset on ANY forward advance
/// inside the grace, checking only the clock — never which offset the
/// reply actually arrived on.
///
/// That assumption holds for a STALL-triggered switch (the triggering
/// resend went out on the old offset one slot before the commit, so its
/// answer cannot be about the new one), but an operator-forced `u` nudge
/// is not coupled to any such resend: it can land on a QSO that is about
/// to transmit, the very next rearm goes out on the NEW offset, and the
/// answer to THAT arrives well inside the 30 s grace. Crediting the
/// vacated offset there records an offset that just demonstrably failed
/// as known-good, and a later stall then emits `Revert` back onto it —
/// undoing the nudge that had just been proven to work.
#[tokio::test]
async fn an_advance_at_the_post_switch_offset_credits_the_new_offset() {
    const CALLER: &str = "W9XYZ";

    let manager = manager_auto(test_config());
    let qso_id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();

    // Operator `u` nudge: no triggering resend, no raised generation.
    let new_offset = FREQ + 400.0;
    assert_eq!(
        manager
            .apply_tx_offset_switch(qso_id, new_offset, OffsetRelocationOrigin::OperatorForced)
            .await
            .unwrap(),
        new_offset
    );

    // The next CQ goes out on the NEW offset and a caller answers it
    // there, still inside `PRE_SWITCH_OFFSET_GRACE`.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.into(),
                responding_station: CALLER.into(),
                grid: Some("FN31".into()),
            },
            format!("{OUR} {CALLER} FN31"),
            new_offset,
            Some(-12.0),
        )
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.state.their_callsign(),
        Some(CALLER),
        "precondition: the answer advanced this QSO"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(new_offset),
        "the caller answered us on {new_offset} Hz — crediting the vacated \
         {FREQ} Hz would aim a later Revert at the offset the nudge escaped"
    );
    assert_eq!(
        after.metadata.pre_switch_offset, None,
        "the ambiguity is resolved either way — the grace is consumed"
    );
}

/// PAN-72 (Codex round 5 re-review on PR #350): the decode-frequency match
/// check above is only *evidence* when the QSO is Tx=Rx — it compares the
/// DX's transmit frequency against OUR vacated TX offset, which are the
/// same number only when no split exists.
///
/// A QSO that already carried a `partner_freq` before the switch (Hound, an
/// operator `o` hold, a de-conflict nudge, a passband clamp) is not Tx=Rx:
/// `apply_tx_offset_switch` deliberately leaves that latch alone, so the DX
/// keeps transmitting at `partner_freq`, nowhere near the offset we vacated.
/// The match check then fails for a reason that says nothing about which of
/// our frames was answered, and falling through would credit the brand-new
/// offset — never transmitted on, zero evidence behind it. For a
/// STALL-triggered switch — this test — the check is therefore skipped and
/// the vacated offset credited, exactly as round 4 did: the switch only
/// happened BECAUSE that offset stopped working, so the triggering resend
/// went out on it and the new one has never been keyed. (Round 6, finding 2
/// splits the OPERATOR-forced case out — see the test below.)
#[tokio::test]
async fn an_advance_on_a_pre_existing_split_credits_the_vacated_offset() {
    const DX_FREQ: f64 = 2400.0;

    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            None,
            CallInitiation::Manual,
            Some(DX_FREQ),
            false,
            None,
        )
        .await
        .unwrap();

    let new_offset = FREQ + 400.0;
    assert_eq!(
        manager
            .apply_tx_offset_switch(qso_id, new_offset, stall_origin(&manager, qso_id).await)
            .await
            .unwrap(),
        new_offset
    );
    let switched = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        switched.metadata.partner_freq,
        Some(DX_FREQ),
        "precondition: the pre-existing split survives the TX-side switch"
    );

    // The DX answers where it has been transmitting all along — at
    // `partner_freq`, which matches neither the vacated nor the new TX
    // offset. Well inside `PRE_SWITCH_OFFSET_GRACE`.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{OUR} {DX} -07"),
            DX_FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, new_offset,
        "precondition: the advance routed and the QSO is on the new offset"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(FREQ),
        "the DX transmits at {DX_FREQ} Hz regardless of where we key, so its \
         reply cannot discriminate our two TX offsets — the vacated {FREQ} \
         Hz is the only one with a proven history, and {new_offset} Hz has \
         never been transmitted on"
    );
}

/// PAN-72 (Codex round 6 on PR #350, finding 2): the same pre-existing
/// split, moved by the OPERATOR's `u` nudge instead of the stall detector —
/// and the opposite answer.
///
/// The decode frequency is just as useless here (the DX transmits at
/// `partner_freq` no matter where we key), but the priors are reversed. A
/// stall switch fires because the old offset demonstrably stopped working;
/// a nudge indicts nothing — the operator simply chose to move, our very
/// next frame goes out on the NEW offset, and an advancing reply after it is
/// better evidence for that new offset than for the one we left
/// deliberately. Crediting the vacated offset here records an offset with no
/// standing over the new one as known-good, and a later stall then emits
/// `Revert` straight back onto it — undoing the nudge that had just worked.
#[tokio::test]
async fn an_operator_nudge_on_a_pre_existing_split_credits_the_new_offset() {
    const DX_FREQ: f64 = 2400.0;

    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            None,
            CallInitiation::Manual,
            Some(DX_FREQ),
            false,
            None,
        )
        .await
        .unwrap();

    // Operator `u` nudge: no stall evidence, no triggering old-offset
    // resend, no raised generation.
    let new_offset = FREQ + 400.0;
    assert_eq!(
        manager
            .apply_tx_offset_switch(qso_id, new_offset, OffsetRelocationOrigin::OperatorForced)
            .await
            .unwrap(),
        new_offset
    );
    let switched = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        switched.metadata.partner_freq,
        Some(DX_FREQ),
        "precondition: the pre-existing split survives the TX-side nudge"
    );

    // The DX answers where it has always transmitted — at `partner_freq`,
    // which matches neither of our TX offsets. Well inside
    // `PRE_SWITCH_OFFSET_GRACE`.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{OUR} {DX} -07"),
            DX_FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, new_offset,
        "precondition: the advance routed and the QSO is on the nudged offset"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(new_offset),
        "the operator moved us to {new_offset} Hz and the DX advanced right \
         after — crediting the vacated {FREQ} Hz would aim a later Revert at \
         the offset the nudge deliberately left"
    );
    assert_eq!(
        after.metadata.pre_switch_offset, None,
        "the first advance after a relocation consumes the grace either way"
    );
}

/// PAN-132 (Codex round 15 on PR #350, P2): the test above without a
/// pre-existing split. A simplex QSO has `partner_freq: None` until a switch
/// moves it; `apply_tx_offset_switch`'s established-QSO latch then sets
/// `partner_freq` to exactly the offset it vacated — for an operator nudge
/// as much as for a stall switch. The DX keeps transmitting there whatever
/// we do, so its reply says nothing about which of our offsets it answered,
/// and provenance must decide, exactly as on a pre-existing split. The
/// crediting block used to count that latch as Tx=Rx; its decode-frequency
/// check then always matched, and the nudge credited the offset the operator
/// had just left.
#[tokio::test]
async fn an_operator_nudge_on_an_established_simplex_qso_credits_the_new_offset() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.partner_freq,
        None,
        "precondition: a simplex QSO — no split before the nudge"
    );

    let new_offset = FREQ + 400.0;
    assert_eq!(
        manager
            .apply_tx_offset_switch(qso_id, new_offset, OffsetRelocationOrigin::OperatorForced)
            .await
            .unwrap(),
        new_offset
    );
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.partner_freq,
        Some(FREQ),
        "precondition: the established-QSO latch created the split"
    );

    // The DX answers where it has always transmitted — the vacated offset,
    // now latched as `partner_freq`. Well inside `PRE_SWITCH_OFFSET_GRACE`.
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, new_offset,
        "precondition: the advance routed and the QSO is on the nudged offset"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(new_offset),
        "the operator moved us to {new_offset} Hz — crediting the vacated \
         {FREQ} Hz would aim a later Revert at the offset the nudge left"
    );
    assert_eq!(
        after.metadata.partner_freq,
        Some(FREQ),
        "crediting must leave the latch exactly as the switch set it"
    );
    assert_eq!(
        after.metadata.pre_switch_offset, None,
        "the first advance after a relocation consumes the grace either way"
    );
}

/// PAN-132: the same rule when `partner_freq` equals the vacated offset
/// for a reason other than this switch's latch — an earlier move put us
/// back onto the DX's frequency, and the operator now nudges off it again.
/// Who created the split is irrelevant: the DX transmits at `partner_freq`
/// regardless, so provenance decides.
#[tokio::test]
async fn an_operator_nudge_off_a_coincident_partner_offset_credits_the_new_offset() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    // Out (the latch sets `partner_freq = FREQ`), back onto FREQ, out again.
    let final_offset = FREQ + 250.0;
    for hz in [FREQ + 400.0, FREQ, final_offset] {
        manager
            .apply_tx_offset_switch(qso_id, hz, OffsetRelocationOrigin::OperatorForced)
            .await
            .unwrap();
    }
    let switched = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(switched.metadata.partner_freq, Some(FREQ));
    assert_eq!(
        switched.metadata.pre_switch_offset.map(|pre| pre.offset_hz),
        Some(FREQ),
        "precondition: the last switch vacated the DX's own frequency"
    );

    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after.metadata.frequency, final_offset);
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(final_offset),
        "an operator nudge credits where it moved us, however the split arose"
    );
}

/// PAN-132: the harm the ticket names, end to end. After a nudge on a
/// simplex QSO and the DX's advancing reply, the next stall must look for a
/// new offset — not `Revert` onto the offset the operator deliberately left.
#[tokio::test]
async fn a_stall_after_an_operator_nudge_on_a_simplex_qso_switches_instead_of_reverting() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let new_offset = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, new_offset, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    let entered_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // Two silent rearms, two slots apart, so each feeds a distinct
    // own-parity TX slot (PAN-128) and the second trips the threshold.
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(15))
        .await;
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(45))
        .await;
    let action = drain(&mut rx).into_iter().find_map(|e| match e {
        QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
        _ => None,
    });
    assert_eq!(
        action,
        Some(OffsetAction::Switch {
            avoid_hz: new_offset
        }),
        "the nudged offset is the known-good one, so a stall Switches away \
         from it rather than Reverting onto the {FREQ} Hz the operator left"
    );
}

/// PAN-83: the slot parity of a reply processed now, and an `aired_at`
/// in our slot just before it. A reply in slot R answers the frame that
/// aired in R-1; reporting it 2 s before "now" lands inside
/// `answered_frame_offset_hz`'s window for any test that finishes within
/// one slot.
fn reply_window_now() -> (pancetta_core::slot::SlotParity, UtcTimestamp) {
    let now = pancetta_core::time::slot_grid_origin();
    (
        pancetta_core::slot::SlotParity::of_with_period(now, DEFAULT_SLOT_NS),
        now - SignedDuration::from_secs(2),
    )
}

/// What the TX worker's `TxFrameSent` does for a frame of this QSO.
async fn report_aired(
    manager: &QsoManager,
    qso_id: QsoId,
    frequency_hz: f64,
    aired_at: UtcTimestamp,
) {
    manager
        .confirm_stall_trigger_aired(
            qso_id,
            "aired",
            &UnsentAttemptPayload {
                frequency_hz,
                remote_binding: Some((false, None)),
            },
            aired_at,
        )
        .await;
}

/// `send_report`, carrying the decoded slot parity production tags.
async fn send_report_in(
    manager: &QsoManager,
    parity: pancetta_core::slot::SlotParity,
    from_hz: f64,
) {
    manager
        .process_message_with_parity(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{OUR} {DX} -07"),
            from_hz,
            Some(-15.0),
            Some(parity),
        )
        .await
        .unwrap();
}

/// PAN-83: the ticket's split-QSO case. Our frame aired on the old
/// offset, the operator's `u` nudge then committed (a nudge pressed while
/// our frame is keyed is enqueued at keypress and committed at the next
/// once-per-slot drain — the start of the DX's slot), and the DX's reply
/// in the following slot answered the frame that aired before the move.
/// Nothing has transmitted on the nudged offset yet, so the aired frame —
/// not the nudge's provenance — names the offset the DX actually heard.
#[tokio::test]
async fn a_nudge_committed_after_the_old_offset_aired_credits_the_old_offset() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let (parity, aired_at) = reply_window_now();
    report_aired(&manager, qso_id, FREQ, aired_at).await;

    let nudged = FREQ + 400.0;
    assert_eq!(
        manager
            .apply_tx_offset_switch(qso_id, nudged, OffsetRelocationOrigin::OperatorForced)
            .await
            .unwrap(),
        nudged
    );

    send_report_in(&manager, parity, FREQ).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, nudged,
        "precondition: the QSO is on the nudged offset"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(FREQ),
        "the DX's reply answered the frame that aired at {FREQ} Hz — \
         crediting the never-keyed {nudged} Hz (provenance's answer) would \
         poison the next stall decision"
    );
    assert_eq!(
        after.metadata.pre_switch_offset, None,
        "the first advance after a relocation consumes the grace either way"
    );
    assert_eq!(
        after.metadata.partner_freq,
        Some(FREQ),
        "the switch's established-QSO latch stands"
    );
}

/// PAN-83: the same nudge timing on a split that predates the switch
/// (Hound, an `o` hold, a de-conflict nudge). The DX transmits at
/// `partner_freq`, so the reply's decode frequency still cannot name our
/// answered frame — but the aired-frame record can, and it says the old
/// offset.
#[tokio::test]
async fn a_nudge_on_a_pre_existing_split_after_the_old_offset_aired_credits_the_old_offset() {
    const DX_FREQ: f64 = 2400.0;

    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            None,
            CallInitiation::Manual,
            Some(DX_FREQ),
            false,
            None,
        )
        .await
        .unwrap();
    let (parity, aired_at) = reply_window_now();
    report_aired(&manager, qso_id, FREQ, aired_at).await;

    let nudged = FREQ + 400.0;
    assert_eq!(
        manager
            .apply_tx_offset_switch(qso_id, nudged, OffsetRelocationOrigin::OperatorForced)
            .await
            .unwrap(),
        nudged
    );

    send_report_in(&manager, parity, DX_FREQ).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, nudged,
        "precondition: the advance routed and the QSO is on the nudged offset"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(FREQ),
        "the reply answered our {FREQ} Hz frame; {nudged} Hz has never \
         been keyed, provenance notwithstanding"
    );
    assert_eq!(
        after.metadata.partner_freq,
        Some(DX_FREQ),
        "the pre-existing split survives the crediting"
    );
}

/// PAN-83: two relocations in consecutive drains with no frame between
/// them. The vacated-offset bookkeeping only remembers the most recent
/// move (2300 vacated 1900), so provenance has no offset that ever aired
/// to reason about — but the aired-frame record still points at the only
/// offset any frame went out on.
#[tokio::test]
async fn two_nudges_with_no_frame_between_credit_the_offset_that_aired() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let (parity, aired_at) = reply_window_now();
    report_aired(&manager, qso_id, FREQ, aired_at).await;

    for hz in [1900.0, 2300.0] {
        manager
            .apply_tx_offset_switch(qso_id, hz, OffsetRelocationOrigin::OperatorForced)
            .await
            .unwrap();
    }

    send_report_in(&manager, parity, FREQ).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, 2300.0,
        "precondition: the QSO is on the twice-nudged offset"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(FREQ),
        "only {FREQ} Hz ever aired — the record answers regardless of how \
         many relocations stack up in between"
    );
}

/// PAN-83: the end-to-end harm the ticket's design pass exists to close.
/// After a nudge whose reply answered the old-offset frame, the old
/// offset is the one the DX demonstrably heard, so a later stall on the
/// new offset must `Revert` back to it — the operator's choice stands
/// until the new offset itself stalls (Decision 11's narrow departure
/// from PAN-132).
#[tokio::test]
async fn a_stall_after_a_nudge_whose_reply_answered_the_old_offset_reverts_to_it() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let (parity, aired_at) = reply_window_now();
    report_aired(&manager, qso_id, FREQ, aired_at).await;
    let nudged = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, nudged, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    send_report_in(&manager, parity, FREQ).await;
    let entered_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // Two silent rearms, two slots apart, so each feeds a distinct
    // own-parity TX slot (PAN-128) and the second trips the threshold.
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(15))
        .await;
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(45))
        .await;
    let action = drain(&mut rx).into_iter().find_map(|e| match e {
        QsoEvent::TxOffsetActionNeeded { action, .. } => Some(action),
        _ => None,
    });
    assert_eq!(
        action,
        Some(OffsetAction::Revert { target_hz: FREQ }),
        "the DX answered our {FREQ} Hz frame after the nudge, so {FREQ} Hz \
         is the known-good offset a stall on {nudged} Hz returns to — not a \
         fresh blind Switch away from the offset the DX heard"
    );
}

/// PAN-83: characterization. When a frame DID air on the new offset
/// before the reply, the record says so and the new offset is credited —
/// PAN-132's outcome for this timing is unchanged.
#[tokio::test]
async fn a_nudge_whose_new_offset_aired_before_the_reply_credits_the_new_offset() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let (parity, aired_at) = reply_window_now();
    report_aired(
        &manager,
        qso_id,
        FREQ,
        aired_at - SignedDuration::from_secs(30),
    )
    .await;
    let nudged = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, nudged, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    report_aired(&manager, qso_id, nudged, aired_at).await;

    send_report_in(&manager, parity, FREQ).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(nudged),
        "a frame aired at {nudged} Hz before the reply — the record names \
         it, and the credit follows"
    );
}

/// PAN-83: characterization of the chained-relocation case (PROBE2), and
/// Decision 2. The second, stall-triggered relocation was held by PAN-182
/// until its trigger frame aired at 1900 Hz; the reply inside the grace
/// window answers THAT frame, so 1900 Hz is credited — not the first
/// offset of the chain (1500), whose last frame aired three or more own
/// slots before the reply. Frame provenance never looks at the chain,
/// only at the frame that aired.
#[tokio::test]
async fn a_chained_stall_relocation_credits_the_offset_its_trigger_aired_on() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // Nudge off FREQ first: 1500 -> 1900.
    manager
        .apply_tx_offset_switch(qso_id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();

    // Two silent rearm cycles trip the stall on 1900; the second holds
    // the action on its own trigger frame (PAN-182).
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(45))
        .await;
    confirm_last_frame_aired(&manager, qso_id, 1900.0).await;

    // The drain commits the stall relocation: 1900 -> 2300.
    manager
        .apply_tx_offset_switch(qso_id, 2300.0, stall_origin(&manager, qso_id).await)
        .await
        .unwrap();

    // The wall-clock record of that same trigger frame (the PAN-182 hold
    // released on the simulated rearm clock), placed in our slot just
    // before the reply's — what the live TX worker's report would carry.
    let (parity, aired_at) = reply_window_now();
    report_aired(&manager, qso_id, 1900.0, aired_at).await;
    send_report_in(&manager, parity, FREQ).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(1900.0),
        "the reply answered the trigger frame that aired at 1900 Hz — not \
         the chain's first offset ({FREQ} Hz) and not the never-keyed \
         2300 Hz"
    );
}

/// PAN-83: characterization. A record outside the reply's window (the
/// frame aired two of our slots or more before the reply) pins nothing,
/// and the existing provenance rule decides — here, an operator nudge
/// credits where it moved us.
#[tokio::test]
async fn an_aired_frame_outside_the_reply_window_falls_back_to_provenance() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    report_aired(
        &manager,
        qso_id,
        FREQ,
        manager.now_utc() - SignedDuration::from_secs(32),
    )
    .await;
    let nudged = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, nudged, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    let (parity, _) = reply_window_now();
    send_report_in(&manager, parity, FREQ).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(nudged),
        "the only record is outside the reply's window, so the operator-\
         nudge fallback applies exactly as before PAN-83"
    );
}

/// PAN-83: characterization. A decode without a slot parity (no
/// `observed_slot_parity`) cannot anchor the reply slot, so the record
/// pins nothing and provenance decides — unchanged behaviour for every
/// caller that goes through `process_message`.
#[tokio::test]
async fn a_reply_without_a_slot_parity_falls_back_to_provenance() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let (_, aired_at) = reply_window_now();
    report_aired(&manager, qso_id, FREQ, aired_at).await;
    let nudged = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, nudged, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(nudged),
        "no decoded slot parity means no anchored reply slot — the \
         operator-nudge fallback applies exactly as before PAN-83"
    );
}

/// PAN-83: characterization (Decision 3). The Tx=Rx branch keeps the
/// decode-frequency rule and its `partner_freq` latch whatever the aired
/// record says: a station answering our CQ transmits where it clicked
/// us, so its decode frequency already names the frame it answered. This
/// branch is reachable only after a relocation on an unanswered
/// `CallingCq`.
#[tokio::test]
async fn an_unanswered_cq_still_credits_by_reply_frequency_whatever_aired() {
    const CALLER: &str = "W9XYZ";

    let manager = manager_auto(test_config());
    let qso_id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    let nudged = FREQ + 400.0;
    manager
        .apply_tx_offset_switch(qso_id, nudged, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    let (parity, aired_at) = reply_window_now();
    // The record points at the NEW offset — the Tx=Rx branch must not
    // consult it.
    report_aired(&manager, qso_id, nudged, aired_at).await;

    // The caller heard our PRE-switch CQ and answers on the offset that
    // CQ actually went out on, carrying a real decoded slot parity.
    manager
        .process_message_with_parity(
            MessageType::CqResponse {
                calling_station: OUR.into(),
                responding_station: CALLER.into(),
                grid: Some("FN31".into()),
            },
            format!("{OUR} {CALLER} FN31"),
            FREQ,
            Some(-12.0),
            Some(parity),
        )
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.state.their_callsign(),
        Some(CALLER),
        "precondition: the pre-switch answer establishes the QSO"
    );
    assert_eq!(
        after.metadata.last_known_good_offset_hz,
        Some(FREQ),
        "the caller answered at {FREQ} Hz (the vacated offset) — the \
         decode frequency is the evidence on a Tx=Rx QSO, whatever the \
         aired record says"
    );
    assert_eq!(
        after.metadata.partner_freq,
        Some(FREQ),
        "the round-12 latch still latches the caller's frequency"
    );
}

/// PAN-72 (Codex round 6 on PR #350, finding 1): `SendingReport` is two
/// rungs, not one. `RespondingToCq + CqResponse` (the DX returned our call
/// with no report) lands in `SendingReport { their_report: None }`; the
/// DX's first `SignalReport` moves us to `SendingReport { their_report:
/// Some(_) }`, turning our outbound from a plain report into an R-report.
///
/// Ranked equal, that transition read as "no progress": the partial stall
/// streak carried forward, `advance_generation` never moved, and a switch
/// request queued by the preceding threshold-hitting rearm still passed the
/// commit-time stale-action guard — relocating the QSO at the exact moment
/// the DX finally answered.
#[tokio::test]
async fn the_dx_s_first_report_advances_a_sending_report_qso() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 4;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();

    // The DX returns our call with no report yet — `SendingReport` on the
    // `their_report: None` rung.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.into(),
                responding_station: DX.into(),
                grid: Some("FN31".into()),
            },
            format!("{OUR} {DX} FN31"),
            FREQ,
            Some(-12.0),
        )
        .await
        .unwrap();
    let opened = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(
            opened.state,
            QsoState::SendingReport {
                their_report: None,
                ..
            }
        ),
        "precondition: the DX answered without a report, got {:?}",
        opened.state
    );
    let answered_at = opened.metadata.last_call_at.unwrap();

    // A silent slot: we re-send our report, the DX says nothing. Partial
    // stall streak, below the threshold.
    manager
        .rearm_manual_calls_at(answered_at + SignedDuration::from_secs(16))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "precondition: one silent slot builds a partial stall streak"
    );

    let (calls_before, generation_before) = {
        let mut qsos = manager.qsos.write().await;
        let progress = qsos.get_mut(&qso_id).unwrap();
        // Exclude the reprieve inherited from the report-free answer.
        progress.metadata.progressed_this_cycle = false;
        (
            progress.metadata.call_count,
            progress.metadata.advance_generation,
        )
    };

    // ...and NOW the DX's report lands. This is the exchange's single most
    // load-bearing advance: our outbound becomes an R-report.
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;

    let after = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(
            after.state,
            QsoState::SendingReport {
                their_report: Some(-7),
                ..
            }
        ),
        "precondition: the DX's report latched, got {:?}",
        after.state
    );
    assert_eq!(
        after.metadata.stall_cycles, 0,
        "the DX's first report is forward progress — the partial stall \
         streak must reset, not carry into the established exchange"
    );
    assert_eq!(
        after.metadata.advance_generation,
        generation_before + 1,
        "the first report must advance the generation exactly once"
    );
    assert_eq!(
        after.metadata.call_count, calls_before,
        "the first report queues no reply and must not spend a call"
    );
    assert!(
        after.metadata.progressed_this_cycle,
        "PAN-84: the first report must grant the manual watchdog reprieve"
    );
}

/// PAN-84: first acquisition protects one watchdog pass without extending
/// the original deadline. Silence on the next pass still retires the QSO.
#[tokio::test]
async fn first_dx_report_grants_exactly_one_expired_watchdog_reprieve() {
    let mut config = test_config();
    config.timeouts.manual_call_watchdog_minutes = 1;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.into(),
                responding_station: DX.into(),
                grid: Some("FN31".into()),
            },
            format!("{OUR} {DX} FN31"),
            FREQ,
            Some(-12.0),
        )
        .await
        .unwrap();
    let now = manager.now_utc();
    let first_call_at = now - SignedDuration::from_mins(2);
    {
        let mut qsos = manager.qsos.write().await;
        let progress = qsos.get_mut(&qso_id).unwrap();
        assert!(matches!(
            progress.state,
            QsoState::SendingReport {
                their_report: None,
                ..
            }
        ));
        progress.metadata.first_call_at = Some(first_call_at);
        progress.metadata.progressed_this_cycle = false;
    }
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::SendingReport {
            their_report: Some(-7),
            ..
        }
    ));
    let mut rx = manager.subscribe();

    manager.check_timeouts_at(now).await;
    let surviving = manager
        .get_qso(qso_id)
        .await
        .expect("PAN-84: the first report must protect one expired watchdog pass");
    assert!(!surviving.metadata.progressed_this_cycle);
    assert_eq!(surviving.metadata.first_call_at, Some(first_call_at));
    assert!(
        drain(&mut rx).is_empty(),
        "the reprieve must not emit a timeout"
    );

    manager
        .check_timeouts_at(now + SignedDuration::from_secs(5))
        .await;
    assert!(manager.get_qso(qso_id).await.is_err());
    assert!(
        drain(&mut rx).iter().any(|event| matches!(
            event,
            QsoEvent::QsoFailed {
                qso_id: failed_id,
                reason: QsoFailureReason::Timeout,
                ..
            } if *failed_id == qso_id
        )),
        "the next pass must retire the QSO with a timeout"
    );
}

/// The other half of finding 1: a REPEATED report (the "DX never copied our
/// R" regression arm, `Some(_) -> Some(_)`) is deliberately still not
/// progress. Splitting the `SendingReport` rung must not turn every
/// re-delivered report into a fake advance that clears real stall evidence.
#[tokio::test]
async fn a_repeated_dx_report_is_still_not_forward_progress() {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 4;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();

    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    let reported_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(reported_at + SignedDuration::from_secs(16))
        .await;
    let stalled = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        stalled.metadata.stall_cycles, 1,
        "precondition: a silent slot after the first report"
    );
    let generation_before = stalled.metadata.advance_generation;

    // Neither an identical report nor a changed value advances the QSO
    // after a report has already been latched.
    for report in [-7, -3] {
        manager
            .qsos
            .write()
            .await
            .get_mut(&qso_id)
            .unwrap()
            .metadata
            .progressed_this_cycle = true;
        send_report(&manager, report, &format!("{OUR} {DX} {report:03}")).await;

        let after = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(
            after.state,
            QsoState::SendingReport { their_report: Some(value), .. } if value == report
        ));
        assert_eq!(
            after.metadata.advance_generation, generation_before,
            "a repeated report is the DX standing still, not an advance"
        );
        assert_eq!(
            after.metadata.stall_cycles, 1,
            "the stall evidence must survive a repeated report"
        );
        assert!(
            !after.metadata.progressed_this_cycle,
            "a subsequent report must cancel the pending watchdog reprieve"
        );
    }
}

// ── PAN-128: one stall count per own-parity TX slot ──────────────────
//
// A QSO transmits only on its own parity — every OTHER protocol slot —
// so the stall counter counts the own-parity TX slots the rearm
// emissions FEED, not the emissions. Rearm times below are anchored to
// the QSO's own-parity slot grid (`next_slot_with_parity_with_period`)
// so the results never depend on the wall-clock slot phase at test
// start.

/// The first own-parity FT8 slot start strictly after `opened_at + one
/// slot` — the anchor the PAN-128 cadence tests rearm relative to.
fn next_own_ft8_slot(
    after: UtcTimestamp,
    tx_parity: pancetta_core::slot::SlotParity,
) -> UtcTimestamp {
    pancetta_core::slot::next_slot_with_parity_with_period(after, tx_parity, 15_000_000_000)
}

#[test]
fn keep_call_tx_slot_uses_the_scheduler_current_slot_acceptance() {
    use pancetta_core::slot::SlotParity;

    // 2026-01-01T00:00:00Z is timestamp 1767225600 = 15 * 117815040 —
    // an Even slot start under both the FT8 (15 s) and FT4 (7.5 s)
    // grids, so every expectation below is grid-aligned.
    let base = UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let s = SignedDuration::from_secs;

    // The FT8 acceptance cap at the 3000 ms default is
    // 3000 - 800 - 500 = 1700 ms; FT4's is 1500 - 400 - 500 = 600 ms;
    // FT2's saturates to 0 ms.
    assert_eq!(keep_call_acceptance_cap_ms(15_000, 3000), 1700);
    assert_eq!(keep_call_acceptance_cap_ms(7_500, 3000), 600);
    assert_eq!(keep_call_acceptance_cap_ms(3_200, 3000), 0);
    assert_eq!(keep_call_acceptance_cap_ms(15_000, 1000), 0);

    // FT8, off-parity or past the window: strictly the next own slot.
    assert_eq!(
        keep_call_tx_slot(base + s(5), 15_000, 3000, Some(SlotParity::Odd)),
        base + s(15)
    );
    assert_eq!(
        keep_call_tx_slot(base + s(5), 15_000, 3000, Some(SlotParity::Even)),
        base + s(30),
    );
    assert_eq!(
        keep_call_tx_slot(base + s(2), 15_000, 3000, Some(SlotParity::Even)),
        base + s(30),
        "2 s into our own slot is inside the raw 3 s late window but past \
         the 1.7 s queue-time acceptance cap — the worker defers, so must we"
    );
    // FT8, inside our own slot's acceptance window (1.7 s at the
    // 3000 ms default): the CURRENT slot, exactly where `schedule_tx`
    // keys the frame — never the next own slot.
    assert_eq!(
        keep_call_tx_slot(base, 15_000, 3000, Some(SlotParity::Even)),
        base,
        "mstr 0 is inside the window — the current own slot itself"
    );
    assert_eq!(
        keep_call_tx_slot(
            base + SignedDuration::from_millis(1_700),
            15_000,
            3000,
            Some(SlotParity::Even)
        ),
        base,
        "mstr exactly at the cap is still accepted (`<=`, like schedule_tx)"
    );
    assert_eq!(
        keep_call_tx_slot(base + s(1), 15_000, 3000, Some(SlotParity::Even)),
        base
    );
    // Exactly on a boundary of the OTHER parity: current slot not
    // viable, next own slot.
    assert_eq!(
        keep_call_tx_slot(base, 15_000, 3000, Some(SlotParity::Odd)),
        base + s(15)
    );
    // FT4's 7.5 s period: the acceptance cap scales to 600 ms. Odd
    // slots at +7.5 s/+22.5 s, Even at +15 s.
    assert_eq!(
        keep_call_tx_slot(base + s(1), 7_500, 3000, Some(SlotParity::Odd)),
        base + SignedDuration::from_millis(7_500)
    );
    assert_eq!(
        keep_call_tx_slot(
            base + SignedDuration::from_millis(500),
            7_500,
            3000,
            Some(SlotParity::Even)
        ),
        base,
        "0.5 s into an FT4 own slot is inside the scaled 600 ms cap"
    );
    assert_eq!(
        keep_call_tx_slot(base + s(1), 7_500, 3000, Some(SlotParity::Even)),
        base + s(15),
        "1 s into an FT4 own slot is past the scaled cap — defer"
    );
    // No latched parity: the next slot of either parity, still strictly
    // after `now`.
    assert_eq!(
        keep_call_tx_slot(base + s(5), 15_000, 3000, None),
        base + s(15)
    );
    assert_eq!(keep_call_tx_slot(base, 15_000, 3000, None), base + s(15));
}

/// Rearms every protocol slot starting INSIDE our own slot: the
/// in-window emission feeds the own slot it keys, and the off-parity
/// emission after it feeds the NEXT one, so the streak climbs once per
/// own TX slot ([1,2,2,...]) while `call_count` rises on every
/// emission, and the threshold trips on the emission feeding the N-th
/// distinct slot.
#[tokio::test]
async fn stall_counts_once_per_own_parity_tx_slot_when_rearms_start_in_our_slot() {
    use pancetta_core::slot::SlotParity;

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 4;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    // The DX was heard on Odd, so we latch and hold tx_parity = Even.
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(opened.metadata.tx_parity, Some(SlotParity::Even));
    let opened_at = opened.metadata.last_call_at.unwrap();
    let _ = drain(&mut rx);

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    let mut first_action_on_rearm = None;
    for k in 0..8i64 {
        manager
            .rearm_manual_calls_at(x + SignedDuration::from_secs(1 + 15 * k))
            .await;
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles as i64,
            [1, 2, 2, 3, 3, 4, 4, 5][k as usize],
            "rearm {} must count once per own-parity TX slot it feeds",
            k + 1
        );
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.call_count,
            2 + k as u32,
            "the keep-call still goes out on every protocol slot"
        );
        if first_action_on_rearm.is_none()
            && drain(&mut rx)
                .iter()
                .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. }))
        {
            first_action_on_rearm = Some(k + 1);
        }
    }
    assert_eq!(
        first_action_on_rearm,
        Some(6),
        "threshold 4 must trip on the emission feeding the 4th distinct \
         own-parity TX slot — the 6th rearm, not the 4th"
    );
}

/// The mirror start: the first rearm lands in the DX's (off-parity)
/// slot, and the in-window emission after it feeds the SAME own slot —
/// every own slot in the window still counts exactly once.
#[tokio::test]
async fn stall_counts_once_per_own_parity_tx_slot_when_rearms_start_in_their_slot() {
    use pancetta_core::slot::SlotParity;

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 4;
    let manager = manager_auto(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // x+16 s lands in the off-parity slot before the own slot x+30 —
    // and is always past the one-slot rearm gate relative to opened_at.
    let mut first_action_on_rearm = None;
    for k in 0..8i64 {
        manager
            .rearm_manual_calls_at(x + SignedDuration::from_secs(16 + 15 * k))
            .await;
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles as i64,
            [1, 1, 2, 2, 3, 3, 4, 4][k as usize],
            "rearm {} must count once per own-parity TX slot it feeds",
            k + 1
        );
        if first_action_on_rearm.is_none()
            && drain(&mut rx)
                .iter()
                .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. }))
        {
            first_action_on_rearm = Some(k + 1);
        }
    }
    assert_eq!(
        first_action_on_rearm,
        Some(7),
        "threshold 4 must trip on the 7th rearm — the one feeding the \
         4th distinct own-parity TX slot"
    );
}

/// PAN-128 PR review follow-up: the ledger slot must use the TX
/// scheduler's own current-slot acceptance rule (`schedule_tx` picks
/// the current slot when its parity matches and the request is still
/// inside the `tx_late_max_ms` window), not an unconditional
/// "strictly next own slot". A rearm in the off-parity slot before
/// own slot S and a rearm inside S's late-start window target the
/// SAME real TX opportunity, so they must share one ledger key and
/// count once — otherwise the switch fires one transmission before
/// the configured threshold.
#[tokio::test]
async fn in_window_rearm_shares_its_slot_key_with_the_off_parity_predecessor() {
    use pancetta_core::slot::SlotParity;

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // A, one second into the off-parity slot before the own slot
    // x+30: deferred by the scheduler, so it feeds x+30.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(16))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "precondition: the off-parity predecessor counts the own slot \
         it feeds"
    );
    // B, one second INTO x+30 — inside the default 3 s
    // `tx_late_max_ms` window, exactly where `schedule_tx` targets
    // the CURRENT slot. Same real TX opportunity as A.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(31))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "an in-window rearm targets the same real TX slot as its \
         off-parity predecessor and must share its ledger key"
    );
}

/// Codex round 2 on PR #477: the ledger must use the worker's
/// QUEUE-TIME acceptance cap (1.7 s for FT8 at the 3000 ms default),
/// not the raw 3 s late window. A rearm 2 s into own slot S is deferred
/// by the worker to S's successor, which its off-parity successor also
/// feeds — so the pair is one real TX opportunity and must count once,
/// even as the first emission after the QSO opened.
#[tokio::test]
async fn rearm_past_the_acceptance_cap_shares_its_deferred_slot_with_the_next_rearm() {
    use pancetta_core::slot::SlotParity;

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // A, 2 s into own slot x: inside the raw 3 s window but past the
    // 1.7 s acceptance cap, so the worker defers it to x+30.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(2))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "precondition: the first silent rearm counts the slot it feeds"
    );
    // B, in the off-parity slot before x+30: the same deferred slot.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(17))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "a deferred own-slot rearm and its off-parity successor feed one \
         real TX slot and must share its ledger key"
    );
}

// ── PAN-223 ──
// The rearm's `keep_call_tx_slot` key is a prediction; the TX worker
// picks the real slot from the instant it dequeues the frame and reports
// it (`TxFrameScheduled` → `record_scheduled_tx_slot`). An emission
// inside own slot S's 1.7 s acceptance cap that the worker dequeues past
// the cap airs in S+2 (x+30 below), the slot the next off-parity rearm
// feeds too: one real TX opportunity, one count, in either report order.
// Times are anchored on the own-slot grid (PAN-239).

/// Threshold 2, a manual QSO latched to the Even TX parity, and `x`, the
/// first own slot a full protocol slot after it opened.
async fn pan223_open(threshold: u32) -> (QsoManager, QsoId, UtcTimestamp) {
    use pancetta_core::slot::SlotParity;

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = threshold;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    (manager, qso_id, x)
}

async fn pan223_stall(manager: &QsoManager, qso_id: QsoId) -> u32 {
    manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles
}

fn pan223_last_attempt(manager: &QsoManager, qso_id: QsoId) -> TxAttemptId {
    manager.keep_call_attempt_ids(qso_id).last().unwrap().0
}

async fn pan223_last_text(manager: &QsoManager, qso_id: QsoId) -> String {
    manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone()
}

#[tokio::test]
async fn pan223_in_cap_rearm_the_worker_deferred_counts_once_with_its_off_parity_successor() {
    use pancetta_core::slot::SlotParity;

    let (manager, qso_id, x) = pan223_open(2).await;
    let mut rx = manager.subscribe();
    let ms = SignedDuration::from_millis;

    // A, 1.6 s into own slot x: inside the 1.7 s cap, so the rearm
    // predicts x.
    manager.rearm_manual_calls_at(x + ms(1_600)).await;
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);
    let a = pan223_last_attempt(&manager, qso_id);
    // The worker dequeues it 200 ms later, past the cap: it defers the
    // frame to the next own slot.
    assert_eq!(
        keep_call_tx_slot(x + ms(1_800), 15_000, 3000, Some(SlotParity::Even)),
        x + SignedDuration::from_secs(30),
        "precondition: a dequeue past the cap targets the next own slot"
    );
    assert_eq!(
        manager
            .record_scheduled_tx_slot(qso_id, a, x + SignedDuration::from_secs(30))
            .await,
        ScheduledTxSlot::CountMoved
    );

    // B, the off-parity rearm before x+30: the same real TX slot.
    manager.rearm_manual_calls_at(x + ms(16_600)).await;
    let b = pan223_last_attempt(&manager, qso_id);
    assert_eq!(
        pan223_stall(&manager, qso_id).await,
        1,
        "one real TX opportunity (x+30) must count once"
    );
    assert!(
        !drain(&mut rx)
            .iter()
            .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })),
        "threshold 2 must not trip after one real transmission"
    );
    assert_eq!(
        manager.keep_call_attempt_slots(qso_id),
        vec![
            (a, true, Some(x + SignedDuration::from_secs(30))),
            (b, false, Some(x + SignedDuration::from_secs(30))),
        ]
    );
}

#[tokio::test]
async fn pan223_schedule_report_after_the_successor_removes_the_duplicate_count() {
    let (manager, qso_id, x) = pan223_open(2).await;
    let mut rx = manager.subscribe();
    let ms = SignedDuration::from_millis;

    manager.rearm_manual_calls_at(x + ms(1_600)).await;
    let a = pan223_last_attempt(&manager, qso_id);
    manager.rearm_manual_calls_at(x + ms(16_600)).await;
    let b = pan223_last_attempt(&manager, qso_id);
    assert_eq!(
        pan223_stall(&manager, qso_id).await,
        2,
        "precondition: before the report the prediction double-counts"
    );
    assert!(drain(&mut rx)
        .iter()
        .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. })));
    let raised_at_generation = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .advance_generation;

    assert_eq!(
        manager
            .record_scheduled_tx_slot(qso_id, a, x + SignedDuration::from_secs(30))
            .await,
        ScheduledTxSlot::DuplicateCountRemoved { stall_cycles: 1 }
    );
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);
    assert_eq!(
        manager.keep_call_attempt_slots(qso_id),
        vec![
            (a, false, Some(x + SignedDuration::from_secs(30))),
            (b, true, Some(x + SignedDuration::from_secs(30))),
        ]
    );
    let result = manager
        .apply_tx_offset_switch(
            qso_id,
            FREQ + 200.0,
            OffsetRelocationOrigin::StallDetected {
                raised_at_generation,
            },
        )
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionStallRolledBack { .. })
        ),
        "the action raised on the duplicate count is revoked, got {result:?}"
    );
}

#[tokio::test]
async fn pan223_ledger_slot_is_the_worker_target_slot() {
    let (manager, qso_id, x) = pan223_open(2).await;
    let s = SignedDuration::from_secs;

    manager
        .rearm_manual_calls_at(x + SignedDuration::from_millis(1_600))
        .await;
    let a = pan223_last_attempt(&manager, qso_id);
    assert_eq!(manager.keep_call_attempt_slots(qso_id)[0].2, Some(x));

    manager.record_scheduled_tx_slot(qso_id, a, x + s(30)).await;
    assert_eq!(
        manager.keep_call_attempt_slots(qso_id)[0].2,
        Some(x + s(30)),
        "the ledger slot is the slot the worker scheduled"
    );
    // A supersede re-schedules the frame: the newest report wins.
    assert_eq!(
        manager.record_scheduled_tx_slot(qso_id, a, x + s(60)).await,
        ScheduledTxSlot::CountMoved
    );
    assert_eq!(
        manager.keep_call_attempt_slots(qso_id)[0].2,
        Some(x + s(60))
    );
    assert_eq!(
        manager.record_scheduled_tx_slot(qso_id, a, x + s(60)).await,
        ScheduledTxSlot::Unchanged
    );
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);
}

#[tokio::test]
async fn pan223_matching_prediction_and_unknown_ids_change_nothing() {
    let (manager, qso_id, x) = pan223_open(2).await;
    let s = SignedDuration::from_secs;

    manager.rearm_manual_calls_at(x + s(16)).await;
    let p = pan223_last_attempt(&manager, qso_id);
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);
    assert_eq!(
        manager.record_scheduled_tx_slot(qso_id, p, x + s(30)).await,
        ScheduledTxSlot::Unchanged
    );
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);
    assert_eq!(
        manager
            .record_scheduled_tx_slot(qso_id, TxAttemptId(u64::MAX), x + s(60))
            .await,
        ScheduledTxSlot::NoMatchingAttempt,
        "an unknown id never falls back to content matching"
    );

    let text = pan223_last_text(&manager, qso_id).await;
    assert!(matches!(
        manager
            .roll_back_replaced_attempt_at(qso_id, &text, None, Some(p), x + s(20))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.record_scheduled_tx_slot(qso_id, p, x + s(60)).await,
        ScheduledTxSlot::NoMatchingAttempt
    );
    assert_eq!(pan223_stall(&manager, qso_id).await, 0);

    assert_eq!(
        manager
            .record_scheduled_tx_slot(Uuid::new_v4(), p, x + s(60))
            .await,
        ScheduledTxSlot::QsoNotActive
    );
}

#[tokio::test]
async fn pan223_unslotted_and_reset_retired_attempts_are_never_rekeyed() {
    let (manager, qso_id, x) = pan223_open(2).await;
    manager.resend_last_tx(qso_id).await.unwrap();
    let resend = pan223_last_attempt(&manager, qso_id);
    assert_eq!(
        manager
            .record_scheduled_tx_slot(qso_id, resend, x + SignedDuration::from_secs(30))
            .await,
        ScheduledTxSlot::NoSlotKey
    );
    assert_eq!(pan223_stall(&manager, qso_id).await, 0);

    let manager = pan182_threshold_one();
    let (qso_id, rearm_at, _) = pan182_raise(&manager).await;
    let pre_reset = pan223_last_attempt(&manager, qso_id);
    confirm_last_frame_aired(&manager, qso_id, FREQ).await;
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("a released stall switch commits and resets the streak");
    assert_eq!(pan223_stall(&manager, qso_id).await, 0);
    assert_eq!(
        manager
            .record_scheduled_tx_slot(qso_id, pre_reset, rearm_at + SignedDuration::from_secs(30))
            .await,
        ScheduledTxSlot::NoSlotKey,
        "a stall reset retires the slot key with the count"
    );
    assert_eq!(pan223_stall(&manager, qso_id).await, 0);
}

#[tokio::test]
async fn pan223_uncounted_rekey_never_adds_a_count() {
    let (manager, qso_id, x) = pan223_open(2).await;
    let s = SignedDuration::from_secs;

    // P (off-parity) holds x+30's count; A, 1 s into x+30, shares it.
    manager.rearm_manual_calls_at(x + s(16)).await;
    manager.rearm_manual_calls_at(x + s(31)).await;
    let a = pan223_last_attempt(&manager, qso_id);
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);

    assert_eq!(
        manager.record_scheduled_tx_slot(qso_id, a, x + s(60)).await,
        ScheduledTxSlot::RekeyedUncounted
    );
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);
    // The next rearm feeding x+60 counts it: two real slots, two counts.
    manager.rearm_manual_calls_at(x + s(46)).await;
    assert_eq!(pan223_stall(&manager, qso_id).await, 2);
}

#[tokio::test]
async fn pan223_a_surviving_same_slot_sibling_keeps_the_old_slot_count() {
    let (manager, qso_id, x) = pan223_open(2).await;
    let s = SignedDuration::from_secs;

    manager.rearm_manual_calls_at(x + s(16)).await;
    let p = pan223_last_attempt(&manager, qso_id);
    manager.rearm_manual_calls_at(x + s(31)).await;
    let a = pan223_last_attempt(&manager, qso_id);

    assert_eq!(
        manager.record_scheduled_tx_slot(qso_id, p, x + s(60)).await,
        ScheduledTxSlot::CountLeftWithSibling
    );
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);
    assert_eq!(
        manager.keep_call_attempt_slots(qso_id),
        vec![(p, false, Some(x + s(60))), (a, true, Some(x + s(30)))]
    );
}

#[tokio::test]
async fn pan223_a_held_trigger_follows_its_attempt_to_the_worker_slot() {
    let (manager, qso_id, x) = pan223_open(1).await;
    let s = SignedDuration::from_secs;

    manager
        .rearm_manual_calls_at(x + SignedDuration::from_millis(1_600))
        .await;
    assert_eq!(pan223_stall(&manager, qso_id).await, 1);
    let a = pan223_last_attempt(&manager, qso_id);
    let text = pan223_last_text(&manager, qso_id).await;
    assert_eq!(
        manager.record_scheduled_tx_slot(qso_id, a, x + s(30)).await,
        ScheduledTxSlot::CountMoved
    );

    let aired = UnsentAttemptPayload {
        frequency_hz: FREQ,
        remote_binding: Some((false, None)),
    };
    assert!(
        !manager
            .confirm_stall_trigger_aired(qso_id, &text, &aired, x + s(13))
            .await,
        "a frame that aired in slot x is not the trigger the worker put in x+30"
    );
    assert!(
        manager
            .confirm_stall_trigger_aired(qso_id, &text, &aired, x + s(43))
            .await,
        "the trigger's own x+30 frame releases the hold"
    );
}

#[tokio::test]
async fn pan223_removing_a_trigger_duplicate_moves_the_hold_to_the_slot_holder() {
    let (manager, qso_id, x) = pan223_open(1).await;
    let ms = SignedDuration::from_millis;

    manager.rearm_manual_calls_at(x + ms(1_600)).await;
    let a = pan223_last_attempt(&manager, qso_id);
    let text = pan223_last_text(&manager, qso_id).await;
    manager.rearm_manual_calls_at(x + ms(16_600)).await;
    assert_eq!(pan223_stall(&manager, qso_id).await, 2);

    assert_eq!(
        manager
            .record_scheduled_tx_slot(qso_id, a, x + SignedDuration::from_secs(30))
            .await,
        ScheduledTxSlot::DuplicateCountRemoved { stall_cycles: 1 }
    );
    assert!(matches!(
        manager
            .roll_back_replaced_attempt_at(qso_id, &text, None, Some(a), x + ms(20_000))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    let result = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionTriggerOutcomePending { .. })
        ),
        "the hold moved to the x+30 slot holder, which has not aired: got {result:?}"
    );
}

/// FT4's 7.5 s slot: own-parity TX slots come every 15 s, so two
/// protocol-slot-apart rearms can share one, and the counter must
/// still climb once per own FT4 slot.
#[tokio::test]
async fn ft4_stall_counts_once_per_own_parity_ft4_slot() {
    use pancetta_core::slot::{next_slot_with_parity_with_period, SlotParity};

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    config.active_mode = "FT4".to_string();
    let mut manager = manager_auto(config);
    manager.set_active_slot_ns_source(Arc::new(std::sync::atomic::AtomicI64::new(7_500_000_000)));
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    let x = next_slot_with_parity_with_period(
        opened_at + SignedDuration::from_millis(7_500),
        SlotParity::Even,
        7_500_000_000,
    );
    // 1 s into an own FT4 slot is past FT4's 600 ms acceptance cap, so
    // the own-slot rearm and the off-parity one after it both feed the
    // NEXT own slot.
    let mut switched = false;
    for k in 0..3i64 {
        manager
            .rearm_manual_calls_at(x + SignedDuration::from_millis(1_000 + 7_500 * k))
            .await;
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles as i64,
            [1, 1, 2][k as usize],
            "FT4 rearm {} must count once per own-parity FT4 TX slot",
            k + 1
        );
        switched |= drain(&mut rx)
            .iter()
            .any(|e| matches!(e, QsoEvent::TxOffsetActionNeeded { .. }));
    }
    assert!(
        switched,
        "threshold 2 must trip on the rearm feeding the 2nd distinct \
         own-parity FT4 slot"
    );
}

/// Two rearms feeding ONE own-parity TX slot: rolling back the
/// uncounted twin (the newest, which newest-first matching picks) must
/// leave the slot's count with the counted attempt; only rolling back
/// the counted one takes the slot's count away.
#[tokio::test]
async fn rolling_back_the_uncounted_twin_keeps_the_stall_count() {
    use pancetta_core::slot::SlotParity;

    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // Both emissions feed the own-parity slot at x+30: A from the
    // off-parity slot before it, B from inside its late-start window.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(16))
        .await;
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(31))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "two rearms feeding one own-parity TX slot count once"
    );
    let text = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();

    // Newest-first matching removes the uncounted twin.
    assert!(matches!(
        manager
            .roll_back_unsent_attempt_at(qso_id, &text, x + SignedDuration::from_secs(35))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "the slot keeps its count while its counted attempt survives"
    );

    // Now the counted attempt itself: no same-slot sibling survives.
    assert!(matches!(
        manager
            .roll_back_unsent_attempt_at(qso_id, &text, x + SignedDuration::from_secs(36))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "with no surviving attempt for the slot, the count goes too"
    );
}

/// PAN-128 D2 (the heir rule): a payload-scoped rollback that removes
/// the COUNTED attempt while an uncounted same-slot twin survives hands
/// the slot's count to that twin instead of decrementing.
#[tokio::test]
async fn rolling_back_the_counted_attempt_hands_its_count_to_a_surviving_twin() {
    use pancetta_core::slot::SlotParity;

    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // Emission A (at FREQ, from the off-parity slot before x+30) is
    // counted; then a TX pivot changes only the offset (no commit, so
    // no streak reset); emission B (at FREQ+100, inside x+30's
    // late-start window) feeds the SAME own-parity slot and stays
    // uncounted.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(16))
        .await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&qso_id)
        .unwrap()
        .metadata
        .frequency = FREQ + 100.0;
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(31))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1
    );
    let text = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();

    // The replaced frame is the one carrying FREQ — attempt A, the
    // counted one. Its count moves to twin B, not to zero.
    let replaced = UnsentAttemptPayload {
        frequency_hz: FREQ,
        remote_binding: Some((false, None)),
    };
    assert!(matches!(
        manager
            .roll_back_replaced_attempt_at(
                qso_id,
                &text,
                Some(&replaced),
                None,
                x + SignedDuration::from_secs(35)
            )
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "a surviving same-slot twin inherits the slot's stall count"
    );

    // Rolling back the twin too (it carried FREQ+100) finally drops it.
    let aired = UnsentAttemptPayload {
        frequency_hz: FREQ + 100.0,
        remote_binding: Some((false, None)),
    };
    assert!(matches!(
        manager
            .roll_back_replaced_attempt_at(
                qso_id,
                &text,
                Some(&aired),
                None,
                x + SignedDuration::from_secs(36)
            )
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0
    );
}

/// A slot whose only attempt was rolled back counts again when a later
/// rearm feeds it — the count belongs to the slot, held by whatever
/// surviving attempt feeds it.
#[tokio::test]
async fn a_rolled_back_slot_counts_again_when_a_later_rearm_feeds_it() {
    use pancetta_core::slot::SlotParity;

    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(16))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1
    );
    let text = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();
    assert!(matches!(
        manager
            .roll_back_unsent_attempt_at(qso_id, &text, x + SignedDuration::from_secs(20))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0
    );

    // One protocol slot later, inside the late-start window of the
    // same own-parity slot at x+30 the rolled-back attempt fed.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(31))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "a slot whose attempt was fully rolled back counts again when a \
         later rearm feeds it"
    );
}

/// PAN-128: a stall reset must clear each attempt's slot key along
/// with its increment, so the heir rule can never hand a post-reset
/// count to a pre-reset attempt. The first post-reset rearm, rolled
/// back unsent, must DECREMENT — not park its count on a cleared
/// attempt that happens to feed the same own-parity TX slot.
#[tokio::test]
async fn heir_does_not_cross_a_committed_switch_reset() {
    use pancetta_core::slot::SlotParity;

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // Feeds the own-parity slot at x (inside its late-start window) …
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(1))
        .await;
    // … and P, in the off-parity slot before x+60, feeds x+60 and
    // trips threshold 2.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(46))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2
    );
    // The commit (once P aired, PAN-182) resets the streak AND must
    // retire P's slot key.
    confirm_last_frame_aired(&manager, qso_id, FREQ).await;
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("commit");
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0
    );
    // R, inside x+60's late-start window, feeds the same slot P fed
    // and is the only COUNTED attempt since the reset.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(61))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1
    );
    let text = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();
    assert!(matches!(
        manager
            .roll_back_unsent_attempt_at(qso_id, &text, x + SignedDuration::from_secs(62))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0,
        "a pre-reset attempt must not inherit a post-reset count"
    );
}

// PAN-129: a DX stuck repeating its report.
//
// While a manual QSO sits in `SendingReport` and the DX keeps repeating
// its report, the R re-send — and with it the stall count — comes from
// the rearm, which `timeout_check_loop` runs every 5 s at a phase that
// has nothing to do with the slot grid. A repeat queues no reply, so it
// must leave that rearm running exactly as it does while the DX is
// silent. The helper below drives the real manager through both
// schedules — the DX's decodes and the rearm ticks — merged in time
// order on a synthetic timeline anchored to the slot grid, so the result
// never depends on the wall-clock phase at test start.

/// The DX repeats (or silent DX-parity slots) each schedule covers.
const PAN129_REPEATS: i64 = 12;
/// `timeout_check_loop`'s rearm interval.
const PAN129_TICK_MS: i64 = 5_000;

/// What one keep-call schedule produced.
struct KeepCallSchedule {
    stall_cycles: u32,
    /// `MessageToSend` emissions for the QSO.
    emissions: usize,
    /// The own-parity TX slots those emissions feed.
    fed_tx_slots: std::collections::BTreeSet<UtcTimestamp>,
    /// DX repeats decoded before the first `TxOffsetActionNeeded`.
    repeats_before_first_action: Option<usize>,
}

/// Run a manual `SendingReport { their_report: Some }` QSO in Auto TX-freq
/// mode through [`PAN129_REPEATS`] DX slot pairs. With `dx_repeats`, the
/// DX repeats its identical report in each of its own slots, decoded
/// `decode_offset_ms` after that slot ends; otherwise it stays silent.
/// The rearm ticks every [`PAN129_TICK_MS`], starting `tick_phase_ms`
/// into the first DX slot. A decode due at the same instant as a tick is
/// processed first.
async fn run_keep_call_schedule(
    slot_ns: i64,
    decode_offset_ms: i64,
    tick_phase_ms: i64,
    dx_repeats: bool,
) -> KeepCallSchedule {
    use pancetta_core::slot::{next_slot_with_parity_with_period, SlotParity};

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 4;
    config.timeouts.manual_call_max_calls = 1_000;
    let mut manager = manager_auto(config);
    manager.set_active_slot_ns_source(Arc::new(std::sync::atomic::AtomicI64::new(slot_ns)));
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Even))
        .await
        .unwrap();
    send_report(&manager, -7, "K5ARH K9ZZ -07").await;
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(
            progress.state,
            QsoState::SendingReport {
                their_report: Some(_),
                ..
            }
        ),
        "precondition: SendingReport with the DX's report, got {:?}",
        progress.state
    );
    let tx_parity = progress.metadata.tx_parity.expect("tx_parity latched");
    let dx_parity = tx_parity.opposite();

    let slot_ms = slot_ns / 1_000_000;
    let anchor = next_slot_with_parity_with_period(
        manager.now_utc() + SignedDuration::from_secs(120),
        dx_parity,
        slot_ns,
    );
    // Repeat k goes out in the DX slot starting 2k slots after the
    // anchor and decodes `decode_offset_ms` after that slot ends.
    let decodes: Vec<UtcTimestamp> = (0..PAN129_REPEATS)
        .map(|k| anchor + SignedDuration::from_millis((2 * k + 1) * slot_ms + decode_offset_ms))
        .collect();
    let end = anchor + SignedDuration::from_millis(2 * PAN129_REPEATS * slot_ms);

    let mut rx = manager.subscribe();
    let mut outcome = KeepCallSchedule {
        stall_cycles: 0,
        emissions: 0,
        fed_tx_slots: std::collections::BTreeSet::new(),
        repeats_before_first_action: None,
    };
    let mut decoded = 0usize;
    let mut next_tick = anchor + SignedDuration::from_millis(tick_phase_ms);
    loop {
        let next_decode = decodes.get(decoded).copied().filter(|_| dx_repeats);
        let tick_due = next_tick < end;
        let at = match next_decode {
            Some(decode_at) if !tick_due || decode_at <= next_tick => {
                let before = manager.get_qso(qso_id).await.unwrap().metadata.last_call_at;
                send_report(&manager, -7, "K5ARH K9ZZ -07").await;
                // `process_message` stamps the wall clock. Move any stamp
                // it made onto the synthetic timeline (the decode
                // instant), where it holds the rearm back exactly as it
                // would on the air.
                if let Some(progress) = manager.qsos.write().await.get_mut(&qso_id) {
                    if progress.metadata.last_call_at != before {
                        progress.metadata.last_call_at = Some(decode_at);
                    }
                }
                decoded += 1;
                decode_at
            }
            _ if tick_due => {
                let tick_at = next_tick;
                manager.rearm_manual_calls_at(tick_at).await;
                next_tick += SignedDuration::from_millis(PAN129_TICK_MS);
                tick_at
            }
            _ => break,
        };
        for event in drain(&mut rx) {
            match event {
                QsoEvent::MessageToSend { qso_id: id, .. } if id == qso_id => {
                    outcome.emissions += 1;
                    outcome
                        .fed_tx_slots
                        .insert(next_slot_with_parity_with_period(at, tx_parity, slot_ns));
                }
                QsoEvent::TxOffsetActionNeeded { qso_id: id, .. } if id == qso_id => {
                    outcome.repeats_before_first_action.get_or_insert(decoded);
                }
                _ => {}
            }
        }
    }
    outcome.stall_cycles = manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles;
    outcome
}

/// PAN-129: a DX repeating its report must leave keep-calling exactly as
/// a silent DX does — same stall count, same own-parity TX slots fed,
/// same number of re-sends — and the stall switch must fire by the 4th
/// repeat (threshold 4). Swept over FT8/FT4/FT2, an early and a late
/// decode, and every 250 ms rearm phase. Before the fix each repeat
/// restamped `last_call_at` at its decode instant, holding the rearm back
/// a full slot: FT8/FT4 lost half their re-sends, and FT2 skipped TX
/// slots and switched as late as the 11th repeat.
#[tokio::test]
async fn repeating_dx_trips_the_stall_switch_on_the_silence_cadence() {
    for (mode, slot_ns) in [
        ("FT8", 15_000_000_000i64),
        ("FT4", 7_500_000_000),
        ("FT2", 3_200_000_000),
    ] {
        for decode_offset_ms in [-1_000i64, 500] {
            for tick_phase_ms in (0..PAN129_TICK_MS).step_by(250) {
                let case = format!(
                    "{mode}, decode {decode_offset_ms:+} ms from slot end, \
                     rearm phase {tick_phase_ms} ms"
                );
                let silent =
                    run_keep_call_schedule(slot_ns, decode_offset_ms, tick_phase_ms, false).await;
                let repeating =
                    run_keep_call_schedule(slot_ns, decode_offset_ms, tick_phase_ms, true).await;
                assert_eq!(
                    repeating.stall_cycles, silent.stall_cycles,
                    "{case}: a repeating DX must count stalls like a silent one"
                );
                assert_eq!(
                    repeating.fed_tx_slots, silent.fed_tx_slots,
                    "{case}: a repeating DX must feed the same own TX slots as a silent one"
                );
                assert_eq!(
                    repeating.emissions, silent.emissions,
                    "{case}: a repeating DX must keep the silence re-send cadence"
                );
                let first_action = repeating
                    .repeats_before_first_action
                    .unwrap_or_else(|| panic!("{case}: the stall switch never fired"));
                assert!(
                    first_action <= 4,
                    "{case}: the stall switch must fire by the 4th repeat, \
                     fired after {first_action}"
                );
            }
        }
    }
}

// PAN-182: a stall offset switch is held until the TX worker reports
// the frame that raised it — aired (`confirm_stall_trigger_aired`) or
// unsent (the PAN-75 rollback) — so a commit can no longer win the race
// against a late not-sent report and strand the QSO on an offset chosen
// from a frame that never aired.

/// Threshold 1, one silent rearm: the rearm's emission raises the
/// action and becomes its pending trigger. Returns the QSO, the
/// rearm's timestamp and the trigger frame's text.
async fn pan182_raise(manager: &QsoManager) -> (QsoId, UtcTimestamp, String) {
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let rearm_at = opened_at + SignedDuration::from_secs(15);
    manager.rearm_manual_calls_at(rearm_at).await;
    let qso = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        qso.metadata.stall_cycles, 1,
        "precondition: the rearm tripped threshold 1"
    );
    (
        qso_id,
        rearm_at,
        qso.messages.last().unwrap().raw_text.clone(),
    )
}

fn pan182_threshold_one() -> QsoManager {
    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 1;
    manager_auto(config)
}

fn aired_on(frequency_hz: f64) -> UnsentAttemptPayload {
    UnsentAttemptPayload {
        frequency_hz,
        remote_binding: Some((false, None)),
    }
}

#[tokio::test]
async fn pan182_stall_switch_refuses_to_commit_while_its_trigger_frame_is_unresolved() {
    let manager = pan182_threshold_one();
    let (qso_id, _, _) = pan182_raise(&manager).await;

    let result = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionTriggerOutcomePending { .. })
        ),
        "a stall switch must wait for its trigger frame's outcome, got {result:?}"
    );
    assert!(result.unwrap_err().is_expected_offset_action_refusal());
    let qso = manager.get_qso(qso_id).await.unwrap();
    assert!((qso.metadata.frequency - FREQ).abs() < f64::EPSILON);
    assert_eq!(
        qso.metadata.stall_cycles, 1,
        "the held action keeps its streak"
    );
}

#[tokio::test]
async fn pan182_aired_stall_trigger_releases_its_switch() {
    let manager = pan182_threshold_one();
    let (qso_id, _, text) = pan182_raise(&manager).await;

    assert!(
        manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await,
        "the trigger frame airing releases the hold"
    );
    let applied = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("a released stall switch commits");
    let qso = manager.get_qso(qso_id).await.unwrap();
    assert!((qso.metadata.frequency - applied).abs() < f64::EPSILON);
    assert!((applied - (FREQ + 300.0)).abs() < f64::EPSILON);
    assert_eq!(qso.metadata.stall_cycles, 0);
}

#[tokio::test]
async fn pan182_unsent_trigger_roll_back_still_revokes_and_keeps_the_qso_on_the_old_offset() {
    let manager = pan182_threshold_one();
    let (qso_id, rearm_at, text) = pan182_raise(&manager).await;

    assert!(matches!(
        manager
            .roll_back_unsent_attempt_at(qso_id, &text, rearm_at + SignedDuration::from_secs(1))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    let result = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionStallRolledBack { .. })
        ),
        "the rollback clears the trigger, so the revocation decides: got {result:?}"
    );
    let qso = manager.get_qso(qso_id).await.unwrap();
    assert!((qso.metadata.frequency - FREQ).abs() < f64::EPSILON);
    assert_eq!(qso.metadata.stall_cycles, 0);
    assert!(
        !manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await,
        "no trigger is left to release"
    );
}

#[tokio::test]
async fn pan182_trigger_payload_mismatch_does_not_release_the_hold() {
    let manager = pan182_threshold_one();
    let (qso_id, _, text) = pan182_raise(&manager).await;

    // A pivot aired the same text at another offset, or under another
    // binding: neither is the trigger frame.
    assert!(
        !manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ + 300.0),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await
    );
    assert!(
        !manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &UnsentAttemptPayload {
                    frequency_hz: FREQ,
                    remote_binding: Some((true, Some("peer".into()))),
                },
                trigger_slot_played_out(&manager, qso_id),
            )
            .await
    );
    assert!(
        !manager
            .confirm_stall_trigger_aired(
                qso_id,
                "CQ K5ARH EM12",
                &aired_on(FREQ),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await
    );
    let result = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(matches!(
        result,
        Err(QsoManagerError::OffsetActionTriggerOutcomePending { .. })
    ));
}

#[tokio::test]
async fn pan182_aged_unresolved_trigger_expires_instead_of_committing() {
    let manager = pan182_threshold_one();
    let (qso_id, _, _) = pan182_raise(&manager).await;
    let slot_millis = manager.rearm_slot_millis();
    manager
        .keep_call_ledger
        .lock()
        .unwrap()
        .get_mut(&qso_id)
        .and_then(|entry| entry.pending_stall_trigger.as_mut())
        .expect("the raise recorded a trigger")
        .emitted_at = manager.now_utc() - SignedDuration::from_millis(5 * slot_millis);

    let result = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionTriggerOutcomeExpired { .. })
        ),
        "an unreported trigger expires to a drop, never a commit: got {result:?}"
    );
    assert!(result.unwrap_err().is_expected_offset_action_refusal());
    let qso = manager.get_qso(qso_id).await.unwrap();
    assert!((qso.metadata.frequency - FREQ).abs() < f64::EPSILON);
    assert_eq!(
        qso.metadata.stall_cycles, 1,
        "the streak survives, so the next raise re-arms the hold"
    );
}

#[test]
fn pan182_stall_trigger_hold_expires_at_four_slots() {
    let emitted_at = pancetta_core::time::slot_grid_origin();
    let slot = 15_000;
    assert!(!stall_trigger_hold_expired(
        emitted_at,
        emitted_at + SignedDuration::from_millis(4 * slot - 1),
        slot
    ));
    assert!(stall_trigger_hold_expired(
        emitted_at,
        emitted_at + SignedDuration::from_millis(4 * slot),
        slot
    ));
}

/// A re-raise only takes over a hold that has expired; the expired one
/// is replaced by the re-raising emission, which a report then resolves.
#[tokio::test]
async fn pan182_a_re_raise_re_arms_an_expired_hold() {
    let manager = pan182_threshold_one();
    let (qso_id, rearm_at, text) = pan182_raise(&manager).await;
    let slot_millis = manager.rearm_slot_millis();

    // Still fresh: a later re-raise leaves the threshold frame's hold.
    let fresh = rearm_at + SignedDuration::from_millis(slot_millis);
    manager.rearm_manual_calls_at(fresh).await;
    let trigger_at = |manager: &QsoManager| {
        manager
            .keep_call_ledger
            .lock()
            .unwrap()
            .get(&qso_id)
            .and_then(|entry| entry.pending_stall_trigger.as_ref())
            .map(|trigger| trigger.emitted_at)
    };
    assert_eq!(trigger_at(&manager), Some(rearm_at));

    // Expired by the next re-raise: it takes over.
    let late = rearm_at + SignedDuration::from_millis(4 * slot_millis);
    manager.rearm_manual_calls_at(late).await;
    assert_eq!(trigger_at(&manager), Some(late));
    assert!(
        manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await
    );
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("the re-armed hold released by its own frame airing");
}

#[tokio::test]
async fn pan182_commit_and_streak_resets_clear_the_pending_trigger() {
    // A committed switch clears it.
    let manager = pan182_threshold_one();
    let (qso_id, _, text) = pan182_raise(&manager).await;
    assert!(
        manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await
    );
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("released switch commits");
    assert!(
        !manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await,
        "nothing is held after the commit"
    );

    // A genuine DX advance resets the streak and clears it too.
    let manager = pan182_threshold_one();
    let (qso_id, _, text) = pan182_raise(&manager).await;
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        0
    );
    assert!(
        !manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await,
        "a DX advance retires the hold with the streak"
    );
}

/// PAN-128 twin: two rearms feed one own-parity slot, and only the first
/// carries its count. When that trigger frame is reported unsent the
/// twin inherits the count — and the hold, since it can still air.
#[tokio::test]
async fn pan182_rolled_back_trigger_with_a_surviving_heir_keeps_the_hold() {
    use pancetta_core::slot::SlotParity;

    let manager = pan182_threshold_one();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // Trigger A (at FREQ, from the off-parity slot before x+30) trips
    // threshold 1; a pivot then changes only the offset; twin B (at
    // FREQ+100, inside x+30's late-start window) feeds the same slot
    // uncounted and only re-raises.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(16))
        .await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&qso_id)
        .unwrap()
        .metadata
        .frequency = FREQ + 100.0;
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(31))
        .await;
    let qso = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        qso.metadata.stall_cycles, 1,
        "one own-parity slot, one count"
    );
    let text = qso.messages.last().unwrap().raw_text.clone();

    assert!(matches!(
        manager
            .roll_back_replaced_attempt_at(
                qso_id,
                &text,
                Some(&aired_on(FREQ)),
                None,
                x + SignedDuration::from_secs(35)
            )
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        1,
        "the twin inherits the slot's count"
    );
    let result = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionTriggerOutcomePending { .. })
        ),
        "the heir can still air, so the hold moves to it: got {result:?}"
    );
    assert!(
        !manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await,
        "the rolled-back trigger's payload no longer releases anything"
    );
    assert!(
        manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ + 100.0),
                trigger_slot_played_out(&manager, qso_id)
            )
            .await,
        "the heir airing releases the hold"
    );
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("released by the heir");
}

/// PAN-182 (self-review): keep-calling repeats the same text on the same
/// offset every slot. A rearm late in its own-parity slot feeds the NEXT
/// own slot and trips the threshold while the current slot's frame — an
/// earlier emission, identical on the air — is still playing. That
/// frame's `TxFrameSent` lands before the trigger's slot even starts and
/// must not release the hold; the trigger's own report, after its slot
/// played out, does.
#[tokio::test]
async fn pan182_an_earlier_same_text_frame_airing_does_not_release_the_hold() {
    use pancetta_core::slot::SlotParity;

    let manager = pan182_threshold_one();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // 10 s into own slot x: past the acceptance cap, so it feeds x+30.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(10))
        .await;
    let text = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();
    assert_eq!(
        manager
            .keep_call_ledger
            .lock()
            .unwrap()
            .get(&qso_id)
            .and_then(|entry| entry.pending_stall_trigger.as_ref())
            .and_then(|trigger| trigger.tx_slot),
        Some(x + SignedDuration::from_secs(30)),
        "precondition: the trigger feeds the next own-parity slot"
    );

    assert!(
        !manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                x + SignedDuration::from_secs(13)
            )
            .await,
        "slot x's frame finishing is not the trigger airing"
    );
    let result = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(matches!(
        result,
        Err(QsoManagerError::OffsetActionTriggerOutcomePending { .. })
    ));
    assert!(
        manager
            .confirm_stall_trigger_aired(
                qso_id,
                &text,
                &aired_on(FREQ),
                x + SignedDuration::from_secs(43)
            )
            .await,
        "the trigger's own slot played out"
    );
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("released by the trigger's own report");
}

/// An operator `u` nudge carries no stall evidence and is never held by
/// a stall trigger, even for the same QSO.
#[tokio::test]
async fn pan182_operator_nudge_is_not_held_by_a_pending_stall_trigger() {
    let manager = pan182_threshold_one();
    let (qso_id, _, _) = pan182_raise(&manager).await;
    manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("an operator nudge commits while a stall trigger is pending");
}

/// The ticket's scenario at the exact ordering that used to lose: the
/// coordinator's commit attempt lands BEFORE the TX worker's not-sent
/// report for the threshold-triggering frame. Before PAN-182 the commit
/// succeeded and erased the evidence the rollback needed, so the QSO
/// stayed on the new offset. Now the commit is refused while the
/// trigger is unresolved, and the late report revokes the action.
#[tokio::test]
async fn pan182_lost_race_commit_attempt_then_not_sent_report_never_moves_the_offset() {
    let manager = pan182_threshold_one();
    let (qso_id, rearm_at, text) = pan182_raise(&manager).await;
    let origin = stall_origin(&manager, qso_id).await;

    let early = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, origin)
        .await;
    assert!(
        matches!(
            early,
            Err(QsoManagerError::OffsetActionTriggerOutcomePending { .. })
        ),
        "the commit can no longer win the race: got {early:?}"
    );
    assert!(matches!(
        manager
            .roll_back_unsent_attempt_at(qso_id, &text, rearm_at + SignedDuration::from_secs(1))
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    let requeued = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, origin)
        .await;
    assert!(
        matches!(
            requeued,
            Err(QsoManagerError::OffsetActionStallRolledBack { .. })
        ),
        "the requeued action dies on the revocation: got {requeued:?}"
    );
    let qso = manager.get_qso(qso_id).await.unwrap();
    assert!(
        qso.metadata.stall_cycles < 1,
        "the stall count is back below the threshold"
    );
    assert!(
        (qso.metadata.frequency - FREQ).abs() < f64::EPSILON,
        "the QSO is not left on an offset chosen from a stall that never aired"
    );
}

/// PR #507 review (P2): when the held trigger is peeled by a
/// payload-qualified rollback while a LATER own-slot emission already
/// pushed the streak past the threshold, the streak stays at/above it
/// and that newer counted attempt becomes the decisive count. The hold
/// must move to it — clearing the hold would let the next drain commit
/// before the newer frame's outcome is known, the PAN-182 race again.
#[tokio::test]
async fn pan183_pending_stall_trigger_matches_by_id() {
    let manager = pan182_threshold_one();
    let (qso_id, rearm_at, text) = pan182_raise(&manager).await;
    let trigger = manager.keep_call_attempt_ids(qso_id)[0].0;
    // A same-content sibling: same text, same stamp, its own id.
    let sibling = manager.next_attempt_id();
    {
        let mut ledger = manager.keep_call_ledger.lock().unwrap();
        let entry = ledger.get_mut(&qso_id).unwrap();
        let mut twin = entry.attempts.back().unwrap().clone();
        twin.attempt_id = sibling;
        twin.bumped_stall = false;
        twin.tx_slot = None;
        twin.recorded_sent = false;
        entry.attempts.push_back(twin);
    }
    assert!(matches!(
        manager
            .roll_back_replaced_attempt_at(
                qso_id,
                &text,
                None,
                Some(sibling),
                rearm_at + SignedDuration::from_secs(1)
            )
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    let held = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(
            held,
            Err(QsoManagerError::OffsetActionTriggerOutcomePending { .. })
        ),
        "the sibling's rollback leaves the trigger's hold: got {held:?}"
    );
    assert!(matches!(
        manager
            .roll_back_replaced_attempt_at(
                qso_id,
                &text,
                None,
                Some(trigger),
                rearm_at + SignedDuration::from_secs(1)
            )
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    let result = manager
        .apply_tx_offset_switch(qso_id, FREQ + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionStallRolledBack { .. })
        ),
        "the trigger's own rollback clears the hold: got {result:?}"
    );
}

#[tokio::test]
async fn pan182_trigger_roll_back_above_threshold_moves_the_hold_to_the_newer_count() {
    use pancetta_core::slot::SlotParity;

    let mut config = test_config();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = manager_auto(config);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Odd))
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    let x = next_own_ft8_slot(opened_at + SignedDuration::from_secs(15), SlotParity::Even);
    // Counts 1 and 2: the second rearm trips threshold 2 and arms the
    // PAN-182 trigger on its own emission (feeding the x+30 slot).
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(1))
        .await;
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(16))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2
    );
    let trigger_text = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();

    // An operator nudge moves the offset between the trigger and the
    // next rearm, so the newer counted attempt is payload-distinguishable
    // from the trigger — the precondition for a pivot rollback to peel
    // the trigger while the newer count survives.
    let nudged = FREQ + 500.0;
    manager
        .qsos
        .write()
        .await
        .get_mut(&qso_id)
        .unwrap()
        .metadata
        .frequency = nudged;
    // Count 3: a later own-parity slot, at the nudged offset.
    manager
        .rearm_manual_calls_at(x + SignedDuration::from_secs(46))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        3
    );

    // The trigger's frame was pivoted away; its payload-qualified
    // not-sent report peels exactly the trigger attempt (the newer count
    // sits at the nudged offset and does not carry the payload).
    assert!(matches!(
        manager
            .roll_back_replaced_attempt_at(
                qso_id,
                &trigger_text,
                Some(&UnsentAttemptPayload {
                    frequency_hz: FREQ,
                    remote_binding: Some((false, None)),
                }),
                None,
                x + SignedDuration::from_secs(50),
            )
            .await,
        UnsentAttemptRollback::RolledBack { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "the newer counted attempt keeps the streak at the threshold"
    );

    // The hold must have moved to that newer attempt: the commit is
    // still refused until its frame's outcome is known.
    let result = manager
        .apply_tx_offset_switch(qso_id, nudged + 300.0, stall_origin(&manager, qso_id).await)
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionTriggerOutcomePending { .. })
        ),
        "the hold moves to the newer counted attempt, got {result:?}"
    );

    // Once that newer frame airs on its own slot, the hold releases and
    // the commit lands.
    assert!(
        manager
            .confirm_stall_trigger_aired(
                qso_id,
                &trigger_text,
                &UnsentAttemptPayload {
                    frequency_hz: nudged,
                    remote_binding: Some((false, None)),
                },
                trigger_slot_played_out(&manager, qso_id),
            )
            .await,
        "the re-pointed hold releases on the newer attempt airing"
    );
    manager
        .apply_tx_offset_switch(qso_id, nudged + 300.0, stall_origin(&manager, qso_id).await)
        .await
        .expect("a released stall switch commits");
}

/// PAN-83: every aired frame of an active QSO is recorded, whether or not
/// a PAN-182 hold was waiting on it — the crediting block reads the same
/// record, and most frames air with no hold pending at all.
#[tokio::test]
async fn confirm_stall_trigger_aired_records_every_aired_frame_even_without_a_hold() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let aired_at = manager.now_utc();
    let released = manager
        .confirm_stall_trigger_aired(
            qso_id,
            "aired",
            &UnsentAttemptPayload {
                frequency_hz: FREQ,
                remote_binding: Some((false, None)),
            },
            aired_at,
        )
        .await;
    assert!(
        !released,
        "precondition: no stall action was raised, so no hold exists to release"
    );
    assert_eq!(
        manager.last_aired_frame(qso_id),
        Some(AiredFrame {
            offset_hz: FREQ,
            aired_at
        }),
        "the frame must be recorded as the QSO's newest aired frame even \
         though it released no hold"
    );
}

/// PAN-83: the record keeps the most recently REPORTED frame, in arrival
/// order — not the one with the newest `aired_at`. The TX worker reports
/// as frames play out, so arrival order is airing order; a test (or a
/// wall-clock follow-up to a simulated-clock PAN-182 release) may report
/// an earlier-stamped frame later, and that later report is the fresher
/// knowledge.
#[tokio::test]
async fn confirm_stall_trigger_aired_keeps_the_most_recently_reported_frame() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let later_aired_at = manager.now_utc();
    let earlier_aired_at = later_aired_at - SignedDuration::from_secs(10);
    let payload = UnsentAttemptPayload {
        frequency_hz: FREQ,
        remote_binding: Some((false, None)),
    };
    manager
        .confirm_stall_trigger_aired(qso_id, "aired", &payload, later_aired_at)
        .await;
    manager
        .confirm_stall_trigger_aired(qso_id, "aired", &payload, earlier_aired_at)
        .await;
    assert_eq!(
        manager.last_aired_frame(qso_id),
        Some(AiredFrame {
            offset_hz: FREQ,
            aired_at: earlier_aired_at
        }),
        "the second report wins by arrival order, whatever its aired_at"
    );
}

/// PAN-83: the aired-frame record is frame history, not stall evidence —
/// a forward advance (which runs `clear_recorded_stall_increments`) must
/// not clear it, or the next relocation's crediting would fall back to
/// provenance with the evidence it needs already discarded.
#[tokio::test]
async fn a_forward_advance_keeps_the_aired_frame_record() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let aired_at = manager.now_utc();
    manager
        .confirm_stall_trigger_aired(
            qso_id,
            "aired",
            &UnsentAttemptPayload {
                frequency_hz: FREQ,
                remote_binding: Some((false, None)),
            },
            aired_at,
        )
        .await;
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    assert_eq!(
        manager.last_aired_frame(qso_id),
        Some(AiredFrame {
            offset_hz: FREQ,
            aired_at
        }),
        "a forward advance resets stall bookkeeping, not the aired-frame \
         record"
    );
}

/// PAN-83: the TX worker's report is only meaningful for an active QSO —
/// one that has been cancelled (or otherwise gone terminal) records
/// nothing, so a stale in-flight report cannot pin crediting for a QSO
/// that no longer exists.
#[tokio::test]
async fn an_inactive_qso_records_no_aired_frame() {
    let manager = manager_auto(test_config());
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    manager.cancel_qso(qso_id).await.unwrap();
    manager
        .confirm_stall_trigger_aired(
            qso_id,
            "aired",
            &UnsentAttemptPayload {
                frequency_hz: FREQ,
                remote_binding: Some((false, None)),
            },
            manager.now_utc(),
        )
        .await;
    assert_eq!(
        manager.last_aired_frame(qso_id),
        None,
        "a report for an inactive QSO must not be recorded"
    );
}
