use pancetta_ft8::{DecodeBudget, Ft8Config, Ft8Decoder, WINDOW_SAMPLES};
#[cfg(not(debug_assertions))]
use pancetta_ft8::{Ft8Encoder, Ft8Modulator, ProtocolParams, SAMPLE_RATE};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fixtures() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wav");
    let mut paths = Vec::new();
    for group in ["wsjt", "jtdx", "basicft8", "generated"] {
        for entry in std::fs::read_dir(root.join(group)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "wav") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
}

fn audio(path: &Path) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap();
    let mut samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|sample| sample.unwrap() as f32 / 32768.0)
        .collect();
    samples.resize(WINDOW_SAMPLES, 0.0);
    samples
}

fn max_decoder() -> Ft8Decoder {
    Ft8Decoder::new(Ft8Config {
        max_decode_passes: 2,
        time_varying_subtraction_enabled: true,
        ..Ft8Config::default()
    })
    .unwrap()
}

fn keys(path: &Path, budgeted: bool) -> BTreeSet<String> {
    let mut decoder = max_decoder();
    let budget = if budgeted {
        DecodeBudget::until(Instant::now() + Duration::from_millis(2000))
    } else {
        DecodeBudget::unlimited()
    };
    let (messages, _) = decoder
        .decode_window_budgeted(&audio(path), budget)
        .unwrap();
    messages
        .iter()
        .map(|m| format!("{}|{:.1}|{:.3}", m.text, m.frequency_offset, m.time_offset))
        .collect()
}

#[test]
fn baseline_decode_sets_are_supersets_post_change() {
    let baseline_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/subtraction_recall_baseline.json");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut actual = BTreeMap::new();
    for path in fixtures() {
        let name = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        actual.insert(format!("{name}:unlimited"), keys(&path, false));
        #[cfg(not(debug_assertions))]
        actual.insert(format!("{name}:budgeted"), keys(&path, true));
    }
    if std::env::var_os("PANCETTA_BLESS_SUBTRACTION_BASELINE").is_some() {
        std::fs::write(&baseline_path, serde_json::to_vec_pretty(&actual).unwrap()).unwrap();
        return;
    }
    let baseline: BTreeMap<String, BTreeSet<String>> =
        serde_json::from_slice(&std::fs::read(&baseline_path).unwrap()).unwrap();
    for name in actual.keys() {
        assert!(
            baseline.contains_key(name),
            "fixture missing from baseline: {name}"
        );
    }
    for (name, got) in actual {
        let expected = baseline.get(&name).expect("fixture missing from baseline");
        let missing: Vec<_> = expected.difference(&got).collect();
        assert!(missing.is_empty(), "{name} lost {missing:?}");
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn max_shape_busy_window_skips_fewer_than_ten_percent() {
    let params = ProtocolParams::ft8();
    let mut audio = vec![0.0_f32; WINDOW_SAMPLES];
    for i in 0..20 {
        let call = format!("{}{}ABC", if i < 10 { 'K' } else { 'N' }, i % 10);
        let symbols = Ft8Encoder::new().encode_cq(&call, "FN42", false).unwrap();
        let mut modulator = Ft8Modulator::new(SAMPLE_RATE, 300.0 + i as f64 * 40.0, 1.0).unwrap();
        let signal = modulator
            .modulate_symbols_protocol(&symbols, 0.0, &params)
            .unwrap();
        for (a, b) in audio.iter_mut().zip(signal) {
            *a += 0.12 * b;
        }
    }
    let mut decoder = max_decoder();
    let (_, report) = decoder
        .decode_window_budgeted(
            &audio,
            DecodeBudget::until(Instant::now() + Duration::from_millis(2000)),
        )
        .unwrap();
    let stage = report
        .stages
        .iter()
        .find(|row| row.0 == "multipass-subtract")
        .expect("subtraction stage");
    eprintln!("PAN-370 busy subtraction stage: {stage:?}");
    assert!(stage.3 <= 1 && stage.2 + stage.3 >= 18, "stage={stage:?}");
}
