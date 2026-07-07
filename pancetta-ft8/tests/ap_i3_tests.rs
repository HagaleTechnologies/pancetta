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
use pancetta_ft8::ap::{inject_ap_llrs, ApContext, ApLevel, MyCallAp, QsoAp, QsoApProgress};
use pancetta_ft8::ldpc::gray_to_binary;
use pancetta_ft8::message::PAYLOAD_BITS;
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
    inject_ap_llrs(&mut llrs, ApLevel::Ap4, &ctx);

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
    inject_ap_llrs(&mut ap3_llrs, ApLevel::Ap3, &ctx);

    let mut ap4_llrs = vec![0.0f32; 77];
    inject_ap_llrs(&mut ap4_llrs, ApLevel::Ap4, &ctx);

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
