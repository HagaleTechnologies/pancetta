//! Batch 28 / Diagnostic B — Audio-transform decode diversity
//!
//! Two hypotheses:
//!
//! * **hb-117 reconfirm at extreme gain (±48 dB)**: prior diagnostic
//!   shelved at ±12 dB with 0/861 novel TPs. Extreme rescaling pushes
//!   into f32 precision-limit territory. Confirm that decoder remains
//!   gain-invariant down to fundamental numerical limits.
//! * **hb-118 (USB/LSB IQ-pair diversity)**: synthesize an "LSB"
//!   variant by spectral mirroring around the baseband mid-point (3 kHz
//!   for FT8). Compare decoded TP sets. If the mirror reveals NEW TPs,
//!   USB+LSB diversity has surface area.
//!
//! Method: top-20 hard-200; for each WAV:
//!   - Decode at ±48 dB rescaled
//!   - Decode the spectral-mirrored variant
//!   - Truth-match each set; report novel TPs (TPs not in baseline 0 dB)
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch28_audio_transforms

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::f32::consts::PI;
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

fn load_truth(ws: &Path, sha: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return HashSet::new();
    };
    v["decodes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn decode_set(samples: &[f32], cfg: &Ft8Config) -> Result<HashSet<String>> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

fn rescaled(samples: &[f32], gain_linear: f32) -> Vec<f32> {
    samples.iter().map(|s| s * gain_linear).collect()
}

/// Spectral mirror around the baseband mid-point — modulates each
/// sample by `cos(2π * fs/2 * n / fs) = (-1)^n` which flips the spectrum
/// top-to-bottom (Nyquist mirror). This is the simplest "LSB-style"
/// transform on a real-valued signal at 12 kHz: 0–6 kHz content lands
/// at 6–12 kHz and vice versa.
fn spectral_mirror(samples: &[f32]) -> Vec<f32> {
    samples
        .iter()
        .enumerate()
        .map(|(i, &s)| if i % 2 == 0 { s } else { -s })
        .collect()
}

/// Shift baseband content by 3 kHz via cosine modulation — a softer
/// "LSB-like" perturbation that maps 0.5 kHz → 5.5 kHz, 2 kHz → 4 kHz,
/// etc. Real-valued mod produces both upper and lower sidebands; the
/// FT8 decoder only sees one (cropped by its own internal filtering).
#[allow(dead_code)]
fn cosine_shift_3khz(samples: &[f32]) -> Vec<f32> {
    let fs: f32 = 12_000.0;
    let f_shift: f32 = 3_000.0;
    samples
        .iter()
        .enumerate()
        .map(|(i, &s)| s * (2.0 * PI * f_shift * (i as f32) / fs).cos())
        .collect()
}

fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH28_TOP_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    println!("## Batch 28 / Diagnostic B — audio transforms (top-{top_n} hard-200)");

    let cfg = Ft8Config::default();
    let g_neg48 = db_to_linear(-48.0);
    let g_pos48 = db_to_linear(48.0);

    let mut hb117_extreme_novel_tp: Vec<usize> = Vec::new();
    let mut hb118_mirror_novel_tp: Vec<usize> = Vec::new();
    let mut wavs_processed: Vec<String> = Vec::new();

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let original = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

        let baseline_set = decode_set(&original, &cfg)?;
        let baseline_tp: HashSet<&String> = baseline_set.intersection(&truth).collect();

        // hb-117 extreme: ±48 dB
        let neg48 = rescaled(&original, g_neg48);
        let pos48 = rescaled(&original, g_pos48);
        let neg48_set = decode_set(&neg48, &cfg)?;
        let pos48_set = decode_set(&pos48, &cfg)?;
        let neg48_novel_tp = neg48_set
            .iter()
            .filter(|s| truth.contains(*s) && !baseline_tp.contains(s))
            .count();
        let pos48_novel_tp = pos48_set
            .iter()
            .filter(|s| truth.contains(*s) && !baseline_tp.contains(s))
            .count();
        hb117_extreme_novel_tp.push(neg48_novel_tp + pos48_novel_tp);

        // hb-118 spectral mirror
        let mirror = spectral_mirror(&original);
        let mirror_set = decode_set(&mirror, &cfg)?;
        let mirror_novel_tp = mirror_set
            .iter()
            .filter(|s| truth.contains(*s) && !baseline_tp.contains(s))
            .count();
        hb118_mirror_novel_tp.push(mirror_novel_tp);

        wavs_processed.push(sha[..8].to_string());
    }

    println!("\n### hb-117 reconfirm at extreme gain (±48 dB)");
    let total_117 = hb117_extreme_novel_tp.iter().sum::<usize>();
    let mean_117 = total_117 as f64 / hb117_extreme_novel_tp.len() as f64;
    println!(
        "  Total novel TPs at ±48 dB: {} across {} WAVs (mean {:.2}/WAV)",
        total_117,
        hb117_extreme_novel_tp.len(),
        mean_117
    );
    if total_117 == 0 {
        println!("  Verdict: SHELVE — gain invariance extends to ±48 dB (numerical-precision limit); confirms hb-117");
    } else {
        println!("  Verdict: SURPRISE — extreme rescaling reveals decode diversity not seen at ±12 dB; investigate");
    }

    println!("\n### hb-118 — USB/LSB diversity via spectral mirror");
    let total_118 = hb118_mirror_novel_tp.iter().sum::<usize>();
    let mean_118 = total_118 as f64 / hb118_mirror_novel_tp.len() as f64;
    println!(
        "  Total novel TPs from spectral mirror: {} across {} WAVs (mean {:.2}/WAV)",
        total_118,
        hb118_mirror_novel_tp.len(),
        mean_118
    );
    if total_118 == 0 {
        println!("  Verdict: SHELVE — spectral mirror reveals no new TPs; the LSB/USB path on the same RF audio has no decoder-side diversity");
    } else if mean_118 < 1.0 {
        println!("  Verdict: WEAK SHELVE — mean < 1.0/WAV; not a productive mining vein");
    } else {
        println!("  Verdict: PROCEED — spectral mirror surfaces real new TPs; investigate proper USB/LSB IQ tap");
    }

    Ok(())
}
