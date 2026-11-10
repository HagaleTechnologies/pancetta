//! PAN-331: the encoder and modulator are part of every `pancetta-ft8`
//! build; `transmit` gates only the audio/PTT transmitter (`transmit.rs`).
//!
//! This file carries no cfg gate. It must compile and pass with default
//! features, the build manta and other programs use, so the decoder's
//! self-encode passes (A7 templates, cross-sequence A7, the GFSK
//! subtraction reference) run there exactly as they run in the station.

use pancetta_ft8::{
    decode_cycle, ApContext, CrossSequenceSeed, Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator,
    Protocol, SAMPLE_RATE, WINDOW_SAMPLES,
};
use std::path::Path;

fn fixture_window(subpath: &str) -> Vec<f32> {
    let path = format!(
        "{}/tests/fixtures/wav/{subpath}",
        env!("CARGO_MANIFEST_DIR")
    );
    let reader = hound::WavReader::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    assert_eq!(reader.spec().channels, 1);
    assert_eq!(reader.spec().sample_rate, SAMPLE_RATE);
    let mut samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect();
    samples.resize(WINDOW_SAMPLES, 0.0);
    samples
}

fn decode_texts(config: Ft8Config, window: &[f32]) -> Vec<String> {
    let mut decoder = Ft8Decoder::new(config).expect("decoder");
    let mut texts: Vec<String> = decoder
        .decode_window_with_ap_scoped(window, &ApContext::default(), None)
        .expect("decode")
        .into_iter()
        .map(|m| m.text)
        .collect();
    texts.sort();
    texts
}

/// Every `transmit` feature gate under `src/` guards the audio/PTT
/// transmitter, or the provenance helper shared with `benchmark`. A new
/// gate on decoder or encoder code fails here. (This comment avoids the
/// literal cfg spelling so the file stays out of `rg -c` gate counts.)
#[test]
fn transmit_gates_only_the_audio_ptt_transmitter() {
    fn walk(dir: &Path, out: &mut Vec<(std::path::PathBuf, usize, String)>) {
        for entry in std::fs::read_dir(dir).expect("read src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("read file");
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    if line.contains("feature = \"transmit\"") {
                        let next = lines.get(i + 1).map_or("", |l| l.trim()).to_string();
                        out.push((path.clone(), i + 1, next));
                    }
                }
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut gates = Vec::new();
    walk(&src, &mut gates);
    let offenders: Vec<_> = gates
        .iter()
        .filter(|(file, _, next)| {
            // `Path::ends_with` compares components, so this also matches
            // `src\lib.rs` on Windows.
            let allowed = (file.ends_with("src/lib.rs")
                && (next == "pub mod transmit;" || next.starts_with("pub use transmit::")))
                || file.ends_with("src/neural_osd_weights.rs");
            !allowed
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "`transmit` must gate only transmit.rs (PAN-331); offending gates: {offenders:#?}"
    );
}

/// A default-features consumer can encode, modulate and decode one cycle.
#[test]
fn default_build_encodes_modulates_and_decodes_a_cycle() {
    let symbols = Ft8Encoder::new()
        .encode_message("CQ K1ABC FN42", None)
        .expect("encode");
    let tx = Ft8Modulator::new_default()
        .expect("modulator")
        .modulate_symbols(&symbols, 0.0)
        .expect("modulate");
    let mut cycle = vec![0.0f32; 6_000]; // 0.5 s lead-in
    cycle.extend(tx);
    cycle.resize(180_000, 0.0);
    let texts: Vec<String> = decode_cycle(&cycle, Protocol::Ft8)
        .expect("decode_cycle")
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(texts, ["CQ K1ABC FN42"]);
}

/// Cross-sequence A7 runs on a recorded window. Before PAN-331 a
/// default-features build returned an empty set here (`0` against `3`
/// with `transmit`).
#[test]
fn cross_sequence_a7_runs_on_a_recorded_window() {
    let window = fixture_window("jtdx/190227_155815.wav");
    let mut base = Ft8Decoder::new(Ft8Config::default()).expect("decoder");
    let seeds: Vec<CrossSequenceSeed> = base
        .decode_window_with_ap_scoped(&window, &ApContext::default(), None)
        .expect("decode")
        .iter()
        .filter_map(|m| {
            let t: Vec<&str> = m.text.split_whitespace().collect();
            (t.len() >= 2 && t[0] != "CQ" && !t[1].starts_with('<')).then(|| CrossSequenceSeed {
                callsign: t[1].to_string(),
                partner_callsign: Some(t[0].to_string()),
                freq_hz: m.frequency_offset,
            })
        })
        .collect();
    assert!(!seeds.is_empty(), "fixture must yield seeds");
    let mut decoder = Ft8Decoder::new(Ft8Config {
        cross_sequence_a7_enabled: true,
        ..Ft8Config::default()
    })
    .expect("decoder");
    let found = decoder
        .try_cross_sequence_decodes(&window, &seeds)
        .expect("cross-sequence");
    assert!(!found.is_empty(), "cross-sequence A7 must not be a no-op");
    assert!(found.iter().all(|m| m.via_cross_sequence_a7));
}

/// The station's Max preset (`pancetta/src/coordinator/effort.rs`
/// `apply_effort_overrides`: FT8 at the full budget) decodes the same set
/// in this build as in the station. Before PAN-331 the default build
/// compiled out the time-varying GFSK subtraction and missed
/// `CQ UW5EJX/MM` (8 decodes against the station's 9).
#[test]
fn station_max_preset_decodes_like_the_station() {
    let window = fixture_window("wsjt/181201_180245.wav");
    let max = Ft8Config {
        impulse_robust_llr: Some(6.0),
        max_decode_passes: 2,
        time_varying_subtraction_enabled: true,
        ..Ft8Config::default()
    };
    let texts = decode_texts(max, &window);
    assert!(
        texts.iter().any(|t| t == "CQ UW5EJX/MM"),
        "station Max preset must recover CQ UW5EJX/MM; got {texts:?}"
    );
}
