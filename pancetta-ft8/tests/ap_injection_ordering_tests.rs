//! Task W1.7: AP1-AP4 callsign-field injection ordering + offset fix.
//!
//! Spec ref: `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`.
//! Full writeup: `research/experiments/2026-07-07-w1.7-ap-injection-ordering-fix.md`,
//! which extends the finding first surfaced in
//! `research/experiments/2026-07-07-w1.1-ap4-i3-fix.md` (Task W1.1).
//!
//! ## The bug
//!
//! `pancetta-ft8/src/ap.rs`'s `inject_ap_llrs` (AP1/AP2/AP3/AP4 arms) injected
//! callsigns at the wrong payload offsets AND with the two callsigns'
//! semantic roles swapped:
//!
//! - AP1 injected our own callsign at offset 28 (bits 28-55).
//! - AP3/AP4 injected the QSO partner's callsign at offset 0 (bits 0-27) and
//!   our own callsign at offset 28 (bits 28-55).
//! - `inject_ap2_caller` injected the candidate caller at offset 0; its
//!   companion `inject_recent_call_at_called` injected at offset 28.
//!
//! But per `message.rs::parse_type1_standard` (the ground truth for i3=1/2
//! "standard message" payloads, the only family AP1-4 target): `to_callsign`
//! occupies bits 0-27 (offset 0) and `from_callsign` occupies bits 29-56
//! (offset 29) — there is a 1-bit gap at bit 28 (the `to_callsign` suffix
//! flag) between the two 28-bit fields, so they are NOT evenly spaced at
//! offsets 0 and 28.
//!
//! We are always the addressee (`to_callsign`) for any message we decode (we
//! never decode our own transmissions), so the injection had the two
//! callsigns' roles backwards (our call belongs at `to_callsign`/offset 0,
//! not offset 28; the other station belongs at `from_callsign`/offset 29,
//! not offset 0) AND was additionally off-by-one at the offset-28 boundary.
//!
//! `decoder.rs::ap_injection_survived` (written independently) already
//! required the correct assignment (`to_callsign == my_call`,
//! `from_callsign == qso.their_call` for AP3/AP4; `to_callsign == my_call`
//! for AP1/AP2) — so the old injection could only survive verification when
//! LDPC belief propagation overrode the wrong prior entirely and reconstructed
//! the true message from the untouched redundant bits alone (in which case
//! AP added no value AP0 didn't already have), or failed verification
//! outright. Confirmed both by code-reading and by an A/B test below that
//! reverts to the pre-fix injection code and shows it fails to rescue a
//! decode that the fixed code rescues, holding everything else (including
//! the exact audio bytes) constant.
//!
//! ## The fix
//!
//! - AP1: inject own callsign at offset 0 (was 28).
//! - AP3/AP4: inject own callsign at offset 0 (was 28), QSO partner at
//!   offset 29 (was 0).
//! - `inject_ap2_caller`: offset 29 (was 0) — candidate caller is always
//!   `from_callsign`.
//! - `inject_recent_call_at_called`: offset 0 (was 28) — candidate "called"
//!   station is always `to_callsign`.
//! - `enumerate_a8_expected_texts`: swapped from `"{dx} {my} ..."` to
//!   `"{my} {dx} ..."` — `Ft8Message::Display` renders `to_callsign` first,
//!   and we are always `to_callsign` in a message directed at us.
//!
//! ## Empirical note on AP0-fails/AP-rescues test design
//!
//! A naive "corrupt/erase the callsign-field audio, then compare AP0 vs
//! AP-context" test does NOT discriminate between the buggy and fixed
//! injection at every corruption severity: LDPC belief propagation, given a
//! sufficiently large number of iterations and enough clean redundant bits
//! elsewhere, can escape a degenerate near-zero-LLR deadlock from ANY
//! confident prior (right or wrong) and converge on the unique codeword
//! consistent with those clean bits — independent of whether the prior's
//! *value* was correct. This was verified empirically (see the research log)
//! across several corruption patterns before landing on the specific
//! combination below, which — at the crate's *default* `Ft8Config`
//! (`ldpc_iterations = 100`, all other defaults) — genuinely discriminates:
//! reverting only `ap.rs` to the pre-fix injection (holding the exact same
//! audio bytes, config, and everything else constant) causes the rescue to
//! disappear.

#![cfg(feature = "transmit")]

use pancetta_ft8::ap::{ApContext, MyCallAp, QsoAp, QsoApProgress};
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};

mod test_signal_generator;

const SAMPLES_PER_SYMBOL: usize = 1920; // 0.16s * 12000 Hz = SYMBOL_DURATION * SAMPLE_RATE

/// Build a QSO-context signal: global noise across the whole 12.64s window
/// (stresses the redundant/parity + content bits, thinning LDPC's recovery
/// margin), PLUS an additional noise burst stacked on top of just the
/// callsign-field data tones (payload bits 0..57 live in data tones 7..26,
/// right after the leading 7-tone Costas array — see `ap.rs`'s bit-layout
/// doc comment). Costas sync tones (0..7, 36..43, 72..79) are only touched
/// by the mild global noise, so sync always locks and only the LDPC pass is
/// stressed.
fn build_qso_context_signal(
    text: &str,
    global_snr_db: f32,
    callsign_field_snr_db: f32,
) -> Vec<f32> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message(text, None)
        .unwrap_or_else(|e| panic!("failed to encode {text:?}: {e}"));
    let mut modulator = Ft8Modulator::new_default().expect("modulator");
    let mut audio = modulator.modulate_symbols(&symbols, 0.0).expect("modulate");
    audio.resize(pancetta_ft8::WINDOW_SAMPLES, 0.0);

    test_signal_generator::add_gaussian_noise(&mut audio, global_snr_db);

    let start = 7 * SAMPLES_PER_SYMBOL;
    let end = 26 * SAMPLES_PER_SYMBOL;
    test_signal_generator::add_gaussian_noise(&mut audio[start..end], callsign_field_snr_db);

    audio
}

/// THE key end-to-end regression test for this task: a QSO-context signal
/// at a noise level where AP0 (no prior) fails to decode, but AP3's
/// now-correctly-placed callsign priors (own call at `to_callsign`, QSO
/// partner at `from_callsign`) rescue it.
///
/// The scenario simulates an operator (K1ABC) mid-QSO with W1AW, expecting
/// W1AW's RR73 confirmation (`QsoApProgress::WaitingForConfirmation`) —
/// exactly the state that gates AP3/AP4 in the production decode path
/// (`decoder.rs`'s `par_decode_candidate`/`try_ldpc_with_ap` call sites).
///
/// This scenario was found empirically (see module docs) to discriminate:
/// with the pre-fix (backwards + off-by-one) injection, AP-context decoding
/// does NOT rescue this signal (verified manually against the pre-fix
/// `ap.rs` with byte-identical audio and config — see the research log for
/// the A/B transcript). With the fix, it does.
#[test]
fn ap3_rescues_qso_context_signal_that_ap0_cannot_decode() {
    let text = "K1ABC W1AW RR73";
    // Callsign-field SNR recalibrated -20.0 -> -24.0 (Task W1.4,
    // decoder-TP-sensitivity plan, 2026-07-07): that task flipped
    // `Ft8Config::default().llr_whitening_enabled` true -> false after a
    // unit-consistency fix showed the (now-correctly-applied) whitening
    // costs real recall. With whitening off the decoder is measurably
    // more sensitive, so the previous -20.0 dB callsign-field noise was
    // no longer harsh enough to force a full AP3 (own+partner callsign)
    // rescue — this exact signal started succeeding at AP1 (own call
    // only) instead. -24.0 dB restores the intended discrimination
    // (AP0 fails, AP1 alone is not enough, AP3 rescues) under the new
    // default. Uses `Ft8Config::default()` deliberately (not a pinned
    // config), so it's expected to need occasional recalibration as
    // default-on decoder strength changes — that's a feature of testing
    // against the real production config, not a design flaw.
    let audio = build_qso_context_signal(text, -28.0, -24.0);

    // AP0 (no context at all) must fail on this signal.
    let mut plain_decoder = Ft8Decoder::new(Ft8Config::default()).expect("decoder");
    let ap0_results = plain_decoder.decode_window(&audio).unwrap_or_default();
    assert!(
        !ap0_results.iter().any(|m| m.text == text),
        "AP0 must NOT decode this signal (test is only meaningful if the \
         baseline genuinely fails); got: {:?}",
        ap0_results.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // AP-context decoding (my_call = K1ABC, active QSO with W1AW awaiting
    // confirmation) must rescue it via AP3's corrected callsign injection.
    let my_call = MyCallAp::new("K1ABC").expect("K1ABC should encode");
    let qso =
        QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).expect("W1AW should encode");
    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: Some(qso),
    };
    let mut ap_decoder = Ft8Decoder::new(Ft8Config::default()).expect("decoder");
    let ap_results = ap_decoder
        .decode_window_with_ap(&audio, &ctx)
        .unwrap_or_default();

    let rescued = ap_results.iter().find(|m| m.text == text);
    assert!(
        rescued.is_some(),
        "AP-context decoding must rescue the signal that AP0 could not \
         decode; got: {:?}",
        ap_results.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    assert_eq!(
        rescued.unwrap().ap_level,
        3,
        "the rescue must come from AP3 (own call + QSO partner callsign \
         priors), confirming the corrected field placement — not AP1/AP2 \
         (which only know one callsign) or a coincidental AP0-equivalent hit"
    );
}
