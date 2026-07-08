//! Task W2.6: AP coverage — CQ mask, post-normalization injection,
//! RR73/RRR/73 full masks.
//!
//! Spec ref: `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`.
//! Bit-level TDD (encoder ground truth) lives in
//! `pancetta-ft8/tests/ap_i3_tests.rs`. This file provides the end-to-end
//! audio-domain rescue tests, following the exact methodology established
//! by `ap_injection_ordering_tests.rs` (Task W1.7): global noise (stresses
//! LDPC's recovery margin) optionally stacked with a targeted burst over
//! the data tones carrying the injected bits, discriminating "AP0 (or a
//! weaker AP level) fails to converge" from "the new mask rescues it."

#![cfg(feature = "transmit")]

use pancetta_ft8::ap::{ApContext, MyCallAp, QsoAp, QsoApProgress};
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};

mod test_signal_generator;

const SAMPLES_PER_SYMBOL: usize = 1920; // 0.16s * 12000 Hz = SYMBOL_DURATION * SAMPLE_RATE

/// Build a signal with global noise across the whole 12.64s window, plus an
/// additional noise burst over the data tones carrying payload bits 0..57
/// (tones 7..26 — right after the leading 7-tone Costas array; see
/// `ap.rs`'s bit-layout doc comment). Mirrors
/// `ap_injection_ordering_tests.rs::build_qso_context_signal` exactly.
fn build_signal(text: &str, global_snr_db: f32, field_snr_db: f32) -> Vec<f32> {
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
    test_signal_generator::add_gaussian_noise(&mut audio[start..end], field_snr_db);

    audio
}

/// Config-level regression guard: `cq_ap_enabled`, `ap4_full_message_mask_enabled`,
/// and `ap_injection_post_normalization` must all default to `false` — the
/// byte-identical-when-off invariant every [A/B] task in this plan
/// maintains.
#[test]
fn w26_flags_default_off() {
    let cfg = Ft8Config::default();
    assert!(!cfg.cq_ap_enabled, "cq_ap_enabled must default to false");
    assert!(
        !cfg.ap4_full_message_mask_enabled,
        "ap4_full_message_mask_enabled must default to false"
    );
    assert!(
        !cfg.ap_injection_post_normalization,
        "ap_injection_post_normalization must default to false"
    );
}

/// Empirical note on the CQ mask's audio-domain rescue search (honest
/// negative finding, not glossed over): using the exact same global-noise +
/// targeted-burst methodology that `ap_injection_ordering_tests.rs`
/// (Task W1.7) used to discriminate AP3's callsign-field fix, an extensive
/// grid search (~106 (global_snr_db, field_snr_db) combinations spanning
/// -25.0 to -34.0 dB global noise at 0.1-0.2 dB resolution near the
/// AP0-fails transition, and burst depths from -14 to -40 dB) over a
/// real encoded+modulated "CQ K5ARH EM10" signal found **zero** points
/// where `ApLevel::Cq` rescued a decode that AP0 (no context) could not
/// already produce: at every noise level tested, either AP0 already
/// decoded it too (both succeed) or nothing decoded at all (both fail) —
/// no window where the CQ-token bias alone tips LDPC belief propagation
/// into convergence appeared for this scenario. Plausible reason: the
/// packed "CQ" special token (value 2, mostly zero bits in the 28-bit
/// field) constrains far less real information than a genuine callsign
/// does (AP1/AP3's injected fields, which correlate with a real, higher-
/// entropy 28-bit value) — the injected prior itself is "thin." This is
/// exactly the kind of empirical caveat W1.7 flagged for its own
/// methodology; the corpus-level A/B (hard-200 + noise_1000, many
/// candidates, real recorded audio) is the actual arbiter for this
/// mechanism, not a single hand-tuned synthetic signal — see the W2.6
/// experiment log for those results. No forced "rescue" unit test is
/// asserted here since one could not be constructed honestly; the
/// plumbing/gating correctness (does the mechanism activate/decode
/// correctly when asked, independent of whether it helps net recall) is
/// instead covered by the bit-level TDD in `ap_i3_tests.rs` and the
/// internal `w26_cq_mask_tests` module in `decoder.rs` (direct-function
/// test calling `par_try_ldpc_with_cq`).
#[test]
fn cq_mask_search_note_no_audio_domain_rescue_found_this_scenario() {
    // Intentionally not a real assertion beyond "doesn't panic" — this
    // test exists to anchor the note above in the permanent test suite
    // rather than only in a throwaway calibration script. The actual
    // negative finding is documented in the doc comment and the W2.6
    // experiment log.
}

/// THE key end-to-end rescue test for the AP4 full message-content mask:
/// a "K1ABC W1AW RR73" QSO-context signal noisy enough that BOTH AP0 and
/// the plain (i3-only) AP4 fail to decode it, but the full mask
/// (`ap4_full_message_mask_enabled: true`, trying RR73/RRR/73 content
/// bits) rescues it.
#[test]
fn ap4_full_mask_rescues_signal_that_plain_ap4_cannot_decode() {
    let text = "K1ABC W1AW RR73";
    // Calibrated (grid search, 0.2 dB resolution): -31.0/-27.0 still lets
    // plain AP4 decode; -33.2/-29.2 and below fails even the full mask.
    // -32.5/-28.5 sits in the middle of the confirmed 6-point robust
    // window ([-32.0..-33.0] global / [-28.0..-29.0] burst) where plain
    // AP4 reliably fails but the full mask reliably rescues.
    let audio = build_signal(text, -32.5, -28.5);

    let my_call = MyCallAp::new("K1ABC").expect("K1ABC should encode");
    let qso =
        QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).expect("W1AW should encode");
    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: Some(qso),
    };

    // AP0 must fail.
    let mut plain_decoder = Ft8Decoder::new(Ft8Config::default()).expect("decoder");
    let ap0_results = plain_decoder.decode_window(&audio).unwrap_or_default();
    assert!(
        !ap0_results.iter().any(|m| m.text == text),
        "AP0 must NOT decode this signal; got: {:?}",
        ap0_results.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Plain AP4 (i3-only, ap4_full_message_mask_enabled = false, the
    // default) must ALSO fail — otherwise this test can't discriminate
    // "the full mask adds value" from "AP4 alone was already enough."
    let mut ap4_decoder = Ft8Decoder::new(Ft8Config::default()).expect("decoder");
    let ap4_results = ap4_decoder
        .decode_window_with_ap(&audio, &ctx)
        .unwrap_or_default();
    assert!(
        !ap4_results.iter().any(|m| m.text == text),
        "plain (i3-only) AP4 must NOT decode this signal (test is only \
         meaningful if the plain-AP4 baseline genuinely fails too); got: {:?}",
        ap4_results.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Full mask enabled must rescue it.
    let full_mask_config = Ft8Config {
        ap4_full_message_mask_enabled: true,
        ..Ft8Config::default()
    };
    let mut full_decoder = Ft8Decoder::new(full_mask_config).expect("decoder");
    let full_results = full_decoder
        .decode_window_with_ap(&audio, &ctx)
        .unwrap_or_default();

    let rescued = full_results.iter().find(|m| m.text == text);
    assert!(
        rescued.is_some(),
        "the AP4 full message-content mask must rescue the signal that \
         both AP0 and plain AP4 could not decode; got: {:?}",
        full_results.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    assert_eq!(
        rescued.unwrap().ap_level,
        4,
        "the rescue must be reported as AP4-family"
    );
}
