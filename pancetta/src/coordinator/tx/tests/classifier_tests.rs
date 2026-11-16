use super::*;
use crate::coordinator::active_tx_qso_key;

fn transmit_request(text: &str, qso_id: Option<&str>) -> MessageType {
    MessageType::TransmitRequest {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: 1500.0,
        qso_id: qso_id.map(|s| s.to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

#[test]
fn classify_supersedes_on_different_text_same_qso() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = transmit_request("KA1ABC K5ARH RR73", Some("qso-1"));
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    match outcome {
        super::IncomingDuringTx::Supersede { text, qso_id, .. } => {
            assert_eq!(text, "KA1ABC K5ARH RR73");
            assert_eq!(qso_id.as_deref(), Some("qso-1"));
        }
        other => panic!("expected Supersede, got {other:?}"),
    }
}

#[test]
fn classify_supersedes_on_manual_free_text_no_qso() {
    // qso_id == None is the genuine manual/free-text/tune/test-TX case —
    // this is the actual "operator trigger" this classifier exists for,
    // and always supersedes.
    let pivoted_once = std::collections::HashMap::new();
    let candidate = transmit_request("CQ K5ARH EM12", None);
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(matches!(outcome, super::IncomingDuringTx::Supersede { .. }));
}

/// Codex round 2 (PR #346): an untracked (qso_id == None) candidate that
/// is byte-identical to an in-flight untracked item's text (e.g. an
/// unchanged manual CQ re-arriving) is a no-op, not a fresh override —
/// must Drop, not Supersede (which would needlessly deassert PTT and
/// restart, or past `tx_late_max_ms` abandon, the very frame on the air).
#[test]
fn classify_drops_identical_untracked_duplicate_instead_of_superseding() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = transmit_request("CQ K5ARH EM12", None);
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1500.0,
            qso_id: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Drop),
        "an unchanged untracked re-send must Drop, not Supersede — got {outcome:?}"
    );
}

/// A DIFFERENT untracked candidate (distinct text, still qso_id == None)
/// must still supersede — only the identical-text case is a no-op.
#[test]
fn classify_supersedes_different_untracked_text_even_with_an_untracked_item_in_flight() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = transmit_request("CQ K5ARH EM12", None);
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "TEST TEST TEST".to_string(),
            frequency_offset: 1500.0,
            qso_id: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(matches!(outcome, super::IncomingDuringTx::Supersede { .. }));
}

/// PAN-73 regression: reproduces the 2026-09-05 VP2MAA/JF1RDH/JA5GYU
/// live incident. A DIFFERENT QSO's own routine message arriving in the
/// channel while another QSO is in flight must NOT supersede it — that
/// killed a perfectly good in-flight transmission for content that (in
/// the live incident) wasn't even schedulable this slot, leaving the
/// slot silent until an unrelated third QSO's retry timer
/// opportunistically keyed into the dead air ~5s later. It also must
/// NOT be silently discarded (round-1's own regression, Codex finding
/// 4) — `Requeue`, not `Drop`.
#[test]
fn classify_requeues_different_qsos_message_instead_of_dropping_or_superseding() {
    let pivoted_once = std::collections::HashMap::new();
    // In flight: VP2MAA's QSO. Candidate: JF1RDH's QSO, a distinct
    // qso_id and distinct text — exactly the shape that raced in
    // production.
    let candidate = transmit_request("JF1RDH W5AU EM10", Some("qso-jf1rdh"));
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "VP2MAA W5AU EM10".to_string(),
            frequency_offset: 1450.0,
            qso_id: Some("qso-vp2maa".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Requeue),
        "a different QSO's own message must neither supersede an unrelated in-flight TX \
         nor be discarded — got {outcome:?}"
    );
}

/// Codex finding 2 (PR #346 round 1): the multi-TX arm's in-flight bundle
/// can hold more than one QSO. A fresher message for the SECOND (or
/// later) in-flight item must still be recognized as same-QSO and
/// supersede — not be misread as "a different QSO" just because it
/// isn't the bundle's first item.
#[test]
fn classify_matches_against_any_in_flight_bundle_item_not_just_the_first() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = transmit_request("SECOND W5AU RR73", Some("qso-second"));
    let in_flight = [
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "FIRST W5AU R-10".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-first".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "SECOND W5AU R-08".to_string(),
            frequency_offset: 1400.0,
            qso_id: Some("qso-second".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
    ];
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &in_flight,
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    match outcome {
        super::IncomingDuringTx::Supersede { text, qso_id, .. } => {
            assert_eq!(text, "SECOND W5AU RR73");
            assert_eq!(qso_id.as_deref(), Some("qso-second"));
        }
        other => {
            panic!("expected Supersede for the 2nd in-flight item's fresher message, got {other:?}")
        }
    }
}

#[test]
fn classify_drops_identical_content() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = transmit_request("KA1ABC K5ARH R-15", Some("qso-1"));
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(matches!(outcome, super::IncomingDuringTx::Drop));
}

/// Codex P1, round 21 (PR #362): byte-identical content is NOT a safe
/// duplicate when the CANDIDATE's own authorization differs from the
/// in-flight frame's — e.g. a local operator takes over an A-bound
/// remote QSO and repeats its current text. Dropping this would let a
/// later disarm of A abort the in-flight frame while the differently-
/// authorized replacement (the one actually entitled to transmit) is
/// gone for good, with no normal rearm to recover a completed QSO.
#[test]
fn classify_supersedes_identical_content_when_authorization_differs() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: Some("client-a".to_string()),
        }],
        &pivoted_once,
        // In-flight side is bound to a Remote client — candidate above
        // is Local, so authorization differs despite identical content.
        crate::message_bus::TxOrigin::Remote,
        Some("client-a"),
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Supersede { .. }),
        "identical content under DIFFERENT authorization must supersede, not Drop — \
         got {outcome:?}"
    );
}

/// Codex P1, round 21 (PR #362): same authorization-aware check for a
/// `MultiTransmitRequest` bundle whose overlapping item's content is
/// unchanged but the bundle's own authorization differs from the
/// in-flight frame's.
#[test]
fn classify_supersedes_multi_transmit_request_with_unchanged_overlap_when_authorization_differs() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = MessageType::MultiTransmitRequest {
        items: vec![crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "IN-FLIGHT W5AU R-01".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-in-flight".to_string()),
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: Some("client-b".to_string()),
        }],
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Remote,
        remote_client_key_id: Some("client-b".to_string()),
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "IN-FLIGHT W5AU R-01".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-in-flight".to_string()),
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: Some("client-a".to_string()),
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Remote,
        Some("client-a"),
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Supersede { .. }),
        "an unchanged overlap under DIFFERENT authorization must supersede, not Requeue \
         — got {outcome:?}"
    );
}

fn bound_item(
    text: &str,
    qso: &str,
    origin: crate::message_bus::TxOrigin,
    client: Option<&str>,
) -> crate::message_bus::TransmitRequestItem {
    crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: 1000.0,
        qso_id: Some(qso.to_string()),
        origin,
        remote_client_key_id: client.map(str::to_string),
    }
}

/// PAN-136 (Codex P2 on PR #456): same folded bundle binding, same text,
/// but one overlapping item's own binding changed (local takeover of one
/// of two Remote-A QSOs). That is a changed overlap, not a repeat.
#[test]
fn classify_supersedes_bundle_when_one_items_binding_changed_under_the_same_fold() {
    use crate::message_bus::TxOrigin::{Local, Remote};
    let pivoted_once = std::collections::HashMap::new();
    let candidate = MessageType::MultiTransmitRequest {
        items: vec![
            bound_item("A W5AU R-01", "qso-1", Remote, Some("client-a")),
            bound_item("B W5AU R-02", "qso-2", Local, None),
        ],
        tx_parity: None,
        origin: Remote,
        remote_client_key_id: Some("client-a".to_string()),
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[
            bound_item("A W5AU R-01", "qso-1", Remote, Some("client-a")),
            bound_item("B W5AU R-02", "qso-2", Remote, Some("client-a")),
        ],
        &pivoted_once,
        Remote,
        Some("client-a"),
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Supersede { .. }),
        "a per-item binding change must supersede, got {outcome:?}"
    );
}

/// PAN-136 (Codex P2 on PR #456): the single-request arm compares the
/// matching in-flight item's own binding too. In a Remote-A bundle that
/// carries a Local item, a client-A request for that QSO with identical
/// content re-binds it; it is not an exact duplicate.
#[test]
fn classify_supersedes_request_rebinding_an_in_flight_item_under_the_same_fold() {
    use crate::message_bus::TxOrigin::{Local, Remote};
    let pivoted_once = std::collections::HashMap::new();
    let candidate = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "B W5AU R-02".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-2".to_string()),
        tx_parity: None,
        origin: Remote,
        remote_client_key_id: Some("client-a".to_string()),
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[
            bound_item("A W5AU R-01", "qso-1", Remote, Some("client-a")),
            bound_item("B W5AU R-02", "qso-2", Local, None),
        ],
        &pivoted_once,
        Remote,
        Some("client-a"),
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Supersede { .. }),
        "a re-bound same-content request must supersede, got {outcome:?}"
    );
}

/// Control: identical content AND identical per-item bindings under the
/// same fold is still the unchanged repeat PAN-73 round 6 filters out.
#[test]
fn classify_requeues_bundle_with_unchanged_items_and_bindings() {
    use crate::message_bus::TxOrigin::Remote;
    let pivoted_once = std::collections::HashMap::new();
    let items = vec![
        bound_item("A W5AU R-01", "qso-1", Remote, Some("client-a")),
        bound_item("B W5AU R-02", "qso-2", Remote, Some("client-a")),
    ];
    let candidate = MessageType::MultiTransmitRequest {
        items: items.clone(),
        tx_parity: None,
        origin: Remote,
        remote_client_key_id: Some("client-a".to_string()),
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &items,
        &pivoted_once,
        Remote,
        Some("client-a"),
    );
    assert_eq!(outcome, super::IncomingDuringTx::Requeue);
}

/// Codex round 3 (PR #346): a stuck-DX QSO can re-render the SAME text
/// at a deliberately hopped frequency to dodge a collision
/// (`qso_manager.rs`'s stuck-DX offset hop). Matching a same-QSO
/// duplicate on text alone would wrongly Drop this and leave the
/// old-offset signal transmitting into the collision it was trying to
/// avoid — frequency must be part of the identity check.
#[test]
fn classify_supersedes_same_text_different_frequency_same_qso() {
    let pivoted_once = std::collections::HashMap::new();
    let mut candidate = transmit_request("KA1ABC K5ARH R-15", Some("qso-1"));
    if let MessageType::TransmitRequest {
        frequency_offset, ..
    } = &mut candidate
    {
        *frequency_offset = 1800.0; // hopped off 1500.0
    }
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Supersede { .. }),
        "identical text at a hopped frequency must supersede, not Drop — got {outcome:?}"
    );
}

#[test]
fn classify_drops_pivot_tombstone_duplicate() {
    let mut pivoted_once = std::collections::HashMap::new();
    pivoted_once.insert(
        active_tx_qso_key("qso-1"),
        ("KA1ABC K5ARH RR73".to_string(), 1500.0),
    );
    let candidate = transmit_request("KA1ABC K5ARH RR73", Some("qso-1"));
    // in_flight_text is something else — the pivot already sent RR73 via
    // Step 4c, this is the stale second copy of the request that produced it.
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(matches!(outcome, super::IncomingDuringTx::Drop));
}

/// Codex P1, round 22 (PR #362): a pivot tombstone match (same qso_id,
/// text, frequency) is not a safe duplicate when the CANDIDATE's own
/// authorization differs from the in-flight frame's — round 21's
/// authorization check ran only AFTER this tombstone check, so control
/// transferring from client A to client B while B resends the exact
/// pivoted text still got swallowed as A's stale tombstone duplicate.
#[test]
fn classify_does_not_drop_pivot_tombstone_when_authorization_differs() {
    let mut pivoted_once = std::collections::HashMap::new();
    pivoted_once.insert(
        active_tx_qso_key("qso-1"),
        ("KA1ABC K5ARH RR73".to_string(), 1500.0),
    );
    let candidate = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "KA1ABC K5ARH RR73".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Remote,
        remote_client_key_id: Some("client-b".to_string()),
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH RR73".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Remote,
            remote_client_key_id: Some("client-a".to_string()),
        }],
        &pivoted_once,
        // In-flight side is bound to client-a — candidate above is
        // client-b, so authorization differs despite an exact tombstone
        // match.
        crate::message_bus::TxOrigin::Remote,
        Some("client-a"),
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Supersede { .. }),
        "a tombstone match under DIFFERENT authorization must supersede, not Drop — \
         got {outcome:?}"
    );
}

/// Codex round 6 (PR #346): a pivot tombstone for the same QSO and text
/// but a DIFFERENT frequency (a stuck-DX collision-avoidance hop that
/// happens to reuse the pivoted text) must NOT be treated as the stale
/// tombstone duplicate — `is_pivot_duplicate` runs BEFORE this
/// function's own frequency-aware same-QSO check, so without frequency
/// in the tombstone's identity this hop would be dropped here and never
/// reach that check at all, leaving the transmission on the collision
/// frequency the hop was trying to avoid.
#[test]
fn classify_does_not_drop_pivot_tombstone_when_frequency_differs() {
    let mut pivoted_once = std::collections::HashMap::new();
    pivoted_once.insert(
        active_tx_qso_key("qso-1"),
        ("KA1ABC K5ARH RR73".to_string(), 1500.0),
    );
    let mut candidate = transmit_request("KA1ABC K5ARH RR73", Some("qso-1"));
    if let MessageType::TransmitRequest {
        frequency_offset, ..
    } = &mut candidate
    {
        *frequency_offset = 1800.0; // hopped off the tombstoned 1500.0
    }
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH RR73".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Supersede { .. }),
        "a hopped frequency must not be swallowed by the pivot tombstone check — got {outcome:?}"
    );
}

/// Codex finding 3 (PR #346 round 1): a `MultiTransmitRequest` bundle
/// candidate must only supersede when it actually shares a QSO with the
/// in-flight set. `autonomous.rs` can enqueue a bundle covering entirely
/// OTHER QSOs while a transmission is in flight (the same PAN-73 race,
/// just bundle-shaped) — that must Requeue, same as a single-item
/// different-QSO candidate, not unconditionally supersede.
#[test]
fn classify_requeues_multi_transmit_request_with_no_overlap() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = MessageType::MultiTransmitRequest {
        items: vec![crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "OTHER1 W5AU R-05".to_string(),
            frequency_offset: 900.0,
            qso_id: Some("qso-other1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "IN-FLIGHT W5AU R-01".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-in-flight".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Requeue),
        "a bundle covering entirely other QSOs must not supersede, got {outcome:?}"
    );
}

/// A `MultiTransmitRequest` bundle that DOES share a QSO with the
/// in-flight set is legitimate new information for that QSO and must
/// still supersede (unchanged from before Codex round 1 — this is not
/// the PAN-73 race shape).
#[test]
fn classify_supersedes_multi_transmit_request_with_overlap() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = MessageType::MultiTransmitRequest {
        items: vec![
            crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: "OTHER1 W5AU R-05".to_string(),
                frequency_offset: 900.0,
                qso_id: Some("qso-other1".to_string()),
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
            crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: "IN-FLIGHT W5AU RR73".to_string(),
                frequency_offset: 1000.0,
                qso_id: Some("qso-in-flight".to_string()),
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
        ],
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "IN-FLIGHT W5AU R-01".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-in-flight".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Supersede { .. }),
        "a bundle sharing an in-flight QSO must still supersede, got {outcome:?}"
    );
}

/// Codex round 6 (PR #346): a bundle whose overlapping item is
/// BYTE-IDENTICAL (text and frequency) to the in-flight item it matches
/// — alongside unrelated items for other QSOs — must NOT supersede.
/// `supersede_and_rekey_or_bundle` cannot merge an incoming
/// `MultiTransmitRequest` at all, so superseding here would deassert
/// PTT and abandon the good transmission purely because of a
/// coincidental ID match, with nothing fresher to show for it — the
/// exact PAN-73 dead-air failure, reached via a bundle instead of a
/// single `TransmitRequest`.
#[test]
fn classify_requeues_multi_transmit_request_with_unchanged_overlap() {
    let pivoted_once = std::collections::HashMap::new();
    let candidate = MessageType::MultiTransmitRequest {
        items: vec![
            crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: "OTHER1 W5AU R-05".to_string(),
                frequency_offset: 900.0,
                qso_id: Some("qso-other1".to_string()),
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
            // Byte-identical to the in-flight item below — no fresher
            // content for the currently-keyed QSO.
            crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: "IN-FLIGHT W5AU R-01".to_string(),
                frequency_offset: 1000.0,
                qso_id: Some("qso-in-flight".to_string()),
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
        ],
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let outcome = super::classify_incoming_during_tx(
        &candidate,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "IN-FLIGHT W5AU R-01".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-in-flight".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        crate::message_bus::TxOrigin::Local,
        None,
    );
    assert!(
        matches!(outcome, super::IncomingDuringTx::Requeue),
        "an ID-only match with no changed content must not supersede, got {outcome:?}"
    );
}
