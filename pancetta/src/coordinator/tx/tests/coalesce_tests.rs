use super::*;
use pancetta_core::time::UtcTimestamp;
use std::collections::HashSet;

fn audit_tmp() -> crate::coordinator::audit_test_util::TempPath {
    crate::coordinator::audit_test_util::TempPath::new("pancetta-tx-coalesce-test")
}

/// `max_concurrent_qsos` for tests that aren't about the concurrent-QSO
/// cap itself — large enough that it never becomes the limiting factor,
/// leaving `MAX_RETAINED_TX_STREAMS` as the only cap in play (unchanged
/// pre-existing behavior).
const UNBOUNDED: u32 = MAX_RETAINED_TX_STREAMS as u32;

fn entry(text: &str, qso_id: Option<&str>) -> CoalesceEntry {
    CoalesceEntry {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: 1000.0,
        qso_id: qso_id.map(|s| s.to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

/// Like [`entry`] but with an explicit `frequency_offset` — used by tests
/// that expect ≥2 entries to survive together in `retained` (the
/// FQ-F4/TX-F6 frequency-separation exclusion pass would otherwise treat
/// same-frequency retained entries as a bundle-frequency conflict, since
/// `entry`'s default 1000.0 Hz is shared by every plain `entry(...)`
/// call in this module).
fn entry_freq(text: &str, qso_id: Option<&str>, freq: f64) -> CoalesceEntry {
    CoalesceEntry {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: freq,
        qso_id: qso_id.map(|s| s.to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

/// Predicate over a fixed live-set (uppercased+trimmed to match the
/// production canonicalization). `None` is always live.
fn live_in(set: &HashSet<String>) -> impl FnMut(Option<&str>) -> bool + '_ {
    move |id: Option<&str>| match id {
        None => true,
        Some(id) => set.contains(&super::super::active_tx_qso_key(id)),
    }
}

fn pan73_tx(text: &str, qso: &str, hz: f64) -> MessageType {
    MessageType::TransmitRequest {
        attempt_id: None,
        message_text: text.into(),
        frequency_offset: hz,
        qso_id: Some(qso.into()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

async fn pan73_coalesce(head: MessageType, inbox: &super::TxInbox, live: &[&str]) -> MessageType {
    let bus = MessageBus::new(16).unwrap();
    // PAN-147: bind the guard so it outlives every write inside this
    // helper's `.await` and cleans the path afterward.
    let audit_tmp = audit_tmp();
    coalesce_backlog_into(
        head,
        inbox,
        &bus,
        &std::sync::Arc::new(std::sync::RwLock::new(liveset(live))),
        &std::sync::Arc::new(std::sync::RwLock::new(HashSet::new())),
        4,
        &std::sync::Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new())),
        &pancetta_agent::audit::AuditLog::new(audit_tmp.path()),
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        None,
    )
    .await
}

/// PAN-73 follow-up: the message the drain stops at goes back where it
/// was, ahead of later traffic. It used to be re-sent to the back of the
/// channel, behind requests that arrived after it (main left ["C", "Tune"]).
#[tokio::test(flavor = "current_thread")]
async fn coalesce_puts_the_stop_message_back_in_arrival_order() {
    let (tx, rx) = crossbeam_channel::unbounded();
    for m in [
        pan73_tx("B", "qso-b", 1300.0),
        MessageType::TuneRequest {
            duration_secs: 5,
            tone_offset_hz: 1500.0,
        },
        pan73_tx("C", "qso-c", 1600.0),
    ] {
        tx.send(ComponentMessage::new(
            ComponentId::Qso,
            ComponentId::Ft8Transmitter,
            m,
            Instant::now(),
        ))
        .unwrap();
    }
    let inbox = super::TxInbox::new(rx, Default::default());
    let _ = pan73_coalesce(
        pan73_tx("A", "qso-a", 1000.0),
        &inbox,
        &["qso-a", "qso-b", "qso-c"],
    )
    .await;
    let left: Vec<String> = std::iter::from_fn(|| inbox.try_recv().ok())
        .map(|m| match m.message_type {
            MessageType::TransmitRequest { message_text, .. } => message_text,
            MessageType::TuneRequest { .. } => "Tune".into(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(left, ["Tune", "C"]);
}

/// PAN-73 follow-up: held messages are older than the channel's, so the
/// coalescer folds them first and newest-per-QSO keeps the channel's.
#[tokio::test(flavor = "current_thread")]
async fn coalesce_folds_the_requeue_front_before_the_channel() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let inbox = super::TxInbox::new(rx, Default::default());
    inbox.requeue_front(vec![super::self_message(pan73_tx(
        "B -07", "qso-b", 1300.0,
    ))]);
    tx.send(ComponentMessage::new(
        ComponentId::Qso,
        ComponentId::Ft8Transmitter,
        pan73_tx("B R-09", "qso-b", 1300.0),
        Instant::now(),
    ))
    .unwrap();
    let result = pan73_coalesce(pan73_tx("A", "qso-a", 1000.0), &inbox, &["qso-a", "qso-b"]).await;
    let MessageType::MultiTransmitRequest { items, .. } = result else {
        panic!("expected a bundle, got {result:?}")
    };
    let b: Vec<&str> = items
        .iter()
        .filter(|i| i.qso_id.as_deref() == Some("qso-b"))
        .map(|i| i.message_text.as_str())
        .collect();
    assert_eq!(b, ["B R-09"], "the newer request for qso-b must win");
}

/// PAN-73 follow-up: a request the coalescer defers to keep consent
/// channels apart arrived before the non-TX message the drain stopped
/// at, so it must come out of the inbox first. Arm setup as in
/// `backlog_preserves_consent_channels_until_key_time`.
#[tokio::test(flavor = "current_thread")]
async fn consent_deferred_request_stays_ahead_of_the_stop_message() {
    use pancetta_agent::arm::{ArmState, ConsentChannel, VerifiedArmGrant};
    use std::sync::{Arc, Mutex, RwLock};

    let bus = MessageBus::new(16).unwrap();
    let now_ms = UtcTimestamp::now().as_millisecond();
    let mut state = ArmState::new();
    state.set_channel_consent(ConsentChannel::StationAgent, true, now_ms);
    state.set_channel_consent(ConsentChannel::WsjtxUdp, true, now_ms);
    state.arm(
        VerifiedArmGrant {
            operator_callsign: "K5ARH".into(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "channel-backlog-stop-order".into(),
            client_key_id: "client-a".into(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(Mutex::new(state));
    let request = |udp: bool| MessageType::TransmitRequest {
        attempt_id: None,
        message_text: if udp { "UDP" } else { "AGENT" }.into(),
        frequency_offset: if udp { 1000.0 } else { 1300.0 },
        qso_id: Some(if udp { "udp-qso" } else { "agent-qso" }.into()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Remote,
        remote_client_key_id: if udp { None } else { Some("client-a".into()) },
    };
    let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
    for m in [
        request(false),
        MessageType::TuneRequest {
            duration_secs: 5,
            tone_offset_hz: 1500.0,
        },
    ] {
        backlog_tx
            .send(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Ft8Transmitter,
                m,
                Instant::now(),
            ))
            .unwrap();
    }
    let backlog_inbox = super::TxInbox::new(backlog_rx, Default::default());
    let audit_tmp = audit_tmp();
    let result = coalesce_backlog_into(
        request(true),
        &backlog_inbox,
        &bus,
        &Arc::new(RwLock::new(liveset(&["udp-qso", "agent-qso"]))),
        &Arc::new(RwLock::new(HashSet::new())),
        2,
        &arm,
        &pancetta_agent::audit::AuditLog::new(audit_tmp.path()),
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
        None,
    )
    .await;
    assert!(
        matches!(&result, MessageType::TransmitRequest { message_text, .. } if message_text == "UDP"),
        "different consent channels must remain separate: {result:?}"
    );
    let left: Vec<String> = std::iter::from_fn(|| backlog_inbox.try_recv().ok())
        .map(|m| match m.message_type {
            MessageType::TransmitRequest { message_text, .. } => message_text,
            MessageType::TuneRequest { .. } => "Tune".into(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(left, ["AGENT", "Tune"]);
}

fn liveset(ids: &[&str]) -> HashSet<String> {
    ids.iter()
        .map(|s| super::super::active_tx_qso_key(s))
        .collect()
}

#[test]
fn single_request_passthrough_unchanged() {
    // The no-backlog case: one request in, one retained out, zero reduced.
    let live = liveset(&["qso-a"]);
    let out =
        coalesce_transmit_requests(vec![entry("CQ", Some("qso-a"))], UNBOUNDED, live_in(&live));
    assert_eq!(out.retained, vec![entry("CQ", Some("qso-a"))]);
    assert_eq!(out.coalesced, 0);
    assert_eq!(out.dropped_terminal, 0);
    assert_eq!(out.truncated, 0);
    assert!(out.is_noop());
}

#[test]
fn newest_per_qso_id_wins() {
    // Three keep-calls for the SAME live QSO collapse to the LAST one.
    let live = liveset(&["qso-a"]);
    let out = coalesce_transmit_requests(
        vec![
            entry("OLD-1", Some("qso-a")),
            entry("OLD-2", Some("qso-a")),
            entry("NEWEST", Some("qso-a")),
        ],
        UNBOUNDED,
        live_in(&live),
    );
    assert_eq!(out.retained, vec![entry("NEWEST", Some("qso-a"))]);
    assert_eq!(out.coalesced, 2);
    assert_eq!(out.dropped_terminal, 0);
    assert_eq!(out.truncated, 0);
}

#[test]
fn newest_per_qso_id_is_case_insensitive() {
    // Mixed-case ids for the same QSO coalesce together (canonical key).
    let live = liveset(&["qso-a"]);
    let out = coalesce_transmit_requests(
        vec![entry("OLD", Some("QSO-A")), entry("NEWEST", Some("qso-a"))],
        UNBOUNDED,
        live_in(&live),
    );
    assert_eq!(out.retained.len(), 1);
    assert_eq!(out.retained[0].message_text, "NEWEST");
    assert_eq!(out.coalesced, 1);
}

#[test]
fn terminal_qso_requests_dropped() {
    // qso-dead is not in the live set → its requests are dropped; the live
    // QSO survives.
    let live = liveset(&["qso-a"]);
    let out = coalesce_transmit_requests(
        vec![
            entry("DEAD-1", Some("qso-dead")),
            entry("LIVE", Some("qso-a")),
            entry("DEAD-2", Some("qso-dead")),
        ],
        UNBOUNDED,
        live_in(&live),
    );
    assert_eq!(out.retained, vec![entry("LIVE", Some("qso-a"))]);
    assert_eq!(out.dropped_terminal, 2);
    assert_eq!(out.coalesced, 0);
}

#[test]
fn manual_none_entries_preserved_and_never_coalesced() {
    // Two distinct manual sends (qso_id == None) must BOTH survive — they
    // are never coalesced into each other, and never gated by liveness.
    // Distinct, well-separated frequencies so the FQ-F4/TX-F6 bundle-
    // frequency check (a SEPARATE reduction) doesn't also fire here.
    let live = liveset(&[]); // empty: no QSO is "live"
    let out = coalesce_transmit_requests(
        vec![
            entry_freq("MANUAL-1", None, 1000.0),
            entry_freq("MANUAL-2", None, 3000.0),
        ],
        UNBOUNDED,
        live_in(&live),
    );
    assert_eq!(out.retained.len(), 2);
    assert_eq!(out.retained[0].message_text, "MANUAL-1");
    assert_eq!(out.retained[1].message_text, "MANUAL-2");
    assert_eq!(out.coalesced, 0);
    assert_eq!(out.dropped_terminal, 0);
}

#[test]
fn manual_send_survives_keepcall_flood() {
    // A flood of keep-calls for one QSO plus an operator manual send: the
    // QSO collapses to its newest frame, and the manual send is retained.
    // MANUAL is on a well-separated frequency so the FQ-F4/TX-F6 bundle-
    // frequency check (a SEPARATE reduction) doesn't also fire here.
    let live = liveset(&["qso-a"]);
    let out = coalesce_transmit_requests(
        vec![
            entry("KC-1", Some("qso-a")),
            entry("KC-2", Some("qso-a")),
            entry_freq("MANUAL", None, 3000.0),
            entry("KC-3", Some("qso-a")),
        ],
        UNBOUNDED,
        live_in(&live),
    );
    // QSO stream first (first-seen), manual last.
    assert_eq!(out.retained.len(), 2);
    assert_eq!(out.retained[0].message_text, "KC-3"); // newest for qso-a
    assert_eq!(out.retained[1].message_text, "MANUAL");
    assert_eq!(out.coalesced, 2);
}

#[test]
fn cap_enforced_with_truncation_count() {
    // More distinct live streams than the cap → truncated to the cap,
    // first-seen streams kept, overflow counted. Distinct, well-separated
    // frequencies (100 Hz apart) so the FQ-F4/TX-F6 bundle-frequency
    // check (a SEPARATE reduction from the cap) doesn't also fire here —
    // this test is purely about the cap/truncation count.
    let ids: Vec<String> = (0..MAX_RETAINED_TX_STREAMS + 3)
        .map(|i| format!("qso-{i}"))
        .collect();
    let live: HashSet<String> = ids
        .iter()
        .map(|s| super::super::active_tx_qso_key(s))
        .collect();
    let drained: Vec<CoalesceEntry> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| entry_freq(id, Some(id), 1000.0 + i as f64 * 100.0))
        .collect();
    let out = coalesce_transmit_requests(drained, UNBOUNDED, live_in(&live));
    assert_eq!(out.retained.len(), MAX_RETAINED_TX_STREAMS);
    assert_eq!(out.truncated, 3);
    // First-seen streams kept (FIFO fairness): qso-0..qso-7.
    assert_eq!(out.retained[0].message_text, "qso-0");
    assert_eq!(
        out.retained[MAX_RETAINED_TX_STREAMS - 1].message_text,
        format!("qso-{}", MAX_RETAINED_TX_STREAMS - 1)
    );
}

#[test]
fn distinct_live_qsos_all_retained_under_cap() {
    // Two distinct live QSOs, one frame each, under cap → both retained,
    // nothing reduced (is_noop). Distinct, well-separated frequencies so
    // the FQ-F4/TX-F6 bundle-frequency check doesn't also fire.
    let live = liveset(&["qso-a", "qso-b"]);
    let out = coalesce_transmit_requests(
        vec![
            entry("A", Some("qso-a")),
            entry_freq("B", Some("qso-b"), 3000.0),
        ],
        UNBOUNDED,
        live_in(&live),
    );
    assert_eq!(out.retained.len(), 2);
    assert!(out.is_noop());
}

#[test]
fn empty_input_yields_empty_retained() {
    let live = liveset(&[]);
    let out = coalesce_transmit_requests(Vec::new(), UNBOUNDED, live_in(&live));
    assert!(out.retained.is_empty());
    assert!(out.is_noop());
}

#[test]
fn all_terminal_yields_empty_retained_with_drop_count() {
    let live = liveset(&[]); // nothing live
    let out = coalesce_transmit_requests(
        vec![entry("D1", Some("qso-x")), entry("D2", Some("qso-y"))],
        UNBOUNDED,
        live_in(&live),
    );
    assert!(out.retained.is_empty());
    assert_eq!(out.dropped_terminal, 2);
    assert!(!out.is_noop());
}

// ── Bundle-parity conflict exclusion (Batch 2, part 2) ───────────────────

fn entry_with_parity(
    text: &str,
    qso_id: Option<&str>,
    tx_parity: Option<pancetta_core::slot::SlotParity>,
) -> CoalesceEntry {
    CoalesceEntry {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: 1000.0,
        qso_id: qso_id.map(|s| s.to_string()),
        tx_parity,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

/// Like [`entry_with_parity`] but with an explicit `frequency_offset` —
/// see [`entry_freq`]'s doc comment for why some multi-entry tests need
/// distinct frequencies now that the FQ-F4/TX-F6 bundle-frequency
/// exclusion pass is a separate reduction alongside the parity one.
fn entry_with_parity_freq(
    text: &str,
    qso_id: Option<&str>,
    tx_parity: Option<pancetta_core::slot::SlotParity>,
    freq: f64,
) -> CoalesceEntry {
    CoalesceEntry {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: freq,
        qso_id: qso_id.map(|s| s.to_string()),
        tx_parity,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

/// Three distinct-QSO streams survive coalescing: two share `Some(Even)`
/// and one is `Some(Odd)`. The bundle must NOT silently coerce the Odd
/// outlier onto the Even anchor (that would put it in the wrong slot
/// window for its actual partner) — it is excluded from `retained` for
/// this cycle and counted in `parity_excluded`, keeping only the two
/// agreeing Even entries.
#[test]
fn disagreeing_parity_stream_is_excluded_not_coerced() {
    use pancetta_core::slot::SlotParity;

    // A and B (both Even, both expected to survive together) get
    // distinct, well-separated frequencies so the FQ-F4/TX-F6
    // bundle-frequency check (a separate reduction) doesn't also fire.
    // C's frequency doesn't matter — it's excluded by the parity check
    // before the frequency pass ever sees it.
    let live = liveset(&["qso-a", "qso-b", "qso-c"]);
    let out = coalesce_transmit_requests(
        vec![
            entry_with_parity("A", Some("qso-a"), Some(SlotParity::Even)),
            entry_with_parity_freq("B", Some("qso-b"), Some(SlotParity::Even), 3000.0),
            entry_with_parity("C", Some("qso-c"), Some(SlotParity::Odd)),
        ],
        UNBOUNDED,
        live_in(&live),
    );
    assert_eq!(
        out.retained.len(),
        2,
        "the Odd outlier must be excluded, keeping only the two Even entries"
    );
    assert_eq!(out.retained[0].message_text, "A");
    assert_eq!(out.retained[1].message_text, "B");
    assert!(
        out.retained
            .iter()
            .all(|e| e.tx_parity == Some(SlotParity::Even)),
        "every surviving entry must agree with the bundle anchor"
    );
    assert_eq!(
        out.parity_excluded, 1,
        "exactly one stream excluded for a parity conflict"
    );
    assert!(
        !out.is_noop(),
        "a parity exclusion must not be reported as a no-op"
    );
}

/// A `None` (no-preference) `tx_parity` must NOT be treated as
/// disagreeing with a concrete anchor — it rides along unchanged, exactly
/// like the pre-existing manual/None-keyed behavior.
#[test]
fn none_parity_stream_is_not_treated_as_a_conflict() {
    use pancetta_core::slot::SlotParity;

    // Distinct, well-separated frequencies so the FQ-F4/TX-F6
    // bundle-frequency check doesn't also fire — this test is purely
    // about parity, and both entries are expected to survive together.
    let live = liveset(&["qso-a", "qso-b"]);
    let out = coalesce_transmit_requests(
        vec![
            entry_with_parity("A", Some("qso-a"), Some(SlotParity::Even)),
            entry_with_parity_freq("B", Some("qso-b"), None, 3000.0),
        ],
        UNBOUNDED,
        live_in(&live),
    );
    assert_eq!(
        out.retained.len(),
        2,
        "a None-parity stream must not be excluded"
    );
    assert_eq!(out.parity_excluded, 0);
    assert!(out.is_noop());
}

// ── max_concurrent_qsos cap on the backlog coalescer ─────────────────────

#[test]
fn concurrent_qso_cap_truncates_qso_streams_below_hard_backstop() {
    // Three distinct live QSOs land in the same pre-PTT window, but the
    // station is configured for only 2 concurrent QSOs — well under the
    // MAX_RETAINED_TX_STREAMS hard backstop. The 3rd (later-seen) stream
    // must be excluded, not silently allowed through just because it's
    // under the hard cap.
    let live = liveset(&["qso-a", "qso-b", "qso-c"]);
    let out = coalesce_transmit_requests(
        vec![
            entry_freq("A", Some("qso-a"), 1000.0),
            entry_freq("B", Some("qso-b"), 1300.0),
            entry_freq("C", Some("qso-c"), 1600.0),
        ],
        2,
        live_in(&live),
    );
    assert_eq!(
        out.retained.len(),
        2,
        "only 2 of 3 concurrent QSO streams may survive under the configured cap"
    );
    assert_eq!(out.retained[0].message_text, "A");
    assert_eq!(out.retained[1].message_text, "B");
    assert_eq!(out.truncated, 1);
    assert_eq!(
        out.qso_cap, 2,
        "the effective cap actually applied must be exposed"
    );
    assert_eq!(
        out.truncated_entries.len(),
        1,
        "the truncated stream's own entry must be exposed so the caller can report a \
             failed TransmitComplete for it"
    );
    assert_eq!(out.truncated_entries[0].message_text, "C");
    assert!(!out.is_noop());
}

#[test]
fn concurrent_qso_cap_never_excludes_manual_sends() {
    // max_concurrent_qsos == 1, but manual/None-keyed sends (tune,
    // free-text) are not concurrent QSOs and must never be excluded by
    // this cap — only the QSO-keyed portion is bounded by it.
    let live = liveset(&["qso-a"]);
    let out = coalesce_transmit_requests(
        vec![
            entry_freq("QSO", Some("qso-a"), 1000.0),
            entry_freq("MANUAL-1", None, 1300.0),
            entry_freq("MANUAL-2", None, 1600.0),
        ],
        1,
        live_in(&live),
    );
    assert_eq!(
        out.retained.len(),
        3,
        "manual sends must survive even when the QSO cap is 1"
    );
    assert_eq!(out.truncated, 0);
    assert!(out.truncated_entries.is_empty());
}

/// PR #348 review (Codex P2): applying the QSO cap BEFORE the parity/
/// frequency conflict passes could truncate away a stream that would
/// have survived, while keeping one that gets excluded moments later for
/// a conflict — missing an otherwise-valid bundle combination. With
/// cap=2 and ordered offsets A=1000, B=1050 (50 Hz from A — a genuine
/// frequency conflict), C=1300 (300 Hz from A — fine): the OLD
/// cap-first order kept [A, B], then excluded B for the conflict,
/// leaving only A. Conflict-filtering first must instead exclude B
/// immediately and let the cap see [A, C] — a valid two-stream bundle.
#[test]
fn conflicting_entries_filtered_before_cap_not_after() {
    let live = liveset(&["qso-a", "qso-b", "qso-c"]);
    let out = coalesce_transmit_requests(
        vec![
            entry_freq("A", Some("qso-a"), 1000.0),
            entry_freq("B", Some("qso-b"), 1050.0),
            entry_freq("C", Some("qso-c"), 1300.0),
        ],
        2,
        live_in(&live),
    );
    assert_eq!(
        out.freq_excluded, 1,
        "B must be excluded for the frequency conflict with A"
    );
    assert_eq!(
        out.retained.len(),
        2,
        "the cap must not additionally truncate a valid post-conflict-filter bundle"
    );
    assert_eq!(out.retained[0].message_text, "A");
    assert_eq!(
        out.retained[1].message_text, "C",
        "C must survive as the valid second stream once B (not C) is filtered for the \
             frequency conflict"
    );
    assert_eq!(
        out.truncated, 0,
        "no QSO cap truncation should occur once B is correctly excluded by the conflict pass"
    );
}

/// PR #348 review round 2 (Codex P2): a manual send must be checked
/// against the FINAL surviving QSO set (post conflict-filter, post-cap),
/// never against a QSO stream that's doomed to be cap-truncated anyway.
/// cap=1, QSO A(1000) survives, QSO B(1300) is over-cap and will be
/// truncated, manual(1320) conflicts with B (20 Hz apart) but NOT with A
/// (320 Hz apart) — the manual send must survive alongside A.
#[test]
fn manual_send_not_excluded_by_a_qso_stream_that_gets_cap_truncated() {
    let live = liveset(&["qso-a", "qso-b"]);
    let out = coalesce_transmit_requests(
        vec![
            entry_freq("A", Some("qso-a"), 1000.0),
            entry_freq("B", Some("qso-b"), 1300.0),
            entry_freq("MANUAL", None, 1320.0),
        ],
        1,
        live_in(&live),
    );
    assert_eq!(
        out.truncated, 1,
        "B must be cap-truncated (cap=1, A wins first-seen)"
    );
    assert_eq!(
        out.truncated_entries[0].message_text, "B",
        "B is the one that's cap-truncated, not excluded by a frequency conflict"
    );
    assert_eq!(
        out.freq_excluded, 0,
        "the manual send is compatible with the SURVIVING QSO set (A) — it must not be \
             excluded just because it conflicted with B, which never survives anyway"
    );
    assert_eq!(
        out.retained.len(),
        2,
        "A and the manual send must both survive"
    );
    assert_eq!(out.retained[0].message_text, "A");
    assert_eq!(out.retained[1].message_text, "MANUAL");
}

/// PR #348 review (Codex P1): a cap-truncated QSO-keyed stream already
/// has live call-bookkeeping (call_count, last_call_at, Sent timeline)
/// advanced as though it would transmit — dropping it silently would let
/// that bookkeeping drift from what actually went on air. Prove the
/// wiring end to end: `coalesce_backlog_into` must report a failed
/// `TransmitComplete` for every entry the cap truncates.
#[tokio::test(flavor = "current_thread")]
async fn backlog_into_reports_transmit_complete_for_cap_truncated_streams() {
    use crate::message_bus::MessageBus;
    use std::sync::Arc;

    let bus = MessageBus::new(16).unwrap();
    let (_autonomous_tx, autonomous_rx) =
        bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
    let active_tx_qsos = Arc::new(std::sync::RwLock::new(liveset(&[
        "qso-a", "qso-b", "qso-c",
    ])));

    let head = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "A".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-a".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    for (text, id, freq) in [("B", "qso-b", 1300.0), ("C", "qso-c", 1600.0)] {
        backlog_tx
            .send(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    attempt_id: None,
                    message_text: text.to_string(),
                    frequency_offset: freq,
                    qso_id: Some(id.to_string()),
                    tx_parity: None,
                    origin: crate::message_bus::TxOrigin::Local,
                    remote_client_key_id: None,
                },
                Instant::now(),
            ))
            .unwrap();
    }

    // max_concurrent_qsos = 1: only "A" survives, "B" and "C" are cap-truncated.
    // Every entry here is Local, so the identity-bound arm gate never
    // consults this — an unarmed ArmState is the neutral choice.
    let arm = Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new()));
    let audit_tmp = audit_tmp();
    let audit_log = pancetta_agent::audit::AuditLog::new(audit_tmp.path());
    let display_feed_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backlog_inbox = super::TxInbox::new(backlog_rx.clone(), Default::default());
    let _ = coalesce_backlog_into(
        head,
        &backlog_inbox,
        &bus,
        &active_tx_qsos,
        &Arc::new(std::sync::RwLock::new(HashSet::new())),
        1,
        &arm,
        &audit_log,
        &display_feed_enabled,
        None,
    )
    .await;

    let mut got = Vec::new();
    while let Ok(msg) = autonomous_rx.try_recv() {
        got.push(msg);
    }
    assert_eq!(
        got.len(),
        2,
        "a TransmitComplete must be reported for each of the 2 cap-truncated streams"
    );
    for msg in &got {
        match &msg.message_type {
            MessageType::TransmitComplete { success, .. } => {
                assert!(
                    !success,
                    "a cap-truncated stream's TransmitComplete must report failure"
                );
            }
            other => panic!("expected TransmitComplete, got {other:?}"),
        }
    }
    let mut notices = Vec::new();
    while let Ok(msg) = qso_rx.try_recv() {
        if let MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
            attempt_id: _,
            qso_id,
            message_text,
            replaced: None,
        }) = msg.message_type
        {
            notices.push((qso_id, message_text));
        } else {
            panic!("unexpected QSO notification");
        }
    }
    notices.sort();
    assert_eq!(
        notices,
        vec![
            ("qso-b".to_string(), "B".to_string()),
            ("qso-c".to_string(), "C".to_string())
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn backlog_preserves_consent_channels_until_key_time() {
    use pancetta_agent::arm::{ArmState, ConsentChannel, VerifiedArmGrant};
    use std::sync::{Arc, Mutex, RwLock};

    // Exercise both FIFO orders: neither channel may borrow the other's
    // consent when the TX worker rechecks after coalescing.
    for udp_first in [true, false] {
        let bus = MessageBus::new(16).unwrap();
        let (_, deferred_rx) = bus
            .create_channel(ComponentId::Ft8Transmitter)
            .await
            .unwrap();
        let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
        let now_ms = UtcTimestamp::now().as_millisecond();
        let mut state = ArmState::new();
        state.set_channel_consent(ConsentChannel::StationAgent, true, now_ms);
        state.set_channel_consent(ConsentChannel::WsjtxUdp, true, now_ms);
        state.arm(
            VerifiedArmGrant {
                operator_callsign: "K5ARH".into(),
                ttl_ms: 120_000,
                armed_until_ms: now_ms + 120_000,
                capability_exp_ms: now_ms + 120_000 + 3_600_000,
                tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
                effective_ttl_ms: 120_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "channel-backlog".into(),
                client_key_id: "client-a".into(),
            },
            pancetta_agent::arm::ArmTime::now(),
        );
        let arm = Arc::new(Mutex::new(state));
        let request = |udp| MessageType::TransmitRequest {
            attempt_id: None,
            message_text: if udp { "UDP" } else { "AGENT" }.into(),
            frequency_offset: if udp { 1000.0 } else { 1300.0 },
            qso_id: Some(if udp { "udp-qso" } else { "agent-qso" }.into()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: if udp { None } else { Some("client-a".into()) },
        };
        backlog_tx
            .send(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Ft8Transmitter,
                request(!udp_first),
                Instant::now(),
            ))
            .unwrap();
        let backlog_inbox = super::TxInbox::new(backlog_rx.clone(), Default::default());
        let audit_tmp = audit_tmp();
        let result = coalesce_backlog_into(
            request(udp_first),
            &backlog_inbox,
            &bus,
            &Arc::new(RwLock::new(liveset(&["udp-qso", "agent-qso"]))),
            &Arc::new(RwLock::new(HashSet::new())),
            2,
            &arm,
            &pancetta_agent::audit::AuditLog::new(audit_tmp.path()),
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert!(
            matches!(result, MessageType::TransmitRequest { .. }),
            "different consent channels must remain separate: {result:?}"
        );
        let deferred = backlog_inbox
            .try_recv()
            .expect("other channel must be requeued");
        assert!(backlog_inbox.try_recv().is_err());
        // Held by the worker, not sent back through the bus.
        assert!(deferred_rx.is_empty());

        // Revoke only UDP after the snapshot. The actual final gate must
        // refuse its frame while still permitting the station-agent frame.
        arm.lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::WsjtxUdp, false, now_ms);
        let mut payloads = Vec::new();
        for message in [result, deferred.message_type] {
            let MessageType::TransmitRequest {
                message_text,
                remote_client_key_id,
                origin,
                ..
            } = message
            else {
                panic!("expected a separate request");
            };
            assert_eq!(origin, crate::message_bus::TxOrigin::Remote);
            assert_eq!(
                super::remote_tx_permitted_for(
                    &arm,
                    pancetta_agent::arm::ArmTime::now,
                    remote_client_key_id.as_deref()
                ),
                message_text == "AGENT"
            );
            payloads.push(message_text);
        }
        payloads.sort();
        assert_eq!(payloads, ["AGENT", "UDP"]);
    }
}

/// PAN-136: a folded bundle's items keep the binding of the request each
/// came from, and the bundle-level binding is the fold of those items.
#[tokio::test(flavor = "current_thread")]
async fn backlog_bundle_items_carry_their_own_binding() {
    use pancetta_agent::arm::{ArmState, ConsentChannel, VerifiedArmGrant};
    use std::sync::{Arc, Mutex, RwLock};

    let bus = MessageBus::new(16).unwrap();
    let (_, _deferred_rx) = bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
    let now_ms = UtcTimestamp::now().as_millisecond();
    let mut state = ArmState::new();
    state.set_channel_consent(ConsentChannel::StationAgent, true, now_ms);
    state.arm(
        VerifiedArmGrant {
            operator_callsign: "K5ARH".into(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "pan136-item-binding".into(),
            client_key_id: "client-a".into(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(Mutex::new(state));

    // Head: a Local manual send. Backlog: a Remote/client-a QSO frame.
    let head = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "CQ K5ARH EM10".into(),
        frequency_offset: 900.0,
        qso_id: None,
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    backlog_tx
        .send(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: "SM2LIY K5ARH R-14".into(),
                frequency_offset: 1500.0,
                qso_id: Some("agent-qso".into()),
                tx_parity: None,
                origin: crate::message_bus::TxOrigin::Remote,
                remote_client_key_id: Some("client-a".into()),
            },
            Instant::now(),
        ))
        .unwrap();

    let backlog_inbox = super::TxInbox::new(backlog_rx, Default::default());
    let audit_tmp = audit_tmp();
    let result = coalesce_backlog_into(
        head,
        &backlog_inbox,
        &bus,
        &Arc::new(RwLock::new(liveset(&["agent-qso"]))),
        &Arc::new(RwLock::new(HashSet::new())),
        2,
        &arm,
        &pancetta_agent::audit::AuditLog::new(audit_tmp.path()),
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
        None,
    )
    .await;

    let MessageType::MultiTransmitRequest {
        items,
        origin,
        remote_client_key_id,
        ..
    } = result
    else {
        panic!("a Local manual send + an armed Remote QSO frame must fold: {result:?}");
    };
    assert_eq!(items.len(), 2);
    let manual = items.iter().find(|it| it.qso_id.is_none()).unwrap();
    assert_eq!(manual.origin, crate::message_bus::TxOrigin::Local);
    assert_eq!(manual.remote_client_key_id, None);
    let remote = items
        .iter()
        .find(|it| it.qso_id.as_deref() == Some("agent-qso"))
        .unwrap();
    assert_eq!(remote.origin, crate::message_bus::TxOrigin::Remote);
    assert_eq!(remote.remote_client_key_id.as_deref(), Some("client-a"));
    assert_eq!(origin, crate::message_bus::TxOrigin::Remote);
    assert_eq!(remote_client_key_id.as_deref(), Some("client-a"));
    assert_eq!(
        (origin, remote_client_key_id),
        super::super::fold_bundle_identity(items.iter())
    );
}

/// PAN-91 review follow-up: a backlog containing entries bound to TWO
/// different remote clients must never fold into one bundle authorized
/// under a single client's identity — the entry bound to whichever
/// client is NOT currently armed must be filtered out (and reported as
/// a failed `TransmitComplete`) before folding, not silently carried
/// along under the other entry's identity.
#[tokio::test(flavor = "current_thread")]
async fn backlog_into_drops_entries_bound_to_a_different_client_than_the_one_armed() {
    use crate::message_bus::MessageBus;
    use std::sync::Arc;

    let bus = MessageBus::new(16).unwrap();
    let (_autonomous_tx, autonomous_rx) =
        bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
    let active_tx_qsos = Arc::new(std::sync::RwLock::new(liveset(&["qso-a", "qso-b"])));

    // client-a is armed; client-b is not.
    let mut st = pancetta_agent::arm::ArmState::new();
    let now_ms = UtcTimestamp::now().as_millisecond();
    st.set_channel_consent(
        pancetta_agent::arm::ConsentChannel::StationAgent,
        true,
        now_ms,
    );
    st.set_channel_consent(pancetta_agent::arm::ConsentChannel::WsjtxUdp, true, now_ms);
    st.arm(
        pancetta_agent::arm::VerifiedArmGrant {
            operator_callsign: "K5ARH".to_string(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "mixed-bundle-jti".to_string(),
            client_key_id: "client-a".to_string(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(std::sync::Mutex::new(st));

    // Head entry: bound to client-a (armed) — must survive.
    let head = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "A".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-a".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Remote,
        remote_client_key_id: Some("client-a".to_string()),
    };
    // Backlogged entry: bound to client-b (NOT armed) — must be dropped,
    // never folded into a bundle that client-a's arm would authorize.
    backlog_tx
        .send(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: "B".to_string(),
                frequency_offset: 1300.0,
                qso_id: Some("qso-b".to_string()),
                tx_parity: None,
                origin: crate::message_bus::TxOrigin::Remote,
                remote_client_key_id: Some("client-b".to_string()),
            },
            Instant::now(),
        ))
        .unwrap();

    let audit_tmp = audit_tmp();
    let audit_log = pancetta_agent::audit::AuditLog::new(audit_tmp.path());
    let display_feed_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backlog_inbox = super::TxInbox::new(backlog_rx.clone(), Default::default());
    let result = coalesce_backlog_into(
        head,
        &backlog_inbox,
        &bus,
        &active_tx_qsos,
        &Arc::new(std::sync::RwLock::new(HashSet::new())),
        2,
        &arm,
        &audit_log,
        &display_feed_enabled,
        None,
    )
    .await;

    match result {
        MessageType::TransmitRequest {
            message_text,
            remote_client_key_id,
            ..
        } => {
            assert_eq!(
                message_text, "A",
                "only client-a's (armed) entry may survive"
            );
            assert_eq!(remote_client_key_id.as_deref(), Some("client-a"));
        }
        other => panic!(
            "expected a single TransmitRequest for client-a's surviving entry, got {other:?}"
        ),
    }

    // Round-9 review (Codex P2): the identity-denied reporting loop is
    // now a DETACHED `tokio::spawn`'d task (so a large stale backlog
    // can't delay returning the admitted frame above) — give the
    // current-thread runtime a chance to actually run it before
    // asserting on its output.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut got = Vec::new();
    while let Ok(msg) = autonomous_rx.try_recv() {
        got.push(msg);
    }
    assert_eq!(
        got.len(),
        1,
        "client-b's identity-denied entry must be reported as a failed TransmitComplete"
    );
    match &got[0].message_type {
        MessageType::TransmitComplete {
            success, qso_id, ..
        } => {
            assert!(!success);
            assert_eq!(qso_id.as_deref(), Some("qso-b"));
        }
        other => panic!("expected TransmitComplete, got {other:?}"),
    }
    let mut notices = Vec::new();
    while let Ok(msg) = qso_rx.try_recv() {
        if let MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
            attempt_id: _,
            qso_id,
            message_text,
            replaced: None,
        }) = msg.message_type
        {
            notices.push((qso_id, message_text));
        } else {
            panic!("unexpected QSO notification");
        }
    }
    notices.sort();
    assert_eq!(notices, vec![("qso-b".to_string(), "B".to_string())]);
}

/// PAN-75 (Codex P2 on PR #418): a pivot duplicate queued ahead of a
/// newer same-QSO request is consumed before coalescing, so its tombstone
/// clears and the frame the pivot replaced is rolled back.
#[tokio::test(flavor = "current_thread")]
async fn backlog_into_consumes_pivot_duplicates_before_coalescing() {
    use crate::message_bus::MessageBus;
    use std::sync::Arc;

    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous_rx) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
    let active_tx_qsos = Arc::new(std::sync::RwLock::new(liveset(&["qso-a"])));
    let arm = Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new()));
    let request = |text: &str| MessageType::TransmitRequest {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-a".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    backlog_tx
        .send(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Ft8Transmitter,
            request("K1ABC N0CALL RR73"),
            Instant::now(),
        ))
        .unwrap();

    let key = super::super::active_tx_qso_key("qso-a");
    let mut pivoted_once = std::collections::HashMap::new();
    pivoted_once.insert(key.clone(), ("K1ABC N0CALL R-07".to_string(), 1000.0));
    let replaced = pancetta_qso::UnsentAttemptPayload {
        frequency_hz: 1000.0,
        remote_binding: Some((false, None)),
    };
    let mut pivoted_from = PivotReplaced::new();
    pivoted_from.insert(
        key.clone(),
        ("K1ABC N0CALL -07".to_string(), replaced.clone(), None),
    );

    let audit_tmp = audit_tmp();
    let audit_log = pancetta_agent::audit::AuditLog::new(audit_tmp.path());
    let display_feed_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backlog_inbox = super::TxInbox::new(backlog_rx.clone(), Default::default());
    let result = coalesce_backlog_into(
        request("K1ABC N0CALL R-07"),
        &backlog_inbox,
        &bus,
        &active_tx_qsos,
        &Arc::new(std::sync::RwLock::new(HashSet::new())),
        2,
        &arm,
        &audit_log,
        &display_feed_enabled,
        Some((&mut pivoted_once, &mut pivoted_from)),
    )
    .await;

    assert!(matches!(
        result,
        MessageType::TransmitRequest { ref message_text, .. } if message_text == "K1ABC N0CALL RR73"
    ));
    assert!(pivoted_once.is_empty() && pivoted_from.is_empty());
    assert!(matches!(
        qso_rx.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref message_text, replaced: Some(ref payload), .. })
            if message_text == "K1ABC N0CALL -07" && *payload == replaced
    ));
    // PAN-185: the pivoted frame keyed and reported; the absorbed
    // duplicate tells Autonomous nothing.
    assert!(autonomous_rx.try_recv().is_err());
}

fn entry_parity(
    text: &str,
    qso_id: &str,
    freq: f64,
    parity: pancetta_core::slot::SlotParity,
) -> CoalesceEntry {
    CoalesceEntry {
        tx_parity: Some(parity),
        ..entry_freq(text, Some(qso_id), freq)
    }
}

/// PAN-185: the coalescer hands back the entries it dropped as terminal
/// or excluded for parity/frequency; a request coalesced into a newer one
/// for the same QSO is in neither.
#[test]
fn pan185_coalescer_returns_terminal_and_excluded_entries() {
    use pancetta_core::slot::SlotParity;
    let live = liveset(&["a", "b", "c"]);
    let out = coalesce_transmit_requests_with_fox_priority(
        vec![
            entry_parity("A1", "a", 1000.0, SlotParity::Even),
            entry_freq("T", Some("t"), 1300.0),
            entry_parity("B", "b", 1600.0, SlotParity::Odd),
            entry_freq("C", Some("c"), 1010.0),
            entry_parity("A2", "a", 1000.0, SlotParity::Even),
        ],
        UNBOUNDED,
        live_in(&live),
        |_| false,
    );
    assert_eq!(texts(&out.retained), vec!["A2"]);
    assert_eq!(out.coalesced, 1);
    assert_eq!(texts(&out.dropped_terminal_entries), vec!["T"]);
    assert_eq!(out.dropped_terminal, 1);
    assert_eq!(texts(&out.excluded_entries), vec!["B", "C"]);
    assert_eq!((out.parity_excluded, out.freq_excluded), (1, 1));
}

/// PAN-279: the coalescer returns the older same-QSO entry it folded,
/// and still counts it.
#[test]
fn pan279_coalescer_returns_coalesced_entries() {
    use pancetta_core::slot::SlotParity;
    let live = liveset(&["a", "b", "c"]);
    let out = coalesce_transmit_requests_with_fox_priority(
        vec![
            entry_parity("A1", "a", 1000.0, SlotParity::Even),
            entry_parity("B", "b", 1600.0, SlotParity::Odd),
            entry_parity("A2", "a", 1000.0, SlotParity::Even),
        ],
        UNBOUNDED,
        live_in(&live),
        |_| false,
    );
    assert_eq!(texts(&out.retained), vec!["A2"]);
    assert_eq!(out.coalesced, 1);
    assert_eq!(texts(&out.coalesced_entries), vec!["A1"]);
}

/// Drain head A1 (qso a, Even) with a backlog of T (terminal), B (qso b,
/// Odd, parity-excluded), C (qso c, Even, freq-excluded) and A2 (qso a,
/// Even). Returns the worker's request, Autonomous `(qso, text, attempt)`
/// failures, Qso `(qso, text, attempt)` not-sent reports, and whether the
/// inbox is empty afterwards.
/// `(qso, text, attempt)` rows a settlement reported.
type Pan279Rows = Vec<(String, String, Option<pancetta_qso::TxAttemptId>)>;

async fn pan279_settle_backlog(
    a1_attempt: Option<u64>,
) -> (MessageType, Pan279Rows, Pan279Rows, bool) {
    use crate::message_bus::MessageBus;
    use pancetta_core::slot::SlotParity;
    use std::sync::Arc;

    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous_rx) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
    let active_tx_qsos = Arc::new(std::sync::RwLock::new(liveset(&["a", "b", "c"])));
    let arm = Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new()));
    let request =
        |text: &str, qso: &str, hz: f64, parity: Option<SlotParity>, attempt: Option<u64>| {
            MessageType::TransmitRequest {
                attempt_id: attempt.map(pancetta_qso::TxAttemptId),
                message_text: text.to_string(),
                frequency_offset: hz,
                qso_id: Some(qso.to_string()),
                tx_parity: parity,
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            }
        };
    for message_type in [
        request("T", "t", 1300.0, None, Some(4)),
        request("B", "b", 1600.0, Some(SlotParity::Odd), Some(5)),
        request("C", "c", 1020.0, Some(SlotParity::Even), Some(6)),
        request("A2", "a", 1000.0, Some(SlotParity::Even), Some(7)),
    ] {
        backlog_tx
            .send(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Ft8Transmitter,
                message_type,
                Instant::now(),
            ))
            .unwrap();
    }
    let audit_tmp = audit_tmp();
    let audit_log = pancetta_agent::audit::AuditLog::new(audit_tmp.path());
    let display_feed_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backlog_inbox = super::TxInbox::new(backlog_rx.clone(), Default::default());
    let result = coalesce_backlog_into(
        request("A1", "a", 1000.0, Some(SlotParity::Even), a1_attempt),
        &backlog_inbox,
        &bus,
        &active_tx_qsos,
        &Arc::new(std::sync::RwLock::new(HashSet::new())),
        UNBOUNDED,
        &arm,
        &audit_log,
        &display_feed_enabled,
        None,
    )
    .await;
    let mut autonomous: Vec<_> = std::iter::from_fn(|| autonomous_rx.try_recv().ok())
        .map(|m| match m.message_type {
            MessageType::TransmitComplete {
                success: false,
                message_text,
                qso_id: Some(id),
                attempt_id,
                ..
            } => (id, message_text, attempt_id),
            other => panic!("expected a failed TransmitComplete, got {other:?}"),
        })
        .collect();
    autonomous.sort_by(|a, b| a.0.cmp(&b.0));
    let mut qso: Vec<_> = std::iter::from_fn(|| qso_rx.try_recv().ok())
        .map(|m| match m.message_type {
            MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
                qso_id,
                message_text,
                attempt_id,
                replaced: None,
            }) => (qso_id, message_text, attempt_id),
            other => panic!("expected TxFrameNotSent, got {other:?}"),
        })
        .collect();
    qso.sort_by(|a, b| a.0.cmp(&b.0));
    let inbox_empty = backlog_inbox.try_recv().is_err();
    (result, autonomous, qso, inbox_empty)
}

/// PAN-279 (replaces PAN-185's Autonomous-only test): terminal and
/// parity/frequency-excluded entries settle with both consumers under
/// their own attempt ids, and the folded older A1 rolls back its own
/// attempt with the QSO engine only; A2 owns qso a's Autonomous outcome.
/// Nothing is requeued.
#[tokio::test]
async fn pan279_backlog_settles_excluded_entries_with_both_consumers() {
    use pancetta_qso::TxAttemptId;
    let (result, autonomous, qso, inbox_empty) = pan279_settle_backlog(Some(3)).await;
    assert!(matches!(
        result,
        MessageType::TransmitRequest { ref message_text, .. } if message_text == "A2"
    ));
    let row = |q: &str, t: &str, a: u64| (q.to_string(), t.to_string(), Some(TxAttemptId(a)));
    assert_eq!(
        autonomous,
        vec![row("b", "B", 5), row("c", "C", 6), row("t", "T", 4)]
    );
    assert_eq!(
        qso,
        vec![
            row("a", "A1", 3),
            row("b", "B", 5),
            row("c", "C", 6),
            row("t", "T", 4)
        ]
    );
    assert!(inbox_empty, "excluded entries are settled, not requeued");
}

/// PAN-279: a folded older frame without an attempt id stays silent: a
/// text match could roll back the surviving frame's attempt.
#[tokio::test]
async fn pan279_coalesced_frame_without_an_attempt_stays_silent() {
    let (_, _, qso, _) = pan279_settle_backlog(None).await;
    assert!(
        qso.iter().all(|(id, ..)| id != "a"),
        "no rollback for the attempt-less folded A1: {qso:?}"
    );
    assert_eq!(qso.len(), 3);
}

// ── PAN-76: Fox-admitted streams keep their slots after Fox mode is off ──

fn texts(entries: &[CoalesceEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.message_text.as_str()).collect()
}

/// The ticket's bug: Fox mode is off again, so the effective cap is back
/// to 1, but the three Hound exchanges Fox mode admitted are still live.
/// None of them may be truncated.
#[test]
fn pan76_fox_admitted_streams_survive_the_restored_normal_cap() {
    let live = liveset(&["h1", "h2", "h3"]);
    let fox = liveset(&["h1", "h2", "h3"]);
    let out = coalesce_transmit_requests_with_fox_priority(
        vec![
            entry_freq("H1", Some("h1"), 1000.0),
            entry_freq("H2", Some("h2"), 1300.0),
            entry_freq("H3", Some("h3"), 1600.0),
        ],
        1,
        live_in(&live),
        |k: &str| fox.contains(k),
    );
    assert_eq!(texts(&out.retained), vec!["H1", "H2", "H3"]);
    assert_eq!(out.truncated, 0);
    assert!(out.truncated_entries.is_empty());
    assert_eq!(out.qso_cap, 3);
}

/// An ordinary stream seen before the Hounds must not take a Hound's slot:
/// the Hounds are ordered first, so the cap truncates the ordinary one.
#[test]
fn pan76_ordinary_stream_seen_first_is_truncated_not_a_hound() {
    let live = liveset(&["o", "h1", "h2", "h3"]);
    let fox = liveset(&["h1", "h2", "h3"]);
    let out = coalesce_transmit_requests_with_fox_priority(
        vec![
            entry_freq("O", Some("o"), 700.0),
            entry_freq("H1", Some("h1"), 1000.0),
            entry_freq("H2", Some("h2"), 1300.0),
            entry_freq("H3", Some("h3"), 1600.0),
        ],
        1,
        live_in(&live),
        |k: &str| fox.contains(k),
    );
    assert_eq!(texts(&out.retained), vec!["H1", "H2", "H3"]);
    assert_eq!(out.truncated, 1);
    assert_eq!(texts(&out.truncated_entries), vec!["O"]);
    assert_eq!(out.qso_cap, 3);
}

/// Round 5's regression in its narrower form: Hounds idling in their
/// completed grace (in the Fox set, no frames this cycle) must lend no
/// capacity to an ordinary pileup.
#[test]
fn pan76_fox_admitted_without_frames_lend_no_slots_to_ordinary_streams() {
    let live = liveset(&["h1", "h2", "h3", "o1", "o2", "o3"]);
    let fox = liveset(&["h1", "h2", "h3"]);
    let out = coalesce_transmit_requests_with_fox_priority(
        vec![
            entry_freq("O1", Some("o1"), 1000.0),
            entry_freq("O2", Some("o2"), 1300.0),
            entry_freq("O3", Some("o3"), 1600.0),
        ],
        1,
        live_in(&live),
        |k: &str| fox.contains(k),
    );
    assert_eq!(texts(&out.retained), vec!["O1"]);
    assert_eq!(out.truncated, 2);
    assert_eq!(out.qso_cap, 1);
}

/// Present Hounds take their slots; ordinary streams get only what the
/// configured cap has left over.
#[test]
fn pan76_ordinary_streams_fill_only_the_room_present_hounds_leave() {
    let live = liveset(&["h1", "o1", "o2", "o3"]);
    let fox = liveset(&["h1"]);
    let out = coalesce_transmit_requests_with_fox_priority(
        vec![
            entry_freq("O1", Some("o1"), 700.0),
            entry_freq("H1", Some("h1"), 1000.0),
            entry_freq("O2", Some("o2"), 1300.0),
            entry_freq("O3", Some("o3"), 1600.0),
        ],
        2,
        live_in(&live),
        |k: &str| fox.contains(k),
    );
    assert_eq!(texts(&out.retained), vec!["H1", "O1"]);
    assert_eq!(out.truncated, 2);
    assert_eq!(texts(&out.truncated_entries), vec!["O2", "O3"]);
    assert_eq!(out.qso_cap, 2);
}

/// With a Hound present, the bundle parity anchor is a Hound, so an
/// ordinary stream on the other parity is the one excluded.
#[test]
fn pan76_fox_admitted_stream_anchors_bundle_parity() {
    use pancetta_core::slot::SlotParity;

    let live = liveset(&["o", "h1", "h2"]);
    let fox = liveset(&["h1", "h2"]);
    let out = coalesce_transmit_requests_with_fox_priority(
        vec![
            entry_with_parity_freq("O", Some("o"), Some(SlotParity::Odd), 700.0),
            entry_with_parity_freq("H1", Some("h1"), Some(SlotParity::Even), 1000.0),
            entry_with_parity_freq("H2", Some("h2"), Some(SlotParity::Even), 1300.0),
        ],
        1,
        live_in(&live),
        |k: &str| fox.contains(k),
    );
    assert_eq!(texts(&out.retained), vec!["H1", "H2"]);
    assert_eq!(out.retained[0].tx_parity, Some(SlotParity::Even));
    assert_eq!(out.parity_excluded, 1);
    assert_eq!(out.truncated, 0);
}

#[test]
fn pan76_manual_sends_still_bypass_the_cap_with_fox_priority() {
    let live = liveset(&["h1"]);
    let fox = liveset(&["h1"]);
    let out = coalesce_transmit_requests_with_fox_priority(
        vec![
            entry_freq("H1", Some("h1"), 1000.0),
            entry_freq("M", None, 1600.0),
        ],
        1,
        live_in(&live),
        |k: &str| fox.contains(k),
    );
    assert_eq!(texts(&out.retained), vec!["H1", "M"]);
    assert_eq!(out.truncated, 0);
}

/// A Fox set that names none of the backlog's QSOs leaves the outcome
/// exactly as the plain coalescer's.
#[test]
fn pan76_unrelated_fox_keys_change_nothing() {
    use pancetta_core::slot::SlotParity;

    let cases: Vec<(Vec<CoalesceEntry>, u32, HashSet<String>)> = vec![
        (
            vec![
                entry_freq("A", Some("qso-a"), 1000.0),
                entry_freq("B", Some("qso-b"), 1300.0),
                entry_freq("C", Some("qso-c"), 1600.0),
            ],
            2,
            liveset(&["qso-a", "qso-b", "qso-c"]),
        ),
        (
            vec![
                entry_freq("QSO", Some("qso-a"), 1000.0),
                entry_freq("MANUAL-1", None, 1300.0),
                entry_freq("MANUAL-2", None, 1600.0),
            ],
            1,
            liveset(&["qso-a"]),
        ),
        (
            vec![
                entry_with_parity("A", Some("qso-a"), Some(SlotParity::Even)),
                entry_with_parity_freq("B", Some("qso-b"), Some(SlotParity::Even), 3000.0),
                entry_with_parity("C", Some("qso-c"), Some(SlotParity::Odd)),
            ],
            UNBOUNDED,
            liveset(&["qso-a", "qso-b", "qso-c"]),
        ),
        (
            vec![
                entry_freq("A", Some("qso-a"), 1000.0),
                entry_freq("B", Some("qso-b"), 1050.0),
                entry_freq("C", Some("qso-c"), 1300.0),
            ],
            2,
            liveset(&["qso-a", "qso-b", "qso-c"]),
        ),
    ];
    for (drained, cap, live) in cases {
        let plain = coalesce_transmit_requests(drained.clone(), cap, live_in(&live));
        let with_fox = coalesce_transmit_requests_with_fox_priority(
            drained,
            cap,
            live_in(&live),
            |k: &str| k == "SOMEONE-ELSE",
        );
        assert_eq!(with_fox, plain);
    }
}

/// End to end through the real backlog drain: an ordinary head request
/// plus three Hound frames under the restored cap of 1. The Hounds are
/// bundled; the ordinary frame is reported as not sent (PAN-75).
#[tokio::test(flavor = "current_thread")]
async fn pan76_coalesce_backlog_into_keeps_fox_admitted_hounds_and_reports_the_ordinary_stream() {
    use crate::message_bus::MessageBus;
    use std::sync::Arc;

    let bus = MessageBus::new(16).unwrap();
    let (_autonomous_tx, autonomous_rx) =
        bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
    let active_tx_qsos = Arc::new(std::sync::RwLock::new(liveset(&[
        "qso-o", "hound-1", "hound-2", "hound-3",
    ])));
    let fox_admitted_qsos = Arc::new(std::sync::RwLock::new(liveset(&[
        "hound-1", "hound-2", "hound-3",
    ])));

    let head = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "O".to_string(),
        frequency_offset: 700.0,
        qso_id: Some("qso-o".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    for (text, id, freq) in [
        ("H1", "hound-1", 1000.0),
        ("H2", "hound-2", 1300.0),
        ("H3", "hound-3", 1600.0),
    ] {
        backlog_tx
            .send(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    attempt_id: None,
                    message_text: text.to_string(),
                    frequency_offset: freq,
                    qso_id: Some(id.to_string()),
                    tx_parity: None,
                    origin: crate::message_bus::TxOrigin::Local,
                    remote_client_key_id: None,
                },
                Instant::now(),
            ))
            .unwrap();
    }

    let arm = Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new()));
    let audit_tmp = audit_tmp();
    let audit_log = pancetta_agent::audit::AuditLog::new(audit_tmp.path());
    let display_feed_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backlog_inbox = super::TxInbox::new(backlog_rx.clone(), Default::default());
    let result = coalesce_backlog_into(
        head,
        &backlog_inbox,
        &bus,
        &active_tx_qsos,
        &fox_admitted_qsos,
        1,
        &arm,
        &audit_log,
        &display_feed_enabled,
        None,
    )
    .await;

    match result {
        MessageType::MultiTransmitRequest { items, .. } => {
            let ids: Vec<Option<&str>> = items.iter().map(|it| it.qso_id.as_deref()).collect();
            assert_eq!(ids, vec![Some("hound-1"), Some("hound-2"), Some("hound-3")]);
        }
        other => panic!("expected a MultiTransmitRequest of the three Hounds, got {other:?}"),
    }

    let mut completes = Vec::new();
    while let Ok(msg) = autonomous_rx.try_recv() {
        completes.push(msg);
    }
    assert_eq!(completes.len(), 1);
    assert!(matches!(
        completes[0].message_type,
        MessageType::TransmitComplete { success: false, .. }
    ));
    let mut notices = Vec::new();
    while let Ok(msg) = qso_rx.try_recv() {
        if let MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
            qso_id,
            ..
        }) = msg.message_type
        {
            notices.push(qso_id);
        }
    }
    assert_eq!(notices, vec!["qso-o".to_string()]);
}
