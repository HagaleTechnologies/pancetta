//! Cycle-level decode API tests (PAN-176).
//!
//! `decode_cycle` / `CycleDecoder` accept one full protocol cycle of 12 kHz
//! audio (FT8: 180 000 samples / 15 s, FT4: 90 000 samples / 7.5 s, sample 0
//! at the slot boundary) and must return exactly the composition the station
//! coordinator runs with neutral state:
//!
//! - C reference pass: `Ft8Decoder::decode_window_ft8lib_protocol`
//! - native AP pass: `decode_window_with_ap_scoped` with
//!   `ApContext::default()`, no frequency scope, unlimited budget
//! - merge: `merge_ft8lib_first`
//!
//! Parity contract: message texts and frequency offsets match exactly;
//! SNR within 1.0 dB and DT within 0.15 s of the hand-run composition
//! (the tolerances exist only because the live station window carries a
//! 0.5 s lead-in slice the cycle API does not).

use pancetta_ft8::merge::merge_ft8lib_first;
use pancetta_ft8::{
    decode_cycle, ApContext, CycleDecoder, DecodedMessage, Ft8Config, Ft8Decoder, Protocol,
    SAMPLE_RATE, WINDOW_SAMPLES,
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

/// The station composition run by hand on the same trimmed window the cycle
/// API uses, with neutral context. Mirrors `pancetta/src/coordinator/ft8.rs`'s
/// decode loop (C pass, native AP pass, `merge_ft8lib_first`) minus the
/// station-only QSO context, narrow filter and wall-clock budget (the plain
/// scoped entry point delegates with `DecodeBudget::unlimited()`).
fn station_composition_neutral(samples: &[f32], protocol: Protocol) -> Vec<DecodedMessage> {
    // Same window prep as decode_cycle: first WINDOW_SAMPLES samples,
    // zero-padded when short.
    let window: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
        samples[..WINDOW_SAMPLES].to_vec()
    } else {
        let mut padded = samples.to_vec();
        padded.resize(WINDOW_SAMPLES, 0.0);
        padded
    };

    let ft8lib = Ft8Decoder::decode_window_ft8lib_protocol(&window, protocol);
    let config = Ft8Config {
        protocol,
        ..Ft8Config::default()
    };
    let mut decoder = Ft8Decoder::new(config).expect("decoder ctor");
    let native = decoder
        .decode_window_with_ap_scoped(&window, &ApContext::default(), None)
        .expect("native decode");
    merge_ft8lib_first(ft8lib, native).messages
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
        assert!(
            (w.frequency_offset - g.frequency_offset).abs() < 0.5,
            "{label}: frequency mismatch for '{}' ({} vs {})",
            w.text,
            w.frequency_offset,
            g.frequency_offset
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
/// composition `decode_cycle` runs (C pass + neutral native pass + PAN-110
/// merge on the trimmed 151 680-sample window). The PAN-176 plan's Design
/// section quoted 26 messages, but that count came from
/// `wav_decode_tests::decode_wav_file`, which feeds the UNTRIMMED 180 000
/// samples to the native pass alone — one decode (`IE2JUO ZP1EUJ R HL33`,
/// a late-DT signal whose message span does not fit the trimmed window) is
/// absent here. Recorded in this container; see deviations.json.
const JTDX_190227_155815: [&str; 25] = [
    "UN7LZ RV3DQC -01",
    "CQ RD3FC KO95",
    "UA3AIU UR5GCK -03",
    "CQ UA3QNE LO01",
    "R9AA IK5EEA 73",
    "G3VAO UA3GDJ KO92",
    "EA7DGC IK8DYE 73",
    "YD1GNL UA8CW MO07",
    "YD1GNL UN7ZAI MO51",
    "R1BEQ UN7FU 73",
    "KA7RLM R7NW R-23",
    "CQ RA9CAD LO96",
    "CQ ES6DO KO27",
    "CQ A41ZZ LL93",
    "YB3BBF SV2HXX R-20",
    "EA1IPH UN7LAY -16",
    "LB8ZH R2FAQ KO04",
    "GD3YUM YB0BTA OI33",
    "YD1GNL R3KAB LN09",
    "GD3YUM BG4QNE PM01",
    "UA6LIK SP3AVS R+00",
    "CQ R9AAB MO04",
    "CQ RT4S LO36",
    "CQ YD0SCZ OI33",
    "EA3HRU R6LCM RR73",
];

#[test]
fn ft8_cycle_golden_sets() {
    // jtdx fixture: exact text list (order as returned).
    let samples = read_wav_file(&fixture("jtdx/190227_155815.wav"));
    let got = decode_cycle(&samples, Protocol::Ft8).expect("decode_cycle");
    let texts: Vec<&str> = got.iter().map(|m| m.text.as_str()).collect();
    println!("jtdx/190227_155815.wav golden set ({} messages):", texts.len());
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

    // Longer input is truncated to the first WINDOW_SAMPLES, no panic.
    let long: Vec<f32> = vec![0.0; WINDOW_SAMPLES + 50_000];
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
    assert!(!got.is_empty(), "expected at least one decode of the shifted signal");
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
        assert_eq!(r.snr_db, f.snr_db, "SNR drift across cycles for '{}'", r.text);
        assert_eq!(
            r.time_offset, f.time_offset,
            "DT drift across cycles for '{}'",
            r.text
        );
    }
}
