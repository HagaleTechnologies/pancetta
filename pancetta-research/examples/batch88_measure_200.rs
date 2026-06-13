//! Batch 88 — hb-249 before/after guard: default-config decode of the
//! first 200 raw_530_full slots, scored against ft8_lib truth with
//! hash-normalized matching (Batch 87 rule). Run once on the pre-fix
//! decoder and once on the post-fix decoder; the dt-convention fix must
//! be TP-neutral-or-positive and must not add FPs.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch88_measure_200

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_research::metrics::hash_normalize_message;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    anyhow::ensure!(spec.channels == 1 && spec.sample_rate == 12000);
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    // Optional: --dump <path> writes "slot\tTP|FP\ttext\tfreq\tdt" lines for
    // before/after decode-set diffing.
    let args: Vec<String> = std::env::args().collect();
    let mut dump: Option<std::fs::File> = args
        .iter()
        .position(|a| a == "--dump")
        .and_then(|i| args.get(i + 1))
        .map(std::fs::File::create)
        .transpose()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/raw_530_full.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .take(200)
        .cloned()
        .collect();

    let (mut slots, mut tot, mut tp, mut fp, mut truth_n, mut found) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let truth_path = ws
            .join("research/baselines/ft8")
            .join(format!("{sha}.ft8lib.json"));
        let Ok(txt) = std::fs::read_to_string(&truth_path) else {
            continue;
        };
        let v: Value = serde_json::from_str(&txt)?;
        let truth_norm: HashSet<String> = v["decodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["message"].as_str())
            .map(hash_normalize_message)
            .collect();
        let Ok(samples) = load_wav(Path::new(wav_path)) else {
            continue;
        };
        slots += 1;
        let mut decoder = Ft8Decoder::new(Ft8Config::default())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        tot += decoded.len();
        let dec_norm: HashSet<String> = decoded
            .iter()
            .map(|d| hash_normalize_message(&d.text))
            .collect();
        for d in &dec_norm {
            if truth_norm.contains(d) {
                tp += 1;
            } else {
                fp += 1;
            }
        }
        if let Some(f) = dump.as_mut() {
            use std::io::Write;
            for d in &decoded {
                let norm = hash_normalize_message(&d.text);
                let cls = if truth_norm.contains(&norm) {
                    "TP"
                } else {
                    "FP"
                };
                writeln!(
                    f,
                    "{wav_path}\t{cls}\t{}\t{:.1}\t{:.2}",
                    d.text, d.frequency_offset, d.time_offset
                )?;
            }
        }
        truth_n += truth_norm.len();
        found += truth_norm.intersection(&dec_norm).count();
        if (i + 1) % 50 == 0 {
            eprintln!("  [{}/{}]", i + 1, entries.len());
        }
    }
    println!(
        "slots={slots} decodes={tot} TP={tp} FP={fp} truth={truth_n} found={found} miss_rate={:.2}%",
        100.0 * (1.0 - found as f64 / truth_n.max(1) as f64)
    );
    Ok(())
}
