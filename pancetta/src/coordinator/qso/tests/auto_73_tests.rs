use super::{
    handle_tx_frame_not_sent, handle_tx_frame_sent, maybe_auto_resend_73, refund_auto_73_attempt,
    stash_auto_73_completion, CompletedTxGrace, PendingAuto73Resend, RecentManualCompletion,
    RecentManualCompletions, AUTO_73_FIRST_RESEND_MIN_DELAY, AUTO_73_MAX_RESENDS,
    AUTO_73_MIN_SPACING, AUTO_73_WINDOW,
};
use crate::message_bus::MessageBus;
use pancetta_core::slot::SlotParity;
use pancetta_core::TxPolicy;
use pancetta_qso::states::MessageType as Mt;
use pancetta_qso::{QsoId, QsoManager, QsoManagerConfig};
use std::collections::HashMap;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use tokio::sync::Mutex;

const OUR: &str = "K5ARH";
const DX: &str = "JA1ABC";

async fn manager() -> QsoManager {
    let m = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    m.start().await.expect("manager start");
    m
}

fn bus() -> MessageBus {
    MessageBus::new(1000).expect("bus")
}

/// Every test in this module uses `remote_origin: false` (see
/// `map_with_dx`), so round-14's identity-bound permission check is
/// always short-circuited regardless of this arm's state — an unarmed
/// placeholder is sufficient.
fn unarmed() -> Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>> {
    Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new()))
}

/// The completed-QSO TX grace registrar the auto-73 re-registers with,
/// at the production 45 s grace (its purge never fires inside a test).
fn grace(mgr: &QsoManager) -> CompletedTxGrace {
    CompletedTxGrace::for_test(mgr, std::time::Duration::from_secs(45))
}

/// PAN-275: a REAL manual QSO with `DX`, completed just now by a close
/// step (`respond_to_caller(SeventyThree)`), with the given remote
/// binding. The auto-73 re-keys a stashed completion by this id, so a
/// stash must point at a QSO the engine holds for a resend to go out.
async fn completed_qso(
    mgr: &QsoManager,
    remote_origin: bool,
    remote_client_key_id: Option<String>,
) -> QsoId {
    mgr.respond_to_caller(
        DX.into(),
        1500.0,
        Some(SlotParity::Even),
        pancetta_core::ResponseStep::SeventyThree,
        Some(-10.0),
        Some(-12),
        None,
        remote_origin,
        remote_client_key_id,
    )
    .await
    .expect("complete a manual QSO")
}

async fn completed_manual_qso(mgr: &QsoManager) -> QsoId {
    completed_qso(mgr, false, None).await
}

/// A manual QSO completed at 73 whose 73 has aired, so its held
/// `QsoCompleted` (the ADIF write) has been emitted (PAN-280).
async fn completed_and_aired_manual_qso(mgr: &QsoManager) -> QsoId {
    let (id, outcome) = mgr
        .respond_to_caller_with_outcome(
            DX.into(),
            1500.0,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::SeventyThree,
            Some(-10.0),
            Some(-12),
            None,
            false,
            None,
        )
        .await
        .expect("complete a manual QSO");
    mgr.confirm_completion_aired(
        id,
        outcome.attempt_id(),
        "",
        &pancetta_qso::UnsentAttemptPayload {
            frequency_hz: 1500.0,
            remote_binding: Some((false, None)),
        },
        chrono::Utc::now(),
    )
    .await;
    id
}

/// A completions map containing a single manual completion for `DX`
/// (QSO `qso_id`), completed far enough in the past to clear the
/// SM-F3/TX-F10 first-resend guard (`AUTO_73_FIRST_RESEND_MIN_DELAY`)
/// but still well within `AUTO_73_WINDOW` — i.e. a stashed completion
/// whose original 73 has certainly gone out by now, matching how these
/// tests were written before that guard existed.
fn map_with_dx(qso_id: QsoId) -> RecentManualCompletions {
    let mut map = HashMap::new();
    map.insert(
        DX.to_string(),
        RecentManualCompletion {
            completed_at: chrono::Utc::now() - chrono::Duration::seconds(40),
            resends: 0,
            last_resend_at: None,
            remote_origin: false,
            remote_client_key_id: None,
            qso_id,
            pending_resends: Vec::new(),
        },
    );
    Arc::new(Mutex::new(map))
}

fn rr73_to_us() -> Mt {
    Mt::FinalConfirmation {
        to_station: OUR.to_string(),
        from_station: DX.to_string(),
    }
}

/// Count `MessageToSend` events the manager has emitted by draining a
/// subscriber that was attached before the action under test.
fn drain_sends(rx: &mut tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>) -> usize {
    let mut n = 0;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, pancetta_qso::QsoEvent::MessageToSend { .. }) {
            n += 1;
        }
    }
    n
}

/// A directed RR73 from a stashed manual completion triggers exactly one
/// auto-73 per slot, and never more than `AUTO_73_MAX_RESENDS` total even
/// if RR73 arrives every slot.
#[tokio::test]
async fn bound_holds_under_repeated_rr73_every_slot() {
    let mgr = manager().await;
    let map = map_with_dx(completed_manual_qso(&mgr).await);
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    // Simulate the DX hammering RR73 across many slots. We bypass the
    // per-slot guard by zeroing last_resend_at between calls — that proves
    // the HARD cap (resends) holds independently of the time guard.
    for _ in 0..10 {
        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            &mgr,
            &map,
            &policy,
            &bus,
            &unarmed(),
            &grace(&mgr),
        )
        .await;
        if let Some(e) = map.lock().await.get_mut(DX) {
            e.last_resend_at = None; // defeat the per-slot guard for this test
        }
    }

    let sends = drain_sends(&mut rx);
    assert_eq!(
        sends as u8, AUTO_73_MAX_RESENDS,
        "auto-73 must be capped at {AUTO_73_MAX_RESENDS}, got {sends}"
    );
    // After the cap the entry is dropped so it can never fire again.
    assert!(map.lock().await.get(DX).is_none());
}

/// Within one slot, two decodes of the same RR73 fire only ONE auto-73.
#[tokio::test]
async fn one_per_slot_dedup() {
    let mgr = manager().await;
    let map = map_with_dx(completed_manual_qso(&mgr).await);
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    for _ in 0..3 {
        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            &mgr,
            &map,
            &policy,
            &bus,
            &unarmed(),
            &grace(&mgr),
        )
        .await;
        // Do NOT reset last_resend_at — same slot.
    }

    assert_eq!(drain_sends(&mut rx), 1, "only one 73 per slot");
    assert_eq!(map.lock().await.get(DX).map(|e| e.resends), Some(1));
}

/// An RR73 outside the 3-minute window never triggers an auto-73 (the
/// entry is pruned on lookup).
#[tokio::test]
async fn outside_window_no_resend() {
    let mgr = manager().await;
    let qso_id = completed_manual_qso(&mgr).await;
    let map = {
        let mut m = HashMap::new();
        m.insert(
            DX.to_string(),
            RecentManualCompletion {
                completed_at: chrono::Utc::now() - AUTO_73_WINDOW - chrono::Duration::seconds(1),
                resends: 0,
                last_resend_at: None,
                remote_origin: false,
                remote_client_key_id: None,
                qso_id,
                pending_resends: Vec::new(),
            },
        );
        Arc::new(Mutex::new(m))
    };
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace(&mgr),
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 0);
    assert!(map.lock().await.get(DX).is_none(), "stale entry pruned");
}

/// While a QSO with the DX is active, no auto-73 (don't fight a live QSO).
#[tokio::test]
async fn active_qso_no_resend() {
    let mgr = manager().await;
    // A real stashed completion, so only the active-QSO skip can stop
    // the resend (PAN-275: a stash for an unknown id never re-keys).
    let map = map_with_dx(completed_manual_qso(&mgr).await);
    // Open a live QSO with DX (RespondingToCq via manual call).
    mgr.respond_to_cq_manual(DX.to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .expect("start qso");
    assert!(
        mgr.get_active_qsos().await.iter().any(|(_, p)| p
            .metadata
            .their_callsign
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(DX))),
        "precondition: a live QSO with DX"
    );
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    // Subscribe AFTER the manual call so its MessageToSend is not counted;
    // we only want to observe whether the auto-73 fires.
    let mut rx = mgr.subscribe();

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace(&mgr),
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 0, "no auto-73 while QSO active");
}

/// A station NOT in the map (e.g. an AUTONOMOUS-completed QSO, which the
/// QsoCompleted handler never stashes) gets no auto-73.
#[tokio::test]
async fn not_in_map_no_resend() {
    let mgr = manager().await;
    let map: RecentManualCompletions = Arc::new(Mutex::new(HashMap::new()));
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace(&mgr),
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 0);
}

/// TX policy DISABLED blocks the auto-73 entirely (and does not consume
/// the resend budget).
#[tokio::test]
async fn disabled_policy_no_resend() {
    let mgr = manager().await;
    let map = map_with_dx(completed_manual_qso(&mgr).await);
    let policy = AtomicU8::new(TxPolicy::Disabled.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace(&mgr),
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 0, "DISABLED blocks auto-73");
    assert_eq!(
        map.lock().await.get(DX).map(|e| e.resends),
        Some(0),
        "budget untouched under DISABLED"
    );
}

/// RESPOND-ONLY allows the auto-73 (it's a response, not an initiation).
#[tokio::test]
async fn respond_only_allows_resend() {
    let mgr = manager().await;
    let map = map_with_dx(completed_manual_qso(&mgr).await);
    let policy = AtomicU8::new(TxPolicy::RespondOnly.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace(&mgr),
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 1, "RESPOND-ONLY permits the 73");
}

/// A non-close message (e.g. a signal report) directed at us never
/// triggers an auto-73, even from a stashed callsign.
#[tokio::test]
async fn non_close_message_ignored() {
    let mgr = manager().await;
    let map = map_with_dx(completed_manual_qso(&mgr).await);
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    let report = Mt::SignalReport {
        to_station: OUR.to_string(),
        from_station: DX.to_string(),
        report: -12,
    };
    maybe_auto_resend_73(
        &report,
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace(&mgr),
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 0);
}

/// An RR73 NOT directed at us (to a third party) is ignored.
#[tokio::test]
async fn rr73_to_third_party_ignored() {
    let mgr = manager().await;
    let map = map_with_dx(completed_manual_qso(&mgr).await);
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    let rr73 = Mt::FinalConfirmation {
        to_station: "W1XYZ".to_string(),
        from_station: DX.to_string(),
    };
    maybe_auto_resend_73(
        &rr73,
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace(&mgr),
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 0);
}

/// SM-F3/TX-F10: two decodes of the SAME closing RR73, arriving
/// back-to-back immediately after `QsoCompleted` (i.e. within the
/// original 73's TX-scheduler defer window), must NOT fire a second 73 —
/// the first one may not have keyed yet. Neither call may consume the
/// resend budget.
#[tokio::test]
async fn duplicate_decode_within_guard_window_no_resend() {
    let mgr = manager().await;
    let qso_id = completed_manual_qso(&mgr).await;
    // Completion JUST happened — the original 73 may still be deferred.
    let map = {
        let mut m = HashMap::new();
        m.insert(
            DX.to_string(),
            RecentManualCompletion {
                completed_at: chrono::Utc::now(),
                resends: 0,
                last_resend_at: None,
                remote_origin: false,
                remote_client_key_id: None,
                qso_id,
                pending_resends: Vec::new(),
            },
        );
        Arc::new(Mutex::new(m))
    };
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    // Two "copies" of the same decode (as FT8 decoders routinely emit),
    // arriving in immediate succession.
    for _ in 0..2 {
        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            &mgr,
            &map,
            &policy,
            &bus,
            &unarmed(),
            &grace(&mgr),
        )
        .await;
    }

    assert_eq!(
        drain_sends(&mut rx),
        0,
        "duplicate decodes within the first-resend guard window must not \
         fire a 2nd 73 before the 1st one's scheduled defer could complete"
    );
    assert_eq!(
        map.lock().await.get(DX).map(|e| e.resends),
        Some(0),
        "guarded attempts must not consume the resend budget"
    );
}

/// A GENUINE later resend — the DX truly didn't copy our first 73 and
/// repeats RR73 well after the first-resend guard window elapses — must
/// still fire normally. This is the feature `maybe_auto_resend_73` exists
/// for; the SM-F3/TX-F10 guard must not regress it.
#[tokio::test]
async fn genuine_later_resend_past_guard_window_fires() {
    let mgr = manager().await;
    let qso_id = completed_manual_qso(&mgr).await;
    let map = {
        let mut m = HashMap::new();
        m.insert(
            DX.to_string(),
            RecentManualCompletion {
                completed_at: chrono::Utc::now()
                    - AUTO_73_FIRST_RESEND_MIN_DELAY
                    - chrono::Duration::seconds(5),
                resends: 0,
                last_resend_at: None,
                remote_origin: false,
                remote_client_key_id: None,
                qso_id,
                pending_resends: Vec::new(),
            },
        );
        Arc::new(Mutex::new(m))
    };
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace(&mgr),
    )
    .await;

    assert_eq!(
        drain_sends(&mut rx),
        1,
        "a genuine later resend past the guard window must still fire"
    );
    assert_eq!(map.lock().await.get(DX).map(|e| e.resends), Some(1));
}

/// Round 14 / PAN-138: a completion bound to client-a must not consume
/// the auto-73 budget while client-b holds the arm. The early check is
/// a fast path; the engine outcome also reports a later denial.
#[tokio::test]
async fn denied_identity_rolls_back_the_resend_commit() {
    let mgr = manager().await;
    let qso_id = completed_qso(&mgr, true, Some("client-a".to_string())).await;
    let map = {
        let mut m = HashMap::new();
        m.insert(
            DX.to_string(),
            RecentManualCompletion {
                completed_at: chrono::Utc::now() - chrono::Duration::seconds(40),
                resends: 0,
                last_resend_at: None,
                remote_origin: true,
                remote_client_key_id: Some("client-a".to_string()),
                qso_id,
                pending_resends: Vec::new(),
            },
        );
        Arc::new(Mutex::new(m))
    };
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    // client-b holds the arm — a mismatch against the entry's client-a.
    let mut st = pancetta_agent::arm::ArmState::new();
    let now_ms = chrono::Utc::now().timestamp_millis();
    st.set_channel_consent(
        pancetta_agent::arm::ConsentChannel::StationAgent,
        true,
        now_ms,
    );
    st.set_channel_consent(pancetta_agent::arm::ConsentChannel::WsjtxUdp, true, now_ms);
    st.arm(
        pancetta_agent::arm::VerifiedArmGrant {
            operator_callsign: "W1AW".to_string(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "auto73-rollback-test-jti".to_string(),
            client_key_id: "client-b".to_string(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(std::sync::Mutex::new(st));

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &arm,
        &grace(&mgr),
    )
    .await;

    assert_eq!(
        drain_sends(&mut rx),
        0,
        "a resend denied for identity mismatch must never emit a frame"
    );
    let entry = map.lock().await.get(DX).cloned().expect("entry retained");
    assert_eq!(
        entry.resends, 0,
        "the resend commit must be rolled back exactly, not left incremented"
    );
    assert_eq!(
        entry.last_resend_at, None,
        "last_resend_at must also be rolled back, or a genuine later RR73 \
         could be wrongly suppressed by the per-slot guard"
    );
}

fn armed_for(client: &str) -> Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>> {
    let mut st = pancetta_agent::arm::ArmState::new();
    let now_ms = chrono::Utc::now().timestamp_millis();
    st.set_channel_consent(
        pancetta_agent::arm::ConsentChannel::StationAgent,
        true,
        now_ms,
    );
    st.set_channel_consent(pancetta_agent::arm::ConsentChannel::WsjtxUdp, true, now_ms);
    st.arm(
        pancetta_agent::arm::VerifiedArmGrant {
            operator_callsign: "W1AW".to_string(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "pan138-auto73-jti".to_string(),
            client_key_id: client.to_string(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    Arc::new(std::sync::Mutex::new(st))
}

/// PAN-138 (round 16 on PR #362): the coordinator's early check passes
/// (client-a holds the arm), but by the time the engine re-checks under
/// its `qsos` guard client-a has disarmed. The resend emits nothing, so
/// its budget commit must be undone and no "Re-sending 73" status shown.
async fn engine_denial_rolls_back(rekey_existing: bool) {
    let engine_permits = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    let permits = Arc::clone(&engine_permits);
    mgr.set_remote_tx_permitted_source(Arc::new(move |_| {
        permits.load(std::sync::atomic::Ordering::SeqCst)
    }));
    mgr.start().await.expect("manager start");
    // A real completed QSO is re-keyed by id; an unknown id re-keys
    // nothing (PAN-275).
    let qso_id = if rekey_existing {
        mgr.respond_to_caller(
            DX.into(),
            1500.0,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::SeventyThree,
            None,
            None,
            None,
            true,
            Some("client-a".into()),
        )
        .await
        .unwrap()
    } else {
        uuid::Uuid::new_v4()
    };
    let map = {
        let mut m = HashMap::new();
        m.insert(
            DX.to_string(),
            RecentManualCompletion {
                completed_at: chrono::Utc::now() - chrono::Duration::seconds(40),
                resends: 0,
                last_resend_at: None,
                remote_origin: true,
                remote_client_key_id: Some("client-a".to_string()),
                qso_id,
                pending_resends: Vec::new(),
            },
        );
        Arc::new(Mutex::new(m))
    };
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let (_tui_s, tui) = bus
        .create_channel(crate::message_bus::ComponentId::Tui)
        .await
        .unwrap();
    let mut rx = mgr.subscribe();
    engine_permits.store(false, std::sync::atomic::Ordering::SeqCst);

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &armed_for("client-a"),
        &grace(&mgr),
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 0);
    let entry = map.lock().await.get(DX).cloned().expect("entry retained");
    assert_eq!(entry.resends, 0, "the resend commit must be rolled back");
    assert_eq!(entry.last_resend_at, None);
    while let Ok(msg) = tui.try_recv() {
        if let crate::message_bus::MessageType::StatusUpdate { text, .. } = &msg.message_type {
            assert!(
                !text.contains("Re-sending 73"),
                "status claimed a resend: {text}"
            );
        }
    }
}

#[tokio::test]
async fn engine_denial_of_a_completed_qso_rekey_rolls_back_the_commit() {
    engine_denial_rolls_back(true).await;
}

/// PAN-275: a stash whose QSO the engine does not hold (an unknown id)
/// re-keys nothing — the auto-73 never opens a QSO — so its commit is
/// rolled back.
#[tokio::test]
async fn engine_refusal_for_an_unknown_completion_rolls_back_the_commit() {
    engine_denial_rolls_back(false).await;
}

/// The rollback only undoes the commit it made: a newer completion
/// stashed for the same callsign in the meantime is left alone.
#[tokio::test]
async fn rollback_leaves_a_replaced_entry_alone() {
    let map = map_with_dx(uuid::Uuid::new_v4());
    let newer = map.lock().await.get(DX).cloned().unwrap();
    let committed_at = chrono::Utc::now();
    super::roll_back_auto_73_commit(&map, DX, uuid::Uuid::new_v4(), committed_at, 0, None).await;
    let after = map.lock().await.get(DX).cloned().unwrap();
    assert_eq!(after.qso_id, newer.qso_id);
    assert_eq!(after.resends, newer.resends);

    if let Some(e) = map.lock().await.get_mut(DX) {
        e.resends = 2;
        e.last_resend_at = Some(committed_at);
    }
    super::roll_back_auto_73_commit(&map, DX, newer.qso_id, committed_at, 1, None).await;
    let after = map.lock().await.get(DX).cloned().unwrap();
    assert_eq!((after.resends, after.last_resend_at), (1, None));
}

fn stashed(
    qso_id: QsoId,
    completed_at: chrono::DateTime<chrono::Utc>,
    resends: u8,
    last_resend_at: Option<chrono::DateTime<chrono::Utc>>,
) -> RecentManualCompletion {
    RecentManualCompletion {
        completed_at,
        resends,
        last_resend_at,
        remote_origin: false,
        remote_client_key_id: None,
        qso_id,
        pending_resends: Vec::new(),
    }
}

/// PAN-275: a repeated completion for the SAME QSO keeps the entry's
/// window and counters, so it can never reset the resend cap.
#[test]
fn stash_auto_73_completion_keeps_counters_for_the_same_qso() {
    let now = chrono::Utc::now();
    let id = uuid::Uuid::new_v4();
    let t = now - chrono::Duration::seconds(20);
    let completed = now - chrono::Duration::seconds(60);
    let mut map = HashMap::new();
    map.insert(DX.to_string(), stashed(id, completed, 2, Some(t)));

    stash_auto_73_completion(&mut map, DX, stashed(id, now, 0, None), now);

    let e = map.get(DX).expect("entry kept");
    assert_eq!(e.resends, 2);
    assert_eq!(e.last_resend_at, Some(t));
    assert_eq!(e.completed_at, completed);
    assert_eq!(e.qso_id, id);
}

/// A completion for a DIFFERENT QSO with the same station is a new
/// contact: it replaces the entry with a fresh budget.
#[test]
fn stash_auto_73_completion_replaces_a_different_qso() {
    let now = chrono::Utc::now();
    let old = uuid::Uuid::new_v4();
    let new = uuid::Uuid::new_v4();
    let mut map = HashMap::new();
    map.insert(
        DX.to_string(),
        stashed(
            old,
            now - chrono::Duration::seconds(60),
            2,
            Some(now - chrono::Duration::seconds(20)),
        ),
    );

    stash_auto_73_completion(&mut map, DX, stashed(new, now, 0, None), now);

    let e = map.get(DX).expect("entry replaced");
    assert_eq!(e.qso_id, new);
    assert_eq!(e.resends, 0);
    assert_eq!(e.last_resend_at, None);
    assert_eq!(e.completed_at, now);
}

#[test]
fn stash_auto_73_completion_inserts_when_vacant_and_prunes_expired() {
    let now = chrono::Utc::now();
    let mut map = HashMap::new();
    map.insert(
        "W9OLD".to_string(),
        stashed(
            uuid::Uuid::new_v4(),
            now - AUTO_73_WINDOW - chrono::Duration::seconds(1),
            0,
            None,
        ),
    );
    let id = uuid::Uuid::new_v4();

    stash_auto_73_completion(&mut map, "dl1abc", stashed(id, now, 0, None), now);

    assert!(map.get("W9OLD").is_none(), "expired entry pruned");
    assert_eq!(map.get("DL1ABC").map(|e| e.qso_id), Some(id));
    assert!(map.get("dl1abc").is_none(), "keyed by the uppercased call");
    assert_eq!(map.len(), 1);
}

/// PAN-275 end to end: a QSO completed 60 s ago (past the 45 s rework
/// grace, inside the 3 min auto-73 window) whose station repeats RR73
/// gets its 73 re-sent for the EXISTING QSO, `qsos.adi` keeps exactly
/// one record for the contact, and the resend counter goes 1, 2, 3
/// (never reset). Runs the real engine, the real ADIF writer, the stash
/// function the `QsoCompleted` arm uses, the grace registrar the
/// component uses, and the real `maybe_auto_resend_73`.
#[tokio::test]
async fn pan275_late_rr73_rekeys_the_existing_qso_and_logs_once() {
    use super::replay_local_log_tests::{adif_record_count, wait_for_adif_record};

    let dir = tempfile::tempdir().expect("tempdir");
    let adif_path = dir.path().join("qsos.adi");
    let mgr = manager().await;
    let adif_subscribers = super::AdifSubscribers::new();
    let (adif, _logger, _seed) = super::start_local_qso_log_writers(
        false,
        &adif_path,
        &dir.path().join("qso.db"),
        100,
        false,
        &pancetta_config::DatabaseConfig::default(),
        &mgr,
        &adif_subscribers,
    )
    .await;
    assert!(adif.is_some(), "a live start runs the ADIF writer");
    let id = completed_and_aired_manual_qso(&mgr).await;
    assert_eq!(wait_for_adif_record(&adif_path).await, 1, "precondition");

    // The QSO completed 60 s ago: past the 45 s grace, inside the 3 min
    // window.
    assert!(
        mgr.backdate_completion_for_test(id, chrono::Duration::seconds(60))
            .await
    );
    let now = chrono::Utc::now();
    let map: RecentManualCompletions = Arc::new(Mutex::new(HashMap::new()));
    stash_auto_73_completion(
        &mut *map.lock().await,
        DX,
        stashed(id, now - chrono::Duration::seconds(60), 0, None),
        now,
    );
    let grace = CompletedTxGrace::for_test(
        &mgr,
        pancetta_qso::qso_manager::COMPLETED_QSO_REWORK_GRACE
            .to_std()
            .unwrap(),
    );
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let key = crate::coordinator::active_tx_qso_key(&id.to_string());
    let mut rx = mgr.subscribe();

    for round in 1..=AUTO_73_MAX_RESENDS {
        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            &mgr,
            &map,
            &policy,
            &bus,
            &unarmed(),
            &grace,
        )
        .await;
        let mut sends = Vec::new();
        let mut completed = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                pancetta_qso::QsoEvent::MessageToSend {
                    qso_id, message, ..
                } => sends.push((qso_id, message)),
                pancetta_qso::QsoEvent::QsoCompleted {
                    qso_id, metadata, ..
                } => {
                    completed += 1;
                    // Feed it through the stash exactly as the arm would.
                    let at = chrono::Utc::now();
                    let call = metadata.their_callsign.clone().unwrap_or_default();
                    stash_auto_73_completion(
                        &mut *map.lock().await,
                        &call,
                        stashed(qso_id, at, 0, None),
                        at,
                    );
                }
                _ => {}
            }
        }
        assert_eq!(sends.len(), 1, "round {round}: exactly one frame");
        assert_eq!(
            sends[0].0, id,
            "round {round}: the 73 is for the existing QSO"
        );
        assert!(
            matches!(sends[0].1, Mt::SeventyThree { .. }),
            "round {round}: re-sent frame is our 73, got {:?}",
            sends[0].1
        );
        assert_eq!(completed, 0, "round {round}: no second QsoCompleted");
        assert_eq!(
            map.lock().await.get(DX).map(|e| e.resends),
            Some(round),
            "the resend counter is incremented, not reset"
        );
        assert!(
            grace.active_tx_qsos.read().unwrap().contains(&key),
            "round {round}: the re-keyed QSO is live for the TX worker"
        );
        // The DX repeats RR73 in a later slot.
        if let Some(e) = map.lock().await.get_mut(DX) {
            e.last_resend_at = e
                .last_resend_at
                .map(|t| t - AUTO_73_MIN_SPACING - chrono::Duration::seconds(1));
        }
    }

    // A fourth repeat sends nothing and drops the entry.
    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace,
    )
    .await;
    assert_eq!(drain_sends(&mut rx), 0, "the cap holds");
    assert!(
        map.lock().await.get(DX).is_none(),
        "entry dropped at the cap"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        adif_record_count(&adif_path),
        1,
        "qsos.adi must hold one record for the contact"
    );
    adif_subscribers
        .stop(std::time::Duration::from_secs(2))
        .await;
}

/// PAN-275 (self-review): a re-key the engine refuses — here the QSO was
/// worked on 20 m and the rig is now on 40 m — sends nothing, restores
/// the budget and registers no TX liveness, so repeated RR73s cannot
/// keep the QSO live for the TX worker through the rollback.
#[tokio::test]
async fn pan275_refused_rekey_registers_no_tx_liveness() {
    let dial = Arc::new(std::sync::atomic::AtomicU64::new(14_074_000));
    let mut mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    mgr.set_dial_frequency_source(Arc::clone(&dial));
    mgr.start().await.expect("manager start");
    let map = map_with_dx(completed_manual_qso(&mgr).await);
    dial.store(7_074_000, std::sync::atomic::Ordering::Relaxed);
    let grace = grace(&mgr);
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let mut rx = mgr.subscribe();

    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        &mgr,
        &map,
        &policy,
        &bus,
        &unarmed(),
        &grace,
    )
    .await;

    assert_eq!(drain_sends(&mut rx), 0, "no 73 for a QSO on another band");
    assert_eq!(map.lock().await.get(DX).map(|e| e.resends), Some(0));
    assert!(
        grace.active_tx_qsos.read().unwrap().is_empty(),
        "a refused re-key registers no TX liveness"
    );
}

/// Guard for the [duplicate_checking] wiring: the pancetta-config defaults
/// must equal pancetta-qso's hard-coded DuplicateCheckConfig::default(),
/// so a config file WITHOUT the section produces byte-identical behavior
/// to the pre-wiring binary. If either side changes, this fails.
#[test]
fn config_duplicate_defaults_match_qso_manager_defaults() {
    let c = pancetta_config::Config::default().duplicate_checking;
    let q = pancetta_qso::DuplicateCheckConfig::default();
    assert_eq!(c.enabled, q.enabled);
    assert_eq!(c.time_window_hours, q.time_window_hours);
    assert_eq!(c.check_frequency, q.check_frequency);
}

/// PAN-280: an engine holding a remote-origin QSO completed at 73 for
/// `DX` (client-a permitted) and a stash entry for it, completed long
/// enough ago to clear the first-resend guard.
async fn completed_remote_qso() -> (QsoManager, RecentManualCompletions, pancetta_qso::QsoId) {
    let mut mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    mgr.set_remote_tx_permitted_source(Arc::new(|k: Option<&str>| k == Some("client-a")));
    mgr.start().await.expect("manager start");
    let qso_id = mgr
        .respond_to_caller(
            DX.into(),
            1500.0,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::SeventyThree,
            None,
            None,
            None,
            true,
            Some("client-a".into()),
        )
        .await
        .unwrap();
    let mut m = HashMap::new();
    m.insert(
        DX.to_string(),
        RecentManualCompletion {
            completed_at: chrono::Utc::now() - chrono::Duration::seconds(40),
            resends: 0,
            last_resend_at: None,
            remote_origin: true,
            remote_client_key_id: Some("client-a".to_string()),
            qso_id,
            pending_resends: Vec::new(),
        },
    );
    (mgr, Arc::new(Mutex::new(m)), qso_id)
}

/// Runs one auto-73 for `DX` and returns the emitted frame's attempt id.
async fn run_auto_73(
    mgr: &QsoManager,
    map: &RecentManualCompletions,
) -> Option<pancetta_qso::TxAttemptId> {
    let mut rx = mgr.subscribe();
    let policy = AtomicU8::new(TxPolicy::Full.as_u8());
    let bus = bus();
    let (_tui_s, _tui) = bus
        .create_channel(crate::message_bus::ComponentId::Tui)
        .await
        .unwrap();
    maybe_auto_resend_73(
        &rr73_to_us(),
        OUR,
        1500.0,
        mgr,
        map,
        &policy,
        &bus,
        &armed_for("client-a"),
        &grace(mgr),
    )
    .await;
    let mut last = None;
    while let Ok(ev) = rx.try_recv() {
        if let pancetta_qso::QsoEvent::MessageToSend { attempt_id, .. } = ev {
            last = attempt_id;
        }
    }
    last
}

/// PAN-280: the 73 of an open at the close logs only when its
/// `TxFrameSent` reaches the Qso task's handler.
#[tokio::test]
async fn tx_frame_sent_commits_a_pending_completion() {
    let mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    let mut rx = mgr.subscribe();
    let (qso_id, opened) = mgr
        .respond_to_caller_with_outcome(
            DX.into(),
            1500.0,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::SeventyThree,
            None,
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let completed = |rx: &mut tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>| {
        let mut n = 0;
        while let Ok(ev) = rx.try_recv() {
            n += usize::from(matches!(ev, pancetta_qso::QsoEvent::QsoCompleted { .. }));
        }
        n
    };
    assert_eq!(completed(&mut rx), 0, "nothing logs at queue time");
    let map: RecentManualCompletions = Arc::new(Mutex::new(HashMap::new()));
    handle_tx_frame_sent(
        &mgr,
        &map,
        &qso_id.to_string(),
        "JA1ABC K5ARH 73",
        &pancetta_qso::UnsentAttemptPayload {
            frequency_hz: 1500.0,
            remote_binding: Some((false, None)),
        },
        chrono::Utc::now(),
        opened.attempt_id(),
    )
    .await;
    assert_eq!(completed(&mut rx), 1);
}

/// PAN-280: a not-sent report reverts a pending completion before the
/// PAN-75 ledger rollback, so a re-key's charge on the reopened (active)
/// QSO is reversible.
#[tokio::test]
async fn tx_frame_not_sent_reverts_before_rolling_back_the_ledger() {
    let mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    let open = |step| {
        mgr.respond_to_caller_with_outcome(
            DX.into(),
            1500.0,
            Some(SlotParity::Even),
            step,
            None,
            None,
            None,
            false,
            None,
        )
    };
    let (qso_id, _) = open(pancetta_core::ResponseStep::ReportAck).await.unwrap();
    let (_, advanced) = open(pancetta_core::ResponseStep::SeventyThree)
        .await
        .unwrap();
    let before_rekey = mgr.get_qso(qso_id).await.unwrap().metadata.call_count;
    let (_, rekey) = open(pancetta_core::ResponseStep::SeventyThree)
        .await
        .unwrap();
    assert!(matches!(rekey, pancetta_qso::ManualCallOutcome::Resent(_)));
    assert_eq!(
        mgr.get_qso(qso_id).await.unwrap().metadata.call_count,
        before_rekey + 1
    );
    let map: RecentManualCompletions = Arc::new(Mutex::new(HashMap::new()));
    let id = qso_id.to_string();
    handle_tx_frame_not_sent(
        &mgr,
        &map,
        &id,
        "JA1ABC K5ARH 73",
        None,
        advanced.attempt_id(),
    )
    .await;
    assert!(
        matches!(
            mgr.get_qso(qso_id).await.unwrap().state,
            pancetta_qso::QsoState::Completed { .. }
        ),
        "the re-key can still air"
    );
    handle_tx_frame_not_sent(&mgr, &map, &id, "JA1ABC K5ARH 73", None, rekey.attempt_id()).await;
    let after = mgr.get_qso(qso_id).await.unwrap();
    assert!(matches!(
        after.state,
        pancetta_qso::QsoState::SendingReport { .. }
    ));
    assert_eq!(
        after.metadata.call_count, before_rekey,
        "the reopened QSO is active, so the re-key's charge rolled back"
    );
}

/// PAN-280 scenario 2: a completed QSO re-sending 73 is denied at key
/// time; the auto-73 counter is not charged for that attempt.
#[tokio::test]
async fn key_time_denial_of_an_auto_73_refunds_the_counter() {
    let (mgr, map, qso_id) = completed_remote_qso().await;
    let id_b = run_auto_73(&mgr, &map)
        .await
        .expect("auto-73 emitted a frame");
    let entry = map.lock().await.get(DX).cloned().unwrap();
    assert_eq!(entry.resends, 1);
    assert!(entry.last_resend_at.is_some());
    assert_eq!(
        entry
            .pending_resends
            .iter()
            .map(|p| p.attempt_id)
            .collect::<Vec<_>>(),
        vec![id_b],
        "the marker holds the emitted frame's attempt id"
    );
    handle_tx_frame_not_sent(
        &mgr,
        &map,
        &qso_id.to_string(),
        "JA1ABC K5ARH 73",
        None,
        Some(id_b),
    )
    .await;
    let entry = map.lock().await.get(DX).cloned().unwrap();
    assert_eq!(entry.resends, 0, "the denied resend's charge is refunded");
    assert_eq!(entry.last_resend_at, None);
    assert!(entry.pending_resends.is_empty());
}

/// PAN-280: an auto-73 that aired keeps its charge; its marker clears.
#[tokio::test]
async fn aired_auto_73_keeps_its_charge() {
    let (mgr, map, qso_id) = completed_remote_qso().await;
    let id_b = run_auto_73(&mgr, &map)
        .await
        .expect("auto-73 emitted a frame");
    handle_tx_frame_sent(
        &mgr,
        &map,
        &qso_id.to_string(),
        "JA1ABC K5ARH 73",
        &pancetta_qso::UnsentAttemptPayload {
            frequency_hz: 1500.0,
            remote_binding: Some((true, Some("client-a".into()))),
        },
        chrono::Utc::now(),
        Some(id_b),
    )
    .await;
    let entry = map.lock().await.get(DX).cloned().unwrap();
    assert_eq!(entry.resends, 1, "an aired resend stays charged");
    assert!(entry.last_resend_at.is_some());
    assert!(entry.pending_resends.is_empty());
    // A later not-sent report for the same id refunds nothing.
    assert!(!refund_auto_73_attempt(&map, Some(id_b)).await);
    assert_eq!(map.lock().await.get(DX).unwrap().resends, 1);
}

/// PAN-280: an operator FIX B re-key reported not sent never touches the
/// auto-73 budget: only attempts an auto-73 charged are refunded.
#[tokio::test]
async fn operator_rekey_not_sent_does_not_touch_the_auto_73_budget() {
    let (mgr, map, qso_id) = completed_remote_qso().await;
    let id_b = run_auto_73(&mgr, &map)
        .await
        .expect("auto-73 emitted a frame");
    let (_, outcome) = mgr
        .respond_to_caller_with_outcome(
            DX.into(),
            1500.0,
            Some(SlotParity::Even),
            pancetta_core::ResponseStep::SeventyThree,
            None,
            None,
            None,
            true,
            Some("client-a".into()),
        )
        .await
        .unwrap();
    let id_c = outcome
        .attempt_id()
        .expect("operator re-key emitted a frame");
    assert_ne!(id_c, id_b);
    handle_tx_frame_not_sent(
        &mgr,
        &map,
        &qso_id.to_string(),
        "JA1ABC K5ARH 73",
        None,
        Some(id_c),
    )
    .await;
    let entry = map.lock().await.get(DX).cloned().unwrap();
    assert_eq!(entry.resends, 1);
    assert_eq!(entry.pending_resends.len(), 1);
    assert_eq!(entry.pending_resends[0].attempt_id, id_b);
    // An id-less report refunds nothing either.
    assert!(!refund_auto_73_attempt(&map, None).await);
    assert_eq!(map.lock().await.get(DX).unwrap().resends, 1);
}

/// PAN-280: two resends in flight one slot apart are refunded one unit
/// each; the pacing stamp is restored only while it holds the refunded
/// resend's own stamp.
#[tokio::test]
async fn overlapping_auto_73_refund_takes_off_one_unit() {
    let map = map_with_dx(uuid::Uuid::new_v4());
    let t1 = chrono::Utc::now() - chrono::Duration::seconds(20);
    let t2 = t1 + chrono::Duration::seconds(15);
    {
        let mut m = map.lock().await;
        let e = m.get_mut(DX).unwrap();
        e.resends = 2;
        e.last_resend_at = Some(t2);
        e.pending_resends = vec![
            PendingAuto73Resend {
                attempt_id: pancetta_qso::TxAttemptId(1),
                committed_at: t1,
                prev_last_resend_at: None,
            },
            PendingAuto73Resend {
                attempt_id: pancetta_qso::TxAttemptId(2),
                committed_at: t2,
                prev_last_resend_at: Some(t1),
            },
        ];
    }
    assert!(refund_auto_73_attempt(&map, Some(pancetta_qso::TxAttemptId(1))).await);
    let e = map.lock().await.get(DX).cloned().unwrap();
    assert_eq!(e.resends, 1);
    assert_eq!(
        e.last_resend_at,
        Some(t2),
        "the later resend's pacing stays"
    );
    assert!(refund_auto_73_attempt(&map, Some(pancetta_qso::TxAttemptId(2))).await);
    let e = map.lock().await.get(DX).cloned().unwrap();
    assert_eq!(e.resends, 0);
    assert_eq!(
        e.last_resend_at, None,
        "back to the value before the first refunded resend"
    );
    assert!(e.pending_resends.is_empty());
}
