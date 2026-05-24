//! jt9_smoke — quick end-to-end test of the Jt9Decoder subprocess wrapper.
//!
//! Picks the first 3 synth-clean WAVs (slot-length, decodable by jt9 directly)
//! and decodes each with both pancetta and jt9. Prints the decode counts
//! side-by-side. Confirms the wrapper parses jt9's output correctly.
//!
//! Note on slot-cutting: jt9 expects exactly one 15s slot per invocation.
//! The synth WAVs are slot-length already. The hard-200/1000 curated WAVs
//! are full multi-slot recordings (pancetta-ft8 handles those internally
//! via spectrogram sliding-window); running jt9 on them produces 0 decodes
//! unless they're slot-cut first. For FP-filter training (hb-024 follow-up),
//! prefer the existing pre-baselined jt9 truth in research/baselines/ft8/.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example jt9_smoke

use anyhow::Context;
use pancetta_research::{DecoderUnderTest, Ft8Decoder, Jt9Decoder};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf();
    let manifest_path = workspace.join("research/corpus/synth/manifests/clean.manifest.json");
    let manifest_str = std::fs::read_to_string(&manifest_path)?;
    let manifest: Value = serde_json::from_str(&manifest_str)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("manifest missing entries")?;

    let pancetta = Ft8Decoder::with_default_config();
    let jt9 = Jt9Decoder::default();

    println!("WAV path                                                |  panc | jt9 | encoded");
    println!("--------------------------------------------------------+-------+-----+--------");
    let mut tested = 0;
    for entry in entries.iter() {
        let wav_rel = entry
            .get("wav_path")
            .and_then(|p| p.as_str())
            .context("entry missing wav_path")?;
        let snr = entry.get("snr_db").and_then(|s| s.as_f64()).unwrap_or(0.0);
        if !(-18.0..=-10.0).contains(&snr) {
            continue; // pick mid-SNR cases where both should succeed
        }
        let wav_abs = workspace.join(wav_rel);
        let p = Path::new(&wav_abs);
        if !p.exists() {
            continue;
        }
        let encoded = entry
            .get("encoded_message")
            .and_then(|s| s.as_str())
            .unwrap_or("?");
        let pd = pancetta.decode_wav(p)?;
        let jd = jt9.decode_wav(p)?;
        let wav_short = wav_rel.rsplit('/').next().unwrap_or(wav_rel);
        println!(
            "{:<55} | {:>5} | {:>3} | {}",
            wav_short,
            pd.len(),
            jd.len(),
            encoded
        );
        for d in jd.iter().take(2) {
            println!(
                "    jt9-decoded: freq={:.0}Hz dt={:.2}s snr={:.0}dB  msg={:?}",
                d.freq_hz, d.dt_s, d.snr_db, d.message
            );
        }
        tested += 1;
        if tested >= 5 {
            break;
        }
    }
    Ok(())
}
