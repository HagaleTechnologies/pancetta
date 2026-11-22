//! Coordinator-level FT4 TX coverage (final-review blocker).
//!
//! Before this wiring the TX worker hardcoded FT8 encode+modulate, so in
//! `[rig].mode = "FT4"` the station DECODED FT4 but TRANSMITTED an FT8
//! waveform (8-GFSK, 79 symbols, 12.64s) onto the 7.5s grid — FT4 QSOs
//! could never complete. These tests pin the branch: FT4 emits the FT4
//! waveform (4-GFSK, 105 symbols, ~5.04s / 60_480 samples @ 12 kHz), and
//! the FT8 branch stays byte-identical to the legacy encode+modulate path.
use super::*;
use pancetta_ft8::{Ft8Encoder, Ft8Modulator, Protocol, ProtocolParams};

/// The message used across the protocol-branch tests. A standard grid
/// message so both FT8 and FT4 encode via `try_encode_standard`.
const MSG: &str = "CQ K5ARH EM12";

#[test]
fn ft4_encode_produces_105_symbols_not_79() {
    let mut enc = Ft8Encoder::with_protocol(ProtocolParams::ft4());
    let symbols =
        encode_for_protocol(&mut enc, Protocol::Ft4, MSG).expect("FT4 encode should succeed");
    assert_eq!(
        symbols.len(),
        105,
        "FT4 must emit 105 symbols (4-GFSK), not FT8's 79"
    );
    // FT4 is 4-GFSK: every symbol must be in 0..=3.
    assert!(
        symbols.iter().all(|&s| s < 4),
        "FT4 symbols must be 4-ary (0-3): {symbols:?}"
    );
}

#[test]
fn ft4_waveform_is_ft4_length_not_ft8() {
    // End-to-end encode+modulate for FT4 through the extracted helper.
    let samples =
        encode_and_modulate(Protocol::Ft4, MSG, 0.0).expect("FT4 encode+modulate should succeed");
    // FT4: 105 symbols × 0.048s × 12_000 Hz = 60_480 samples (~5.04s).
    assert_eq!(
        samples.len(),
        60_480,
        "FT4 waveform must be ~5.04s (60_480 samples @ 12 kHz), the 7.5s-grid FT4 length"
    );
    // Must NOT be the FT8 waveform length (79 × 0.16 × 12_000 = 151_680).
    assert_ne!(
        samples.len(),
        151_680,
        "FT4 must not emit an FT8-length (12.64s) waveform"
    );
}

/// TX-F4: the Step-4c late-pivot re-encode (`coordinator/tx.rs`, "Step 4c:
/// late pivot to the freshest message") must use the SAME protocol-aware
/// `encode_for_protocol` + `modulate_for_protocol` pair Step 1's initial
/// encode uses, not the legacy FT8-only `encode_message`/`modulate_symbols`
/// pair. Before this fix, a pivot in FT4/FT2 mode would emit an FT8-shaped
/// (151_680-sample) waveform onto the FT4/FT2 grid — wrong length, wrong
/// symbol timing. This test exercises the exact call shape the pivot now
/// uses and proves it yields the FT4-length waveform (60_480 samples).
#[test]
fn ft4_pivot_reencode_produces_ft4_shaped_waveform_not_ft8() {
    let mut enc = Ft8Encoder::with_protocol(ProtocolParams::ft4());
    let mut modu = Ft8Modulator::new_default().unwrap();
    // Mirrors the pivot's `modulator.set_base_frequency(new_freq)` call.
    modu.set_base_frequency(1500.0).unwrap();

    let samples = encode_for_protocol(&mut enc, Protocol::Ft4, MSG)
        .and_then(|s| modulate_for_protocol(&mut modu, Protocol::Ft4, &s, 0.0))
        .expect("FT4 pivot re-encode should succeed");

    assert_eq!(
        samples.len(),
        60_480,
        "a Step-4c pivot re-encode in FT4 mode must produce the FT4-shaped \
         waveform (60_480 samples @ 12 kHz)"
    );
    assert_ne!(
        samples.len(),
        151_680,
        "a Step-4c pivot re-encode in FT4 mode must NOT fall back to the \
         FT8-shaped (151_680-sample) waveform"
    );
}

#[test]
fn ft8_waveform_is_ft8_length() {
    let samples =
        encode_and_modulate(Protocol::Ft8, MSG, 0.0).expect("FT8 encode+modulate should succeed");
    // FT8: 79 symbols × 0.16s × 12_000 Hz = 151_680 samples (~12.64s).
    assert_eq!(
        samples.len(),
        151_680,
        "FT8 waveform must be the ~12.64s length"
    );
}

#[test]
fn ft8_branch_is_byte_identical_to_legacy_path() {
    // Legacy path exactly as the TX worker called it pre-FT4-wiring:
    // Ft8Encoder::new() + Ft8Modulator::new_default() + encode_message +
    // modulate_symbols.
    let legacy = {
        let mut enc = Ft8Encoder::new();
        let mut modu = Ft8Modulator::new_default().unwrap();
        let symbols = enc.encode_message(MSG, None).unwrap();
        modu.modulate_symbols(&symbols, 0.0).unwrap()
    };
    // New helper path with Protocol::Ft8.
    let via_helper =
        encode_and_modulate(Protocol::Ft8, MSG, 0.0).expect("FT8 helper should succeed");
    assert_eq!(
        legacy.len(),
        via_helper.len(),
        "FT8 helper sample count must match the legacy path"
    );
    assert_eq!(
        legacy, via_helper,
        "FT8 helper output must be byte-identical to the legacy encode+modulate path"
    );
}

#[test]
fn ft8_split_helpers_match_legacy_combined_call() {
    // Prove the split encode_for_protocol + modulate_for_protocol pair (what
    // the worker's single-TX path now calls) is byte-identical to the legacy
    // combined encode_message().and_then(modulate_symbols) call for FT8.
    let legacy = {
        let mut enc = Ft8Encoder::new();
        let mut modu = Ft8Modulator::new_default().unwrap();
        enc.encode_message(MSG, None)
            .and_then(|symbols| modu.modulate_symbols(&symbols, 0.0))
            .unwrap()
    };
    let split = {
        let mut enc = Ft8Encoder::new();
        let mut modu = Ft8Modulator::new_default().unwrap();
        let symbols = encode_for_protocol(&mut enc, Protocol::Ft8, MSG).unwrap();
        modulate_for_protocol(&mut modu, Protocol::Ft8, &symbols, 0.0).unwrap()
    };
    assert_eq!(
        legacy, split,
        "split FT8 encode/modulate helpers must equal the legacy combined call"
    );
}

fn tx_item(text: &str, freq: f64, qso_id: Option<&str>) -> crate::message_bus::TransmitRequestItem {
    crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: freq,
        qso_id: qso_id.map(|s| s.to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

/// TX-F8: `multi_tx_status_items` is the single source both the initial
/// "QUEUED" status and the Step-2b defer-time refresh use to build the
/// TUI-visible TX-strip items — prove it tags every item with the
/// requested `deferred` flag (not the previously-unconditional `false`).
#[test]
fn multi_tx_status_items_tags_deferred_flag() {
    let items = vec![
        tx_item("CQ K5ARH EM12", 800.0, None),
        tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-1")),
    ];

    let queued = multi_tx_status_items(&items, false);
    assert_eq!(queued.len(), 2);
    assert!(
        queued.iter().all(|it| !it.deferred),
        "the initial QUEUED status must show deferred: false"
    );

    let deferred = multi_tx_status_items(&items, true);
    assert_eq!(deferred.len(), 2);
    assert!(
        deferred.iter().all(|it| it.deferred),
        "the Step-2b defer-time refresh must show deferred: true on every \
         item in the bundle (a bundle defers as a whole)"
    );
    // Content is otherwise preserved (text/freq/qso_id) regardless of
    // the deferred flag.
    assert_eq!(deferred[0].text, "CQ K5ARH EM12");
    assert_eq!(deferred[0].freq_hz, 800.0);
    assert_eq!(deferred[0].qso_id, None);
    assert_eq!(deferred[1].qso_id, Some("qso-1".to_string()));
}

#[test]
fn multi_tx_encode_all_items_succeed_in_order() {
    let mut enc = Ft8Encoder::new();
    let params = ProtocolParams::from_protocol(Protocol::Ft8);
    let items = vec![
        tx_item("CQ K5ARH EM12", 800.0, None),
        tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-1")),
    ];
    let outcome = encode_and_modulate_multi_tx(&mut enc, Protocol::Ft8, &params, &items);
    assert!(outcome.encode_failed.is_empty());
    assert_eq!(outcome.item_texts, vec!["CQ K5ARH EM12", "W1AW K5ARH EM12"]);
    assert_eq!(
        outcome.encoded_qso_ids,
        vec![None, Some("qso-1".to_string())]
    );
    let samples = outcome.samples.expect("both items should modulate fine");
    assert_eq!(
        samples.len(),
        151_680,
        "FT8 multi-TX waveform is one fixed-duration 12.64s slot regardless of item count"
    );
}

/// 2026-07-17 regression lock: the original inline Step 1 code built each
/// `MultiTxItem`'s frequency via `items[i].frequency_offset` AFTER the
/// encode loop had already skipped a failed item from `symbol_sets` — so
/// a partial encode failure silently misaligned every LATER item's
/// frequency with an unrelated, earlier item's. This proves the
/// extracted helper keeps frequency/text/qso_id/symbols correctly
/// paired per-item even when a middle item fails to encode.
#[test]
fn multi_tx_encode_partial_failure_does_not_misalign_frequencies() {
    let mut enc = Ft8Encoder::new();
    let params = ProtocolParams::from_protocol(Protocol::Ft8);
    // A free-text message far too long for FT8's charset/length limit —
    // guaranteed to fail `encode_for_protocol`, unlike the two valid
    // standard-callsign messages bracketing it.
    let bad_text = "THIS FREE TEXT MESSAGE IS WAY TOO LONG FOR FT8 TO EVER ENCODE";
    let items = vec![
        tx_item("CQ K5ARH EM12", 800.0, Some("qso-good-1")),
        tx_item(bad_text, 1000.0, Some("qso-bad")),
        tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-good-2")),
    ];
    let outcome = encode_and_modulate_multi_tx(&mut enc, Protocol::Ft8, &params, &items);

    assert_eq!(
        outcome.encode_failed.len(),
        1,
        "exactly the bad middle item should fail to encode"
    );
    assert_eq!(outcome.encode_failed[0].message_text, bad_text);
    assert_eq!(outcome.encode_failed[0].qso_id.as_deref(), Some("qso-bad"));

    // The two GOOD items must keep their OWN frequencies and qso_ids —
    // not silently inherit each other's via index drift.
    assert_eq!(outcome.item_texts, vec!["CQ K5ARH EM12", "W1AW K5ARH EM12"]);
    assert_eq!(
        outcome.encoded_qso_ids,
        vec![
            Some("qso-good-1".to_string()),
            Some("qso-good-2".to_string())
        ]
    );
    assert!(
        outcome.samples.is_ok(),
        "the two good items should still modulate"
    );
    // `encoded_items` (2026-07-17, post-review) must ALSO stay
    // positionally aligned with item_texts/encoded_qso_ids, skipping
    // exactly the failed item — this is what the caller rebinds
    // `items` to, so downstream code (Step 4b's liveness zip) can
    // never see a stale, unfiltered `items` list again.
    assert_eq!(outcome.encoded_items.len(), 2);
    assert_eq!(
        outcome.encoded_items[0].qso_id.as_deref(),
        Some("qso-good-1")
    );
    assert_eq!(
        outcome.encoded_items[1].qso_id.as_deref(),
        Some("qso-good-2")
    );
}

/// 2026-07-17 (post-review) critical regression lock: an independent
/// review of an earlier version of this fix found that the CALLER was
/// zipping the unfiltered input `items` list against `encoded_qso_ids`
/// (shorter, whenever any item failed to encode) — silently misaligning
/// every item after the first encode failure. In the worst case this
/// could pair a genuinely-cancelled QSO with a "still live" verdict
/// belonging to an unrelated item, transmitting a call the operator had
/// just killed. This test proves the FIX PATTERN Step 1/Step 4b now
/// use — rebind `items = outcome.encoded_items` immediately, THEN zip
/// against a liveness set derived from `encoded_qso_ids` — keeps every
/// item paired with its OWN, correct verdict, even with an encode
/// failure in the middle of the bundle.
#[test]
fn multi_tx_items_rebind_keeps_correct_pairing_after_encode_failure() {
    use std::collections::HashSet;

    let mut enc = Ft8Encoder::new();
    let params = ProtocolParams::from_protocol(Protocol::Ft8);
    let bad_text = "THIS FREE TEXT MESSAGE IS WAY TOO LONG FOR FT8 TO EVER ENCODE";
    let original_items = vec![
        tx_item("CQ K5ARH EM12", 800.0, Some("qso-kenya")),
        tx_item(bad_text, 1000.0, Some("qso-bad-encode")),
        tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-peru")),
    ];
    let outcome = encode_and_modulate_multi_tx(&mut enc, Protocol::Ft8, &params, &original_items);
    assert_eq!(outcome.encode_failed.len(), 1);

    // The fix: rebind `items` to the encoded subset BEFORE any liveness
    // check, exactly as the real Step 1 call site now does.
    let items = outcome.encoded_items;
    let encoded_qso_ids = outcome.encoded_qso_ids;
    assert_eq!(items.len(), encoded_qso_ids.len());

    // Simulate: Kenya was cancelled during the pre-PTT wait; Peru is
    // still live. Only Peru is in the "active" set.
    let mut active: HashSet<String> = HashSet::new();
    active.insert(super::super::active_tx_qso_key("qso-peru"));

    let live_mask: Vec<bool> = encoded_qso_ids
        .iter()
        .map(|id| super::super::tx_qso_is_live(id.as_deref(), &active))
        .collect();

    // Zipping the REBOUND `items` (not the original 3-item list) against
    // `live_mask` must pair each item with ITS OWN verdict.
    let live_items: Vec<_> = items
        .iter()
        .zip(live_mask.iter())
        .filter(|(_, &live)| live)
        .map(|(item, _)| item.qso_id.clone())
        .collect();

    assert_eq!(
        live_items,
        vec![Some("qso-peru".to_string())],
        "only Peru must survive — Kenya must be excluded, not swapped in via index drift"
    );

    let dropped: Vec<_> = items
        .iter()
        .zip(live_mask.iter())
        .filter(|(_, &live)| !live)
        .map(|(item, _)| item.qso_id.clone())
        .collect();
    assert_eq!(
        dropped,
        vec![Some("qso-kenya".to_string())],
        "Kenya must be the one reported as dropped, not silently kept alive"
    );
}

#[test]
fn multi_tx_encode_empty_input_yields_no_samples() {
    let mut enc = Ft8Encoder::new();
    let params = ProtocolParams::from_protocol(Protocol::Ft8);
    let outcome = encode_and_modulate_multi_tx(&mut enc, Protocol::Ft8, &params, &[]);
    assert!(outcome.samples.is_err());
    assert!(outcome.item_texts.is_empty());
    assert!(outcome.encode_failed.is_empty());
}

/// 2026-07-17: proves the key-time re-encode path (Step 4b, when only
/// SOME bundle items are still live) reuses the encode helper correctly
/// on just a subset, producing a shorter but still-valid, correctly
/// frequency-aligned output — the core building block the Step 4b
/// rebuild relies on instead of dropping a still-live station's cycle.
#[test]
fn multi_tx_encode_subset_reencode_matches_full_encode_of_that_subset() {
    let params = ProtocolParams::from_protocol(Protocol::Ft8);
    let all_items = vec![
        tx_item("CQ K5ARH EM12", 800.0, Some("qso-kenya")),
        tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-peru")),
    ];
    // "Full" bundle result.
    let mut enc_full = Ft8Encoder::new();
    let full = encode_and_modulate_multi_tx(&mut enc_full, Protocol::Ft8, &params, &all_items);
    assert!(full.samples.is_ok());

    // "Rebuilt" result for just the still-live subset (as Step 4b does
    // when qso-kenya went stale during the pre-PTT wait).
    let live_subset: Vec<_> = all_items
        .iter()
        .filter(|it| it.qso_id.as_deref() == Some("qso-peru"))
        .cloned()
        .collect();
    let mut enc_rebuild = Ft8Encoder::new();
    let rebuilt =
        encode_and_modulate_multi_tx(&mut enc_rebuild, Protocol::Ft8, &params, &live_subset);

    assert_eq!(rebuilt.item_texts, vec!["W1AW K5ARH EM12"]);
    assert_eq!(rebuilt.encoded_qso_ids, vec![Some("qso-peru".to_string())]);
    assert!(
        rebuilt.samples.is_ok(),
        "the surviving item alone must still modulate"
    );
    // Same fixed protocol duration regardless of how many items are summed.
    assert_eq!(
        rebuilt.samples.unwrap().len(),
        full.samples.unwrap().len(),
        "re-encoding a subset must not change the waveform's fixed slot duration \
         (the Step 4b rebuild reuses the ALREADY-COMPUTED TxSchedule padding/cursor, \
         which assumes this)"
    );
}
