//! Cycle-level decode API tests (PAN-176).
//!
//! `decode_cycle` / `CycleDecoder` accept one full protocol cycle of 12 kHz
//! audio (FT8: 180 000 samples / 15 s, FT4: 90 000 samples / 7.5 s, sample 0
//! at the slot boundary) and must return exactly the composition the station
//! coordinator runs with neutral state:
//!
//! - C reference pass: `Ft8Decoder::decode_window_ft8lib_protocol` on the
//!   FULL cycle (the sliding-frame C monitor keeps the cycle tail a
//!   WINDOW_SAMPLES truncation would discard)
//! - native AP pass: `decode_window_with_ap_scoped` with
//!   `ApContext::default()`, no frequency scope, unlimited budget, on the
//!   first `WINDOW_SAMPLES` samples (the native decoder's documented window)
//! - merge: `merge_ft8lib_first`
//! - post-merge: `drop_unresolved_hash_twins` (the station's delivered-set
//!   pass)
//!
//! Parity contract: message texts and frequency offsets match exactly;
//! SNR within 1.0 dB and DT within 0.15 s of the hand-run composition
//! (the tolerances exist only because the live station window carries a
//! 0.5 s lead-in slice the cycle API does not).

use pancetta_ft8::merge::{drop_unresolved_hash_twins, merge_ft8lib_first};
use pancetta_ft8::{
    decode_cycle, ApContext, CycleDecoder, DecodedMessage, Ft8Config, Ft8Decoder, Protocol,
    ProtocolParams, SAMPLE_RATE, WINDOW_SAMPLES,
};

fn fixture(subpath: &str) -> String {
    format!(
        "{}/tests/fixtures/wav/{}",
        env!("CARGO_MANIFEST_DIR"),
        subpath
    )
}

fn read_wav_file(path: &str) -> Vec<f32> {
    let reader =
        hound::WavReader::open(path).unwrap_or_else(|e| panic!("Failed to open {}: {}", path, e));
    let spec = reader.spec();
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.sample_rate, SAMPLE_RATE);
    reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

/// The station composition run by hand on the same views the cycle API
/// uses, with neutral context. Mirrors `pancetta/src/coordinator/ft8.rs`'s
/// decode loop (C pass on the full cycle, native AP pass on the
/// WINDOW_SAMPLES window, `merge_ft8lib_first`, unresolved-hash twin
/// suppression) minus the station-only QSO context, narrow filter and
/// wall-clock budget (the plain scoped entry point delegates with
/// `DecodeBudget::unlimited()`).
fn station_composition_neutral(samples: &[f32], protocol: Protocol) -> Vec<DecodedMessage> {
    // Same view prep as decode_cycle: the C pass reads the full cycle
    // (capped at one cycle length), the native pass reads the first
    // WINDOW_SAMPLES samples, zero-padded when short.
    let cycle_len = ProtocolParams::from_protocol(protocol).window_samples(SAMPLE_RATE);
    let cycle: Vec<f32> = if samples.len() >= cycle_len {
        samples[..cycle_len].to_vec()
    } else {
        let mut padded = samples.to_vec();
        padded.resize(cycle_len, 0.0);
        padded
    };
    let window: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
        samples[..WINDOW_SAMPLES].to_vec()
    } else {
        let mut padded = samples.to_vec();
        padded.resize(WINDOW_SAMPLES, 0.0);
        padded
    };

    let ft8lib = Ft8Decoder::decode_window_ft8lib_protocol(&cycle, protocol);
    let config = Ft8Config {
        protocol,
        ..Ft8Config::default()
    };
    let mut decoder = Ft8Decoder::new(config).expect("decoder ctor");
    let native = decoder
        .decode_window_with_ap_scoped(&window, &ApContext::default(), None)
        .expect("native decode");
    let merged = merge_ft8lib_first(ft8lib, native).messages;
    drop_unresolved_hash_twins(merged, protocol).0
}

/// Assert `got` matches `want` under the parity contract: text and frequency
/// exact, SNR within 1.0 dB, DT within 0.15 s.
fn assert_parity(label: &str, want: &[DecodedMessage], got: &[DecodedMessage]) {
    assert_eq!(
        want.len(),
        got.len(),
        "{label}: decode count differs (station composition {} vs decode_cycle {})",
        want.len(),
        got.len()
    );
    for (w, g) in want.iter().zip(got.iter()) {
        assert_eq!(w.text, g.text, "{label}: text mismatch");
        assert_eq!(
            w.frequency_offset, g.frequency_offset,
            "{label}: frequency mismatch for '{}'",
            w.text
        );
        assert!(
            (w.snr_db - g.snr_db).abs() <= 1.0 + 1e-6,
            "{label}: SNR out of tolerance for '{}' ({} vs {})",
            w.text,
            w.snr_db,
            g.snr_db
        );
        assert!(
            (w.time_offset - g.time_offset).abs() <= 0.15 + 1e-6,
            "{label}: DT out of tolerance for '{}' ({} vs {})",
            w.text,
            w.time_offset,
            g.time_offset
        );
    }
}

// ---------------------------------------------------------------------------
// 1. Parity by construction, FT8, across the committed 15 s fixtures
// ---------------------------------------------------------------------------

#[test]
fn ft8_cycle_matches_station_composition_on_all_fixtures() {
    let fixtures = [
        "basicft8/170923_082000.wav",
        "basicft8/170923_082015.wav",
        "basicft8/170923_082030.wav",
        "basicft8/170923_082045.wav",
        "basicft8/live_now.wav",
        "jtdx/190227_155815.wav",
        "generated/ft8_cq.wav",
    ];
    for sub in fixtures {
        let samples = read_wav_file(&fixture(sub));
        let got = decode_cycle(&samples, Protocol::Ft8).expect("decode_cycle");
        let want = station_composition_neutral(&samples, Protocol::Ft8);
        println!("{sub}: {} decodes via decode_cycle", got.len());
        assert_parity(sub, &want, &got);
    }
}

// ---------------------------------------------------------------------------
// 2. Golden decode sets (recorded from the composition on the committed
//    fixtures; a regression in the composition changes these)
// ---------------------------------------------------------------------------

/// The recorded decode set for `jtdx/190227_155815.wav` via the exact
/// composition `decode_cycle` runs (C pass on the FULL 180 000-sample
/// cycle + neutral native pass on the 151 680-sample window + PAN-110
/// merge + unresolved-hash twin suppression). The PAN-176 plan's Design
/// section quoted 26 messages, but that count came from
/// `wav_decode_tests::decode_wav_file`, which feeds the UNTRIMMED 180 000
/// samples to the native pass alone — one decode (`IE2JUO ZP1EUJ R HL33`,
/// a native-full-buffer-only decode at the edge of the candidate budget)
/// is absent here. Review round 1 keeps the native pass bounded (its
/// documented window), so that decode remains out of the cycle API's set.
const JTDX_190227_155815: [&str; 25] = [
    "UN7LZ RV3DQC -01",
    "R9AA IK5EEA 73",
    "CQ RD3FC KO95",
    "UA3AIU UR5GCK -03",
    "CQ UA3QNE LO01",
    "YD1GNL UN7ZAI MO51",
    "YD1GNL UA8CW MO07",
    "G3VAO UA3GDJ KO92",
    "CQ A41ZZ LL93",
    "EA7DGC IK8DYE 73",
    "CQ ES6DO KO27",
    "YB3BBF SV2HXX R-20",
    "KA7RLM R7NW R-23",
    "UA6LIK SP3AVS R+00",
    "EA1IPH UN7LAY -16",
    "LB8ZH R2FAQ KO04",
    "CQ RA9CAD LO96",
    "CQ R9AAB MO04",
    "GD3YUM YB0BTA OI33",
    "YD1GNL R3KAB LN09",
    "R1BEQ UN7FU 73",
    "CQ YD0SCZ OI33",
    "CQ RT4S LO36",
    "GD3YUM BG4QNE PM01",
    "EA3HRU R6LCM RR73",
];

#[test]
fn ft8_cycle_golden_sets() {
    // jtdx fixture: exact text list (order as returned).
    let samples = read_wav_file(&fixture("jtdx/190227_155815.wav"));
    let got = decode_cycle(&samples, Protocol::Ft8).expect("decode_cycle");
    let texts: Vec<&str> = got.iter().map(|m| m.text.as_str()).collect();
    println!(
        "jtdx/190227_155815.wav golden set ({} messages):",
        texts.len()
    );
    for t in &texts {
        println!("  {t}");
    }
    assert_eq!(texts.len(), JTDX_190227_155815.len());
    for (want, got_text) in JTDX_190227_155815.iter().zip(texts.iter()) {
        assert_eq!(*want, *got_text);
    }

    // Generated fixtures: each carries exactly its encoded message.
    for (sub, text) in [
        ("generated/ft8_cq.wav", "CQ K1ABC FN42"),
        ("generated/ft8_report.wav", "K1DEF W1ABC -12"),
        ("generated/ft8_rr73.wav", "K1DEF W1ABC RR73"),
    ] {
        let samples = read_wav_file(&fixture(sub));
        let got = decode_cycle(&samples, Protocol::Ft8).expect("decode_cycle");
        println!("{sub}: {} decodes", got.len());
        assert_eq!(
            got.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            vec![text],
            "{sub}: golden decode set changed"
        );
    }
}

// ---------------------------------------------------------------------------
// 2b. Full-cycle recall: the C reference pass must see the whole cycle
// ---------------------------------------------------------------------------

/// PR #550 review round 1: truncating the C pass's input at WINDOW_SAMPLES
/// discards the tail of the cycle — real signal for transmissions that
/// extend past 12.64 s — and measurably loses decodes on committed fixtures
/// (`basicft8/live_now.wav` loses `VO1AE KA3KQO FM29`; the native pass does
/// not recover it). The station, likewise, retains audio through emission
/// time (`boundary_anchored_slice` in `pancetta/src/coordinator/dsp.rs`).
/// This test is red when the C pass is fed the truncated window.
#[test]
fn ft8_cycle_keeps_full_cycle_recall_on_live_now() {
    let samples = read_wav_file(&fixture("basicft8/live_now.wav"));
    let got = decode_cycle(&samples, Protocol::Ft8).expect("decode_cycle");
    let texts: Vec<&str> = got.iter().map(|m| m.text.as_str()).collect();
    println!("live_now: {} decodes", texts.len());
    assert!(
        texts.contains(&"VO1AE KA3KQO FM29"),
        "decode_cycle lost a full-cycle-only decode (C pass truncated at \
         WINDOW_SAMPLES?); got {} messages:\n{:?}",
        texts.len(),
        &texts
    );
}

// ---------------------------------------------------------------------------
// 3. Input length contract
// ---------------------------------------------------------------------------

#[test]
fn cycle_input_length_contract() {
    // Silence of exact cycle length: Ok with (probably) no decodes.
    let full: Vec<f32> = vec![0.0; 180_000];
    let got = decode_cycle(&full, Protocol::Ft8).expect("exact-length cycle decodes");
    assert!(got.is_empty());

    // Short input is zero-padded, not an error or panic.
    let short: Vec<f32> = vec![0.0; 60_000];
    let got = decode_cycle(&short, Protocol::Ft8).expect("short cycle zero-padded");
    assert!(got.is_empty());

    // Longer input is truncated to the first full cycle, no panic.
    let long: Vec<f32> = vec![0.0; 180_000 + 50_000];
    let got = decode_cycle(&long, Protocol::Ft8).expect("long cycle truncated");
    assert!(got.is_empty());

    // Empty input errors, never panics.
    let err = decode_cycle(&[], Protocol::Ft8);
    assert!(err.is_err());
}

// ---------------------------------------------------------------------------
// 4. FT4 round-trip + station-composition parity
// ---------------------------------------------------------------------------

/// The station coordinator runs the SAME two-pass composition for FT4 slots
/// as for FT8 (see `cycle.rs`'s composition comment): the C pass is
/// protocol-aware and the native decoder is rebuilt from a config whose
/// `protocol` is `Protocol::Ft4`. Synthesis helpers need the `transmit`
/// feature (`Ft8Encoder`/`Ft8Modulator`), exactly like the
/// `round_trip_tests.rs` precedent this mirrors.
#[cfg(feature = "transmit")]
#[test]
fn ft4_cycle_round_trips_and_matches_station_composition() {
    use pancetta_ft8::{Ft8Encoder, Ft8Modulator, ProtocolParams, PulseShape, BASE_FREQUENCY};

    fn encode_ft4(text: &str) -> Vec<u8> {
        let mut encoder = Ft8Encoder::with_protocol(ProtocolParams::ft4());
        encoder
            .encode_message_protocol(text, None)
            .unwrap_or_else(|e| panic!("Failed to FT4-encode '{}': {}", text, e))
    }

    fn modulate_ft4(symbols: &[u8], frequency_offset: f64) -> Vec<f32> {
        let params = ProtocolParams::ft4();
        let mut modulator = Ft8Modulator::with_pulse_shape(
            SAMPLE_RATE,
            BASE_FREQUENCY,
            0.5,
            PulseShape::Rectangular,
        )
        .unwrap();
        let mut audio = modulator
            .modulate_symbols_protocol(symbols, frequency_offset, &params)
            .unwrap();
        // One FT4 cycle: 7.5 s * 12 kHz.
        audio.resize(90_000, 0.0);
        audio
    }

    let text = "CQ W1ABC FN42";
    let audio = modulate_ft4(&encode_ft4(text), 0.0);
    let got = decode_cycle(&audio, Protocol::Ft4).expect("decode_cycle FT4");
    println!("ft4 synthetic cycle: {} decodes", got.len());
    assert!(
        got.iter().any(|m| m.text == text),
        "FT4 round-trip failed; got {:?}",
        got.iter().map(|m| m.text.clone()).collect::<Vec<_>>()
    );

    // Parity with the station composition on the same input.
    let want = station_composition_neutral(&audio, Protocol::Ft4);
    assert_parity("ft4-synthetic", &want, &got);
}

// ---------------------------------------------------------------------------
// 5. DT semantics: slot-boundary-relative
// ---------------------------------------------------------------------------

/// A signal placed at +1.0 s inside the cycle reports time_offset ≈ 1.0 s
/// (sample 0 of the cycle IS the slot boundary, so no lead subtraction is
/// needed). Synthesis needs the `transmit` feature.
#[cfg(feature = "transmit")]
#[test]
fn cycle_dt_is_slot_boundary_relative() {
    use pancetta_ft8::{Ft8Encoder, Ft8Modulator, NUM_SYMBOLS};

    fn encode_message(text: &str) -> [u8; NUM_SYMBOLS] {
        let mut encoder = Ft8Encoder::new();
        encoder
            .encode_message(text, None)
            .unwrap_or_else(|e| panic!("Failed to encode '{}': {}", text, e))
    }

    fn modulate_symbols(symbols: &[u8; NUM_SYMBOLS], frequency_offset: f64) -> Vec<f32> {
        let mut modulator = Ft8Modulator::new_default().unwrap();
        let mut audio = modulator
            .modulate_symbols(symbols, frequency_offset)
            .unwrap();
        audio.resize(WINDOW_SAMPLES, 0.0);
        audio
    }

    let dt_secs = 1.0_f64;
    let mut cycle: Vec<f32> = vec![0.0; 180_000];
    let signal = modulate_symbols(&encode_message("K1ABC W9XYZ EN37"), 0.0);
    let start = (dt_secs * SAMPLE_RATE as f64) as usize;
    for (i, s) in signal.iter().enumerate() {
        cycle[start + i] += s;
    }
    let got = decode_cycle(&cycle, Protocol::Ft8).expect("decode_cycle");
    assert!(
        !got.is_empty(),
        "expected at least one decode of the shifted signal"
    );
    let m = got
        .iter()
        .find(|m| m.text == "K1ABC W9XYZ EN37")
        .expect("shifted message decoded");
    assert!(
        (m.time_offset - dt_secs).abs() <= 0.15,
        "DT {:.3} not slot-boundary-relative (expected ≈{dt_secs})",
        m.time_offset
    );
}

// ---------------------------------------------------------------------------
// 6. CycleDecoder reuse across cycles
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 6b. Unresolved-hash twin suppression (the station's delivered-set pass)
// ---------------------------------------------------------------------------

/// PR #550 review round 1: a reused `CycleDecoder`'s native pass resolves a
/// hashed callsign its table learned on an earlier cycle, while the C pass
/// (no-op FFI hash callbacks — `ft8_lib_ffi.rs`) still renders `<...>`, so
/// the exact-text `merge_ft8lib_first` would deliver BOTH renderings of one
/// physical signal. The station removes exactly these duplicates with
/// `drop_unresolved_hash_twins` (`pancetta/src/coordinator/ft8.rs`), and
/// `decode_cycle` must apply the same pass. Recipe mirrors the decoder unit
/// test `decoder_seeds_hash_table_from_nonstdcall_exact_field_too` (needs
/// `transmit` for the synthesis helpers).
#[cfg(feature = "transmit")]
#[test]
fn cycle_suppresses_unresolved_hash_twins_on_reused_decoder() {
    use pancetta_ft8::{Ft8Encoder, Ft8Modulator};

    fn cycle_with(text: &str, freq_hz: f64) -> Vec<f32> {
        let mut encoder = Ft8Encoder::new();
        let symbols = encoder
            .encode_message(text, None)
            .unwrap_or_else(|e| panic!("encode '{text}': {e}"));
        let mut modulator = Ft8Modulator::new_default().expect("modulator");
        let signal = modulator
            .modulate_symbols(&symbols, freq_hz)
            .expect("modulate");
        let mut cycle: Vec<f32> = vec![0.0; 180_000];
        for (i, s) in signal.iter().enumerate() {
            cycle[i] += *s;
        }
        cycle
    }

    let mut cycler = CycleDecoder::new(Protocol::Ft8).expect("CycleDecoder ctor");

    // Cycle 1 teaches the native decoder's hash table: the compound call
    // lands in the type-4 exact 58-bit field in plaintext.
    let teach = cycle_with("CQ YS/WE9G", 500.0);
    let taught = cycler.decode_cycle(&teach).expect("teach cycle");
    assert!(
        taught.iter().any(|m| m.text.contains("YS/WE9G")),
        "teach cycle must decode the compound CQ: {:?}",
        taught.iter().map(|m| m.text.clone()).collect::<Vec<_>>()
    );

    // Cycle 2 hashes YS/WE9G into the 12-bit slot: the C pass renders
    // `PJ4/KA1ABC <...> RR73`, the native pass (learned table) renders
    // `PJ4/KA1ABC <YS/WE9G> RR73` — twins of one physical signal.
    let twin = cycle_with("PJ4/KA1ABC YS/WE9G RR73", 900.0);
    let got = cycler.decode_cycle(&twin).expect("twin cycle");
    let texts: Vec<&str> = got.iter().map(|m| m.text.as_str()).collect();
    println!("twin cycle decodes: {texts:?}");
    assert!(
        !texts.iter().any(|t| t.contains("<...>")),
        "decode_cycle delivered an unresolved-hash twin alongside its \
         resolved copy: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("<YS/WE9G>")),
        "the resolved rendering must survive the twin suppression: {texts:?}"
    );
}

#[test]
fn cycle_decoder_is_stateless_across_cycles() {
    let a = read_wav_file(&fixture("generated/ft8_cq.wav"));
    let b = read_wav_file(&fixture("generated/ft8_report.wav"));

    let mut cycler = CycleDecoder::new(Protocol::Ft8).expect("CycleDecoder ctor");
    let reuse_a1 = cycler.decode_cycle(&a).expect("reuse cycle a");
    let reuse_b = cycler.decode_cycle(&b).expect("reuse cycle b");
    let reuse_a2 = cycler.decode_cycle(&a).expect("reuse cycle a again");

    let fresh_a1 = decode_cycle(&a, Protocol::Ft8).expect("fresh cycle a");
    let fresh_b = decode_cycle(&b, Protocol::Ft8).expect("fresh cycle b");

    let texts = |v: &[DecodedMessage]| -> Vec<String> {
        v.iter().map(|m| m.text.clone()).collect::<Vec<_>>()
    };
    assert_eq!(texts(&reuse_a1), texts(&fresh_a1));
    assert_eq!(texts(&reuse_b), texts(&fresh_b));
    assert_eq!(texts(&reuse_a2), texts(&fresh_a1));
    for (r, f) in reuse_a2.iter().zip(fresh_a1.iter()) {
        assert_eq!(
            r.snr_db, f.snr_db,
            "SNR drift across cycles for '{}'",
            r.text
        );
        assert_eq!(
            r.time_offset, f.time_offset,
            "DT drift across cycles for '{}'",
            r.text
        );
    }
}
