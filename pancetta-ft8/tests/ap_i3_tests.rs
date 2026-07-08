//! Task W1.1: AP4 i3-bit injection correctness.
//!
//! Spec ref: `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`
//! §4 — AP4 (a-priori decoding level 4, used when we're mid-QSO expecting
//! the partner's RR73/RRR/73 confirmation) previously injected payload
//! bits 74-76 (the `i3` message-type field) as `(0,0,0)`. RR73/RRR/73 are
//! **i3=1** ("standard message") frames; `i3=0` selects the
//! FreeText/Telemetry/contest message-type family, which
//! `Ft8Message::is_plausible()` unconditionally rejects. So AP4's own
//! injected prior was self-contradicting the very message class it
//! exists to help decode — AP4 could never survive to a successful
//! decode.
//!
//! This file establishes the i3 bit order **empirically, from this
//! project's own encoder** (not by assumption) and fixes the injected
//! value in `ap.rs`.
//!
//! Investigating this task's required end-to-end rescue test (Step 3 of
//! the task brief) surfaced a SEPARATE, pre-existing, out-of-scope bug:
//! `inject_ap_llrs`'s AP3/AP4 arms inject the DX's callsign at payload
//! offset 0 (the `to_callsign` field) and our own callsign at offset 28
//! (near, but not exactly, the `from_callsign` field) — backwards from
//! real incoming-message semantics (we are always `to_callsign` for any
//! message we decode; we never decode our own TX) and additionally
//! off-by-one at the offset-28 boundary. `decoder.rs::
//! ap_injection_survived`'s AP3/AP4 arm independently (and correctly)
//! requires the opposite assignment (`to_callsign == my_call`), so it
//! unconditionally rejects every AP3/AP4 decode in production today —
//! AP3 and AP4 are both structurally dead code, not just AP4 as this
//! task's narrower framing assumed. Confirmed both by code-reading
//! (`ap_injection_survived`'s literal requirement) and empirically
//! (constructing a payload with the injected bit patterns in place and
//! parsing it directly). Full writeup:
//! `research/experiments/2026-07-07-w1.1-ap4-i3-fix.md`. Out of scope
//! for W1.1 (touches AP1/AP2 too, used on every decode candidate, not
//! just QSO-state AP3/AP4) — flagged as a follow-up rather than fixed
//! here. Because of this, a full audio-domain "AP0 fails, AP4 rescues"
//! demonstration is not achievable honestly today; this file instead
//! verifies the i3 fix at the LLR-injection level, which is what the fix
//! itself actually changes.
//!
//! **Fixed in Task W1.7** (`research/experiments/2026-07-07-w1.7-ap-injection-ordering-fix.md`):
//! AP1-AP4's callsign fields (and `enumerate_a8_expected_texts`'s dx/my
//! template ordering, which shared the same swap) now match the correct
//! convention `ap_injection_survived` always required. The end-to-end
//! "AP0 fails, AP3 rescues" audio-domain test flagged above as not
//! achievable is now in `pancetta-ft8/tests/ap_injection_ordering_tests.rs`.
//!
//! W1.2 extends this same file with further i3/message-family AP tests.

#![cfg(feature = "transmit")]
#![allow(clippy::needless_range_loop)] // protocol-bit-position loops; see ap.rs/encoder.rs

use bitvec::prelude::*;
use pancetta_ft8::ap::{
    inject_ap2_caller, inject_ap_llrs, inject_confirmation_token_bits,
    inject_recent_call_at_called, ApContext, ApLevel, ConfirmationToken, MyCallAp, QsoAp,
    QsoApProgress, RecentCallAp,
};
use pancetta_ft8::ldpc::{gray_to_binary, gray_to_binary_4fsk};
use pancetta_ft8::message::PAYLOAD_BITS;
use pancetta_ft8::protocol::{ProtocolParams, FT4_XOR_SEQUENCE};
use pancetta_ft8::{Ft8Encoder, NUM_SYMBOLS};

// ============================================================================
// Encoder-side ground truth helpers
// ============================================================================

/// Extract the 77-bit payload from encoded FT8 symbols.
///
/// Mirrors `wsjtx_compat_tests.rs::extract_payload_from_symbols` — reverses
/// symbols → Gray decode → codeword bits → info bits[0..77] = payload.
/// Duplicated locally (rather than shared) because these are separate
/// integration-test binaries; kept intentionally small.
fn extract_payload_bits(symbols: &[u8; NUM_SYMBOLS]) -> BitVec {
    const LDPC_CODEWORD_BITS: usize = 174;

    let mut codeword_bits = Vec::with_capacity(LDPC_CODEWORD_BITS);
    for i_tone in 0..NUM_SYMBOLS {
        let is_data = (7..36).contains(&i_tone) || (43..72).contains(&i_tone);
        if !is_data {
            continue;
        }
        let binary_value = gray_to_binary(symbols[i_tone]);
        codeword_bits.push((binary_value & 4) != 0);
        codeword_bits.push((binary_value & 2) != 0);
        codeword_bits.push((binary_value & 1) != 0);
    }
    assert_eq!(codeword_bits.len(), LDPC_CODEWORD_BITS);

    let mut payload = BitVec::with_capacity(PAYLOAD_BITS);
    for &b in &codeword_bits[0..PAYLOAD_BITS] {
        payload.push(b);
    }
    payload
}

/// Encode `message_text` with this project's own encoder and return its
/// real 77-bit payload (ground truth, not assumption).
fn encode_to_payload(message_text: &str) -> BitVec {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message(message_text, None)
        .unwrap_or_else(|e| panic!("failed to encode {message_text:?}: {e}"));
    extract_payload_bits(&symbols)
}

/// Bits 74..77 of a 77-bit payload as a `[bool; 3]`, MSB first (matches
/// `message.rs::parse_payload`'s `bits_to_u32(&payload[74..77])` and the
/// encoder's `for i in (0..3).rev() { payload.push((i3 >> i) & 1 != 0) }`
/// packing — both MSB-first over the 3-bit `i3` field).
fn i3_bits(payload: &BitSlice) -> [bool; 3] {
    [payload[74], payload[75], payload[76]]
}

// ============================================================================
// Step 1/2: encoder-verified i3 bit order + AP4 injection fix
// ============================================================================

/// Ground-truth check, independent of AP4: a real RR73 message encoded by
/// this project's own encoder uses i3=1, i.e. payload bits 74..77 =
/// (false, false, true) — NOT (false, false, false) (i3=0).
///
/// This nails down the bit order empirically before touching `ap.rs`:
/// i3 is packed MSB-first, so for i3=1 (0b001) bit74=0 (MSB), bit75=0,
/// bit76=1 (LSB).
#[test]
fn encoder_rr73_message_uses_i3_one() {
    for msg in ["W1ABC K1DEF RR73", "W1ABC K1DEF RRR", "W1ABC K1DEF 73"] {
        let payload = encode_to_payload(msg);
        assert_eq!(
            i3_bits(&payload),
            [false, false, true],
            "message {msg:?} must encode i3=1 (bits 74..77 = 0,0,1); this is the \
             standard/RR73/RRR/73 message-type family, distinct from the i3=0 \
             FreeText/Telemetry/contest family that Ft8Message::is_plausible() \
             unconditionally rejects"
        );
    }
}

/// THE key regression test for this task: AP4's injected i3 bits must
/// match the encoder's real i3=1 encoding for RR73/RRR/73 messages.
///
/// Before the fix: AP4 injected (false, false, false) (i3=0), which
/// contradicts the encoder's ground truth of (false, false, true)
/// (i3=1) established above — this is why AP4 could never survive
/// `Ft8Message::is_plausible()` (i3=0 selects the FreeText/Telemetry/
/// contest family, all unconditionally rejected).
#[test]
fn ap4_injected_i3_bits_match_encoder_rr73_ground_truth() {
    let true_i3 = i3_bits(&encode_to_payload("W1ABC K1DEF RR73"));
    assert_eq!(
        true_i3,
        [false, false, true],
        "sanity: RR73 ground truth must be i3=1"
    );

    let my_call = MyCallAp::new("K1DEF").expect("K1DEF should encode");
    let qso =
        QsoAp::new("W1ABC", QsoApProgress::WaitingForConfirmation).expect("W1ABC should encode");
    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: Some(qso),
    };

    let mut llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut llrs, ApLevel::Ap4, &ctx, None);

    // inject_bit: true bit -> negative LLR, false bit -> positive LLR.
    let injected_i3 = [llrs[74] < 0.0, llrs[75] < 0.0, llrs[76] < 0.0];

    assert_eq!(
        injected_i3, true_i3,
        "AP4 must inject the i3=1 bit pattern (RR73/RRR/73's real message \
         type) at payload bits 74..77, not i3=0 (FreeText/Telemetry/contest \
         family, unconditionally rejected by is_plausible())"
    );
}

/// AP4 must leave bits 0-73 exactly as AP3 would (same callsign
/// injection), only adding the i3 constraint at 74-76. Regression guard
/// so the i3 fix doesn't accidentally touch anything else.
#[test]
fn ap4_only_changes_i3_bits_relative_to_ap3() {
    let my_call = MyCallAp::new("K1DEF").unwrap();
    let qso = QsoAp::new("W1ABC", QsoApProgress::WaitingForConfirmation).unwrap();
    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: Some(qso),
    };

    let mut ap3_llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut ap3_llrs, ApLevel::Ap3, &ctx, None);

    let mut ap4_llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut ap4_llrs, ApLevel::Ap4, &ctx, None);

    assert_eq!(
        ap3_llrs[0..74],
        ap4_llrs[0..74],
        "AP4 must not alter bits 0..74 relative to AP3"
    );
    // Bits 74..77: AP3 leaves them untouched (0.0); AP4 forces i3=1.
    for i in 74..77 {
        assert_eq!(ap3_llrs[i], 0.0, "AP3 must not touch i3 bits");
    }
    assert_ne!(
        ap4_llrs[74..77],
        ap3_llrs[74..77],
        "AP4 must inject something at the i3 bits that AP3 does not"
    );
}

// ============================================================================
// Task W1.2: FT4 AP injection ignores XOR whitening
// ============================================================================
//
// FT4 payloads are XOR-scrambled with `FT4_XOR_SEQUENCE` *before* LDPC
// encoding (`encoder.rs::payload_to_symbols_protocol`) and un-scrambled the
// same way after LDPC decode + CRC check (`decoder.rs::par_apply_xor`).
// `inject_ap_llrs` sets LLR signs directly on the codeword fed into LDPC
// decode — i.e. the SAME pre-un-XOR (whitened) domain `par_apply_xor`
// reverses — so an AP-injected "real" callsign/i3 bit must itself be
// whitened with the same sequence before its LLR sign is set. Before this
// fix, AP injected the raw (un-whitened) bit unconditionally, which is
// wrong at every payload bit position where `FT4_XOR_SEQUENCE` has a 1
// (roughly half the 77 payload bits) — actively fighting the correct
// decode instead of helping it.
//
// These tests establish the post-XOR ground truth via this project's own
// FT4 encoder (never hand-computed), matching the "own encoder as ground
// truth" pattern already used above for the i3 fix.

/// Extract the first 77 bits of the 174-bit LDPC codeword from FT4
/// symbols. This is the codeword's pre-un-XOR domain: for FT4 (whose
/// `ProtocolParams::xor_sequence` is `Some`), this is NOT the raw 77-bit
/// message payload — it's `payload XOR FT4_XOR_SEQUENCE` (see
/// `encoder.rs::payload_to_symbols_protocol`, which scrambles the payload
/// before it ever reaches LDPC encoding). This is exactly the domain
/// `inject_ap_llrs`'s LLRs live in.
fn extract_ft4_codeword_bits(symbols: &[u8]) -> BitVec {
    const LDPC_CODEWORD_BITS: usize = 174;
    // FT4 data symbol ranges (protocol.rs::FT4_DATA_RANGES): 5..34, 38..67,
    // 71..100 (29 + 29 + 29 = 87 data symbols x 2 bits/symbol = 174 bits).
    let data_ranges: [std::ops::Range<usize>; 3] = [5..34, 38..67, 71..100];

    let mut codeword_bits = Vec::with_capacity(LDPC_CODEWORD_BITS);
    for range in data_ranges {
        for i in range {
            let binary_value = gray_to_binary_4fsk(symbols[i]);
            codeword_bits.push((binary_value & 2) != 0);
            codeword_bits.push((binary_value & 1) != 0);
        }
    }
    assert_eq!(codeword_bits.len(), LDPC_CODEWORD_BITS);

    let mut payload = BitVec::with_capacity(PAYLOAD_BITS);
    for &b in &codeword_bits[0..PAYLOAD_BITS] {
        payload.push(b);
    }
    payload
}

/// Encode `message_text` as FT4 with this project's own encoder and
/// return the first 77 POST-XOR codeword bits (ground truth).
fn encode_ft4_to_codeword_bits(message_text: &str) -> BitVec {
    let mut encoder = Ft8Encoder::with_protocol(ProtocolParams::ft4());
    let symbols = encoder
        .encode_message_protocol(message_text, None)
        .unwrap_or_else(|e| panic!("failed to encode FT4 {message_text:?}: {e}"));
    extract_ft4_codeword_bits(&symbols)
}

/// THE key regression test for this task: AP1's injected LLR signs (own
/// callsign at bits 0-27) must match the FT4 encoder's real POST-XOR
/// codeword bits, not the raw (un-whitened) callsign bits.
///
/// Before the fix, `inject_ap_llrs` injected `my_call.bits` directly
/// regardless of protocol, which only agrees with the post-XOR ground
/// truth at positions where `FT4_XOR_SEQUENCE` happens to have a 0 bit —
/// roughly half the time it's wrong.
#[test]
fn ap1_ft4_injection_matches_post_xor_codeword_not_raw_callsign_bits() {
    // "K1DEF W1ABC RR73": K1DEF is the addressee (to_callsign, bits 0-27),
    // W1ABC is the sender (from_callsign, bits 29-56) — matching the
    // module doc's convention that we are always to_callsign for
    // anything we decode.
    let ground_truth = encode_ft4_to_codeword_bits("K1DEF W1ABC RR73");

    let my_call = MyCallAp::new("K1DEF").expect("K1DEF should encode");
    // Sanity: MyCallAp's raw (un-whitened) bits must differ from the
    // post-XOR ground truth at at least one position in 0..28 — otherwise
    // this test can't distinguish "whitened correctly" from "bug present
    // but coincidentally matches" for this call pair.
    let raw_differs_from_whitened = (0..28).any(|i| my_call.bits[i] != ground_truth[i]);
    assert!(
        raw_differs_from_whitened,
        "test call pair must exercise at least one FT4_XOR_SEQUENCE=1 bit \
         in 0..28, otherwise the whitening bug is untestable here"
    );

    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: None,
    };

    let mut llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut llrs, ApLevel::Ap1, &ctx, Some(&FT4_XOR_SEQUENCE));

    for i in 0..28 {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i}: AP1's injected LLR sign (for FT4) must match the \
             encoder's real post-XOR codeword bit, not the raw callsign bit"
        );
    }
}

/// Same as above, but for AP3 (both callsign fields: to_callsign at
/// bits 0-27 AND from_callsign at bits 29-56) — the from_callsign field
/// exercises whitening independently at a different byte/bit-position
/// offset than to_callsign.
#[test]
fn ap3_ft4_injection_matches_post_xor_codeword_both_callsign_fields() {
    let ground_truth = encode_ft4_to_codeword_bits("K1DEF W1ABC RR73");

    let my_call = MyCallAp::new("K1DEF").expect("K1DEF should encode");
    let qso =
        QsoAp::new("W1ABC", QsoApProgress::WaitingForConfirmation).expect("W1ABC should encode");
    assert!(
        (29..57).any(|i| qso.their_bits[i - 29] != ground_truth[i]),
        "test call pair must exercise at least one FT4_XOR_SEQUENCE=1 bit \
         in 29..57, otherwise the whitening bug is untestable here"
    );

    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: Some(qso),
    };

    let mut llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut llrs, ApLevel::Ap3, &ctx, Some(&FT4_XOR_SEQUENCE));

    for i in 0..28 {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i} (to_callsign) mismatch"
        );
    }
    for i in 29..57 {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i} (from_callsign) mismatch"
        );
    }
}

/// AP4's i3 bits (74-76) are also inside the 77-bit payload the FT4 XOR
/// scrambles, so they must be whitened too, matching a real RR73
/// message's post-XOR i3 field.
#[test]
fn ap4_ft4_i3_bits_match_post_xor_codeword() {
    let ground_truth = encode_ft4_to_codeword_bits("K1DEF W1ABC RR73");

    let my_call = MyCallAp::new("K1DEF").expect("K1DEF should encode");
    let qso =
        QsoAp::new("W1ABC", QsoApProgress::WaitingForConfirmation).expect("W1ABC should encode");
    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: Some(qso),
    };

    let mut llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut llrs, ApLevel::Ap4, &ctx, Some(&FT4_XOR_SEQUENCE));

    for i in 74..77 {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i} (i3 field): AP4's injected LLR sign must match the FT4 \
             encoder's real post-XOR i3 bits"
        );
    }
}

/// FT8 regression: the SAME test structure, run against FT8 (no
/// whitening — `ProtocolParams::ft8().xor_sequence` is `None`), must be
/// completely unaffected by this fix. FT8's post-"XOR" ground truth is
/// just the raw payload bits (there is no scrambling), so passing `None`
/// for `xor_sequence` must inject exactly the raw callsign bits, as
/// before this task.
#[test]
fn ap1_ap3_ft8_injection_unaffected_by_whitening_fix() {
    let ground_truth = encode_to_payload("K1DEF W1ABC RR73");

    let my_call = MyCallAp::new("K1DEF").expect("K1DEF should encode");
    let qso =
        QsoAp::new("W1ABC", QsoApProgress::WaitingForConfirmation).expect("W1ABC should encode");
    let ctx = ApContext {
        my_call: Some(my_call.clone()),
        recent_calls: vec![],
        active_qso: Some(qso.clone()),
    };

    // AP1: raw callsign bits, unchanged.
    let mut ap1_llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut ap1_llrs, ApLevel::Ap1, &ctx, None);
    for i in 0..28 {
        let injected_bit = ap1_llrs[i] < 0.0;
        assert_eq!(
            injected_bit, my_call.bits[i],
            "FT8 AP1 bit {i} must be the raw callsign bit"
        );
        assert_eq!(
            injected_bit, ground_truth[i],
            "FT8 AP1 bit {i} must also match the real (unscrambled) encoder payload bit"
        );
    }

    // AP3: both fields, raw bits, unchanged.
    let mut ap3_llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut ap3_llrs, ApLevel::Ap3, &ctx, None);
    for i in 0..28 {
        assert_eq!(
            ap3_llrs[i], ap1_llrs[i],
            "FT8 AP3 to_callsign bits must match AP1"
        );
    }
    for i in 29..57 {
        let injected_bit = ap3_llrs[i] < 0.0;
        assert_eq!(
            injected_bit,
            qso.their_bits[i - 29],
            "FT8 AP3 from_callsign bit {i} must be the raw callsign bit"
        );
        assert_eq!(
            injected_bit, ground_truth[i],
            "FT8 AP3 from_callsign bit {i} must also match the real encoder payload bit"
        );
    }

    // AP4: i3 bits, raw (false, false, true), unchanged.
    let mut ap4_llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut ap4_llrs, ApLevel::Ap4, &ctx, None);
    assert_eq!(
        [ap4_llrs[74] < 0.0, ap4_llrs[75] < 0.0, ap4_llrs[76] < 0.0],
        [false, false, true],
        "FT8 AP4 i3 bits must remain the raw (false,false,true) pattern"
    );
}

// ============================================================================
// Task W1.2 review follow-up: direct FT4-domain coverage for the two
// `inject_ap_llrs` siblings that share the identical XOR-whitening bug
// (`inject_ap2_caller` at offset 29, `inject_recent_call_at_called` at
// offset 0) — both are live production decode paths (AP2 candidate-caller
// injection and hb-043 my_call-less AP injection, respectively, per
// `decoder.rs`'s call sites), but were previously only exercised in the
// FT8/`None` no-op direction. These mirror
// `ap1_ft4_injection_matches_post_xor_codeword_not_raw_callsign_bits`'s
// exact methodology: real FT4 encoder round-trip as ground truth, plus a
// sanity check that the chosen callsign actually exercises a
// `FT4_XOR_SEQUENCE`=1 bit so the test can't false-pass on a coincidentally
// XOR-neutral input.

/// `inject_ap2_caller` injects a candidate caller's bits at offset 29
/// (`from_callsign`). For FT4, its injected LLR signs must match the real
/// post-XOR codeword bits at 29..57, not the raw (un-whitened) callsign
/// bits — same bug class as AP1/AP3/AP4, same fix (`xor_bit_at` inside
/// `inject_28_bits`), but previously unverified in the FT4 direction for
/// this specific function.
#[test]
fn ap2_caller_ft4_injection_matches_post_xor_codeword_not_raw_callsign_bits() {
    // "K1DEF W1ABC RR73": K1DEF is to_callsign (bits 0-27), W1ABC is
    // from_callsign / the caller (bits 29-56) — matching the module doc's
    // convention that we are always to_callsign for anything we decode.
    let ground_truth = encode_ft4_to_codeword_bits("K1DEF W1ABC RR73");

    let caller = RecentCallAp::new("W1ABC", 10.0).expect("W1ABC should encode");
    // Sanity: the raw (un-whitened) caller bits must differ from the
    // post-XOR ground truth at at least one position in 29..57, otherwise
    // this test can't distinguish "whitened correctly" from "bug present
    // but coincidentally matches" for this callsign.
    let raw_differs_from_whitened = (29..57).any(|i| caller.bits[i - 29] != ground_truth[i]);
    assert!(
        raw_differs_from_whitened,
        "test callsign must exercise at least one FT4_XOR_SEQUENCE=1 bit \
         in 29..57, otherwise the whitening bug is untestable here"
    );

    let mut llrs = vec![0.0f32; 77];
    inject_ap2_caller(&mut llrs, &caller, Some(&FT4_XOR_SEQUENCE));

    for i in 29..57 {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i}: inject_ap2_caller's injected LLR sign (for FT4) must \
             match the encoder's real post-XOR codeword bit, not the raw \
             caller callsign bit"
        );
    }

    // Bits outside 29..57 must be untouched (still the 0.0 no-injection
    // sentinel) — inject_ap2_caller only ever touches the caller field.
    for i in (0..29).chain(57..77) {
        assert_eq!(
            llrs[i], 0.0,
            "bit {i} outside the caller field must be untouched"
        );
    }
}

/// `inject_recent_call_at_called` injects a candidate callsign at offset 0
/// (`to_callsign`). For FT4, its injected LLR signs must match the real
/// post-XOR codeword bits at 0..28, not the raw callsign bits. This is the
/// hb-043 my_call-less AP injection path: a live production decode path
/// with the same whitening requirement as AP1's called-station injection,
/// but previously unverified in the FT4 direction for this specific
/// function.
#[test]
fn recent_call_at_called_ft4_injection_matches_post_xor_codeword_not_raw_callsign_bits() {
    let ground_truth = encode_ft4_to_codeword_bits("K1DEF W1ABC RR73");

    let call = RecentCallAp::new("K1DEF", 10.0).expect("K1DEF should encode");
    // Sanity: the raw (un-whitened) callsign bits must differ from the
    // post-XOR ground truth at at least one position in 0..28.
    let raw_differs_from_whitened = (0..28).any(|i| call.bits[i] != ground_truth[i]);
    assert!(
        raw_differs_from_whitened,
        "test callsign must exercise at least one FT4_XOR_SEQUENCE=1 bit \
         in 0..28, otherwise the whitening bug is untestable here"
    );

    let mut llrs = vec![0.0f32; 77];
    inject_recent_call_at_called(&mut llrs, &call, Some(&FT4_XOR_SEQUENCE));

    for i in 0..28 {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i}: inject_recent_call_at_called's injected LLR sign (for \
             FT4) must match the encoder's real post-XOR codeword bit, not \
             the raw callsign bit"
        );
    }

    // Bits outside 0..28 must be untouched.
    for i in 28..77 {
        assert_eq!(
            llrs[i], 0.0,
            "bit {i} outside the called field must be untouched"
        );
    }
}

// ============================================================================
// Task W2.6: ApLevel::Cq mask + RRR/RR73/73 full-message masks
// ============================================================================
//
// Same "own encoder as ground truth" TDD methodology as the i3 fix above:
// encode a real message with `Ft8Encoder`, extract its 77-bit payload, and
// assert the new masks match at the injected positions — never a hand-typed
// assumption about bit layout.

/// Ground truth, independent of the CQ mask: a real "CQ K1DEF FN42" message
/// encoded by this project's own encoder must have `to_callsign` (bits
/// 0-27) equal to the packed "CQ" special token, suffix flag (bit 28)
/// clear, and i3=1 (bits 74-76 = 0,0,1) — the same "standard message"
/// family AP4 assumes.
#[test]
fn encoder_cq_message_packs_cq_token_at_to_callsign_position() {
    use pancetta_ft8::ap::u32_to_bits_28;

    let payload = encode_to_payload("CQ K1DEF FN42");

    // pack28("CQ") = (2, 0) — verified directly against this project's own
    // encoder's special-token table (see `wsjtx_compat_tests.rs::
    // test_pack28_cq_matches_wsjtx` and `test_special_tokens_payload`).
    let cq_bits = u32_to_bits_28(2);
    for i in 0..28 {
        assert_eq!(
            payload[i], cq_bits[i],
            "bit {i}: CQ K1DEF FN42's to_callsign field must be the packed \"CQ\" token"
        );
    }
    assert!(
        !payload[28],
        "bit 28 (suffix flag) must be clear for plain CQ"
    );
    assert_eq!(
        i3_bits(&payload),
        [false, false, true],
        "CQ messages must encode i3=1, same standard-message family as RR73/RRR/73"
    );
}

/// THE key regression test: `ApLevel::Cq`'s injected LLR signs (bits
/// 0-27+28 and 74-76) must match the encoder's real "CQ K1DEF FN42"
/// ground truth established above.
#[test]
fn cq_mask_injection_matches_encoder_ground_truth() {
    let ground_truth = encode_to_payload("CQ K1DEF FN42");

    let ctx = ApContext::default();
    let mut llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut llrs, ApLevel::Cq, &ctx, None);

    for i in 0..29 {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i}: ApLevel::Cq's injected LLR sign must match the real \
             encoder's CQ K1DEF FN42 payload bit"
        );
    }
    for i in 74..77 {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i} (i3 field): ApLevel::Cq must match the real encoder's i3=1 bits"
        );
    }
}

/// `ApLevel::Cq` must require no `ApContext` at all — needs neither
/// `my_call` nor `active_qso`. Regression guard so a future change can't
/// silently make it context-dependent.
#[test]
fn cq_mask_requires_no_context() {
    let empty = ApContext::default();
    let mut llrs_empty = vec![0.0f32; 77];
    inject_ap_llrs(&mut llrs_empty, ApLevel::Cq, &empty, None);

    let full = ApContext {
        my_call: Some(MyCallAp::new("K1ABC").unwrap()),
        recent_calls: vec![RecentCallAp::new("W1AW", -3.0).unwrap()],
        active_qso: Some(QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).unwrap()),
    };
    let mut llrs_full = vec![0.0f32; 77];
    inject_ap_llrs(&mut llrs_full, ApLevel::Cq, &full, None);

    assert_eq!(
        llrs_empty, llrs_full,
        "ApLevel::Cq must inject identically regardless of ApContext contents"
    );
}

/// Ground truth for the full-message masks: real "K1ABC W1AW RR73" /
/// "... RRR" / "... 73" messages encoded by this project's own encoder
/// must have distinct `ir`/`igrid4` fields (bits 58-73) per token — this
/// is the content AP4 alone (i3-only) never pinned.
#[test]
fn encoder_confirmation_tokens_have_distinct_igrid4_fields() {
    let rr73 = encode_to_payload("K1ABC W1AW RR73");
    let rrr = encode_to_payload("K1ABC W1AW RRR");
    let bare73 = encode_to_payload("K1ABC W1AW 73");

    // ir bit (58) is 0 for all three (no R-prefix on any of them).
    assert!(!rr73[58], "RR73 ir bit must be 0");
    assert!(!rrr[58], "RRR ir bit must be 0");
    assert!(!bare73[58], "73 ir bit must be 0");

    // igrid4 fields (59-73) must all differ from each other.
    let igrid4_of = |p: &BitSlice| -> Vec<bool> { p[59..74].iter().by_vals().collect() };
    let rr73_igrid4 = igrid4_of(&rr73);
    let rrr_igrid4 = igrid4_of(&rrr);
    let bare73_igrid4 = igrid4_of(&bare73);
    assert_ne!(
        rr73_igrid4, rrr_igrid4,
        "RR73 and RRR must pack different igrid4 values"
    );
    assert_ne!(
        rr73_igrid4, bare73_igrid4,
        "RR73 and 73 must pack different igrid4 values"
    );
    assert_ne!(
        rrr_igrid4, bare73_igrid4,
        "RRR and 73 must pack different igrid4 values"
    );
}

/// THE key regression test for the full-message mask: for each of the
/// three confirmation tokens, `inject_confirmation_token_bits`'s injected
/// LLR signs (bits 58-73) must match the encoder's real ground truth for
/// that exact message.
#[test]
fn confirmation_token_mask_matches_encoder_ground_truth_for_all_three() {
    let cases = [
        (ConfirmationToken::RR73, "K1ABC W1AW RR73"),
        (ConfirmationToken::Rrr, "K1ABC W1AW RRR"),
        (ConfirmationToken::Final73, "K1ABC W1AW 73"),
    ];

    for (token, text) in cases {
        let ground_truth = encode_to_payload(text);
        let mut llrs = vec![0.0f32; 77];
        inject_confirmation_token_bits(&mut llrs, token, None);

        for i in 58..74 {
            let injected_bit = llrs[i] < 0.0;
            assert_eq!(
                injected_bit, ground_truth[i],
                "{text:?}: bit {i} (ir/igrid4 field) mismatch for token {token:?}"
            );
        }
        // Bits outside 58..74 untouched.
        for i in (0..58).chain(74..77) {
            assert_eq!(
                llrs[i], 0.0,
                "{text:?}: bit {i} outside ir/igrid4 must be untouched by \
                 inject_confirmation_token_bits"
            );
        }
    }
}

/// Combined AP4 + full mask, exactly as the decoder applies it
/// (`inject_ap_llrs(Ap4)` then `inject_confirmation_token_bits`), must
/// match the encoder's real full "K1ABC W1AW RR73" payload at every
/// injected position: callsigns (0-27, 29-56), i3 (74-76), AND now
/// ir/igrid4 (58-73) too.
#[test]
fn ap4_plus_full_mask_matches_full_encoder_payload_for_rr73() {
    let ground_truth = encode_to_payload("K1ABC W1AW RR73");

    let my_call = MyCallAp::new("K1ABC").expect("K1ABC should encode");
    let qso =
        QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).expect("W1AW should encode");
    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: Some(qso),
    };

    let mut llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut llrs, ApLevel::Ap4, &ctx, None);
    inject_confirmation_token_bits(&mut llrs, ConfirmationToken::RR73, None);

    // Every payload bit this combined injection touches (0-27, 29-56,
    // 58-76 — everything except the untouched bit-28 suffix-flag gap)
    // must match the real encoder ground truth.
    for i in (0..28).chain(29..57).chain(58..77) {
        let injected_bit = llrs[i] < 0.0;
        assert_eq!(
            injected_bit, ground_truth[i],
            "bit {i}: AP4 + full RR73 mask must match the real encoder payload"
        );
    }
}
