//! PAN-7 — ft8_lib sync-candidate seed extraction and pass-0 union.
//!
//! Integration tests that need the **real** vendored ft8_lib C library.
//! File-gated on `not(ft8lib_stub)` so a stub build skips them outright rather
//! than passing vacuously and hiding a broken translator, and on `transmit`
//! because `Ft8Encoder` / `Ft8Modulator` — used to build the synthetic signals —
//! are re-exported only under that feature.
#![cfg(all(not(ft8lib_stub), feature = "transmit"))]

use pancetta_ft8::ft8_lib_ffi::{ft8lib_find_candidate_seeds, ftx_protocol_t, Ft8LibSeedSet};
use pancetta_ft8::{Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};

/// Encode + modulate `text` at `freq_offset` Hz into one full FT8 window,
/// zero-padded to `WINDOW_SAMPLES` so the monitor sees a whole slot.
fn synth_window(text: &str, freq_offset: f64) -> Vec<f32> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message(text, None).expect("encode");
    let mut modulator = Ft8Modulator::new_default().expect("modulator");
    let mut samples = modulator
        .modulate_symbols(&symbols, freq_offset)
        .expect("modulate");
    samples.resize(WINDOW_SAMPLES, 0.0);
    samples
}

/// ft8_lib's own reported frequency for a seed, using the upstream demo's
/// formula (`vendor/ft8_lib/demo/decode_ft8.c`, MIT).
fn top_seed_hz(set: &Ft8LibSeedSet) -> f32 {
    let s = set.seeds[0];
    (set.min_bin as f32 + s.freq_offset as f32 + s.freq_sub as f32 / set.freq_osr as f32)
        / set.symbol_period
}

#[test]
fn find_candidate_seeds_returns_descending_scores_for_a_synthetic_signal() {
    let tx = synth_window("CQ K5ARH EM10", 0.0);
    let set = ft8lib_find_candidate_seeds(&tx, ftx_protocol_t::FTX_PROTOCOL_FT8, 200);

    assert!(
        !set.seeds.is_empty(),
        "ft8_lib found no candidates in a clean synthetic signal — the monitor was not fed correctly"
    );
    assert!(
        set.seeds.windows(2).all(|w| w[0].score >= w[1].score),
        "ftx_find_candidates heap-sorts descending (vendor/ft8_lib/ft8/decode.c:235-250); \
         the seed set must preserve that order"
    );

    // The monitor scalars are what make the returned coordinates interpretable;
    // they must be copied out before monitor_free, and they must match the
    // already-measured ft8_lib configuration (f_min 100 Hz, f_max 3000 Hz).
    assert_eq!(
        set.min_bin, 16,
        "monitor_config f_min=100Hz maps to min_bin 16"
    );
    assert_eq!(set.time_osr, 2);
    assert_eq!(set.freq_osr, 2);
    assert_eq!(set.block_size, 1920, "FT8 block_size at 12 kHz");
    assert!(set.num_bins > 0 && set.num_blocks > 0);
}

#[test]
fn find_candidate_seeds_respects_the_requested_budget() {
    let tx = synth_window("CQ K5ARH EM10", 0.0);

    let five = ft8lib_find_candidate_seeds(&tx, ftx_protocol_t::FTX_PROTOCOL_FT8, 5);
    assert!(
        five.seeds.len() <= 5,
        "requested 5 seeds, got {}",
        five.seeds.len()
    );

    // Zero must short-circuit: never hand ftx_find_candidates a dangling heap.
    let none = ft8lib_find_candidate_seeds(&tx, ftx_protocol_t::FTX_PROTOCOL_FT8, 0);
    assert!(
        none.seeds.is_empty(),
        "a zero budget must return an empty set without calling into C"
    );
}

#[test]
fn top_seed_tracks_the_transmitted_signal_position() {
    let a = ft8lib_find_candidate_seeds(
        &synth_window("CQ K5ARH EM10", 0.0),
        ftx_protocol_t::FTX_PROTOCOL_FT8,
        50,
    );
    let b = ft8lib_find_candidate_seeds(
        &synth_window("CQ K5ARH EM10", 100.0),
        ftx_protocol_t::FTX_PROTOCOL_FT8,
        50,
    );
    assert!(!a.seeds.is_empty() && !b.seeds.is_empty());

    // A +100 Hz transmit shift must move the strongest seed by ~100 Hz. This is
    // self-referential (no hardcoded base frequency) and catches a min_bin or
    // freq_sub mistake in the scalar copy-out.
    let delta = top_seed_hz(&b) - top_seed_hz(&a);
    assert!(
        (delta - 100.0).abs() < 7.0,
        "a +100 Hz shift should move the top seed ~100 Hz, got {delta}"
    );

    // The signal starts at sample 0, so the strongest candidate sits at dt ≈ 0.
    let s = a.seeds[0];
    let t = (s.time_offset as f32 + s.time_sub as f32 / a.time_osr as f32) * a.symbol_period;
    assert!(
        t.abs() < 0.5,
        "expected dt≈0 for a window-aligned signal, got {t}"
    );
}
