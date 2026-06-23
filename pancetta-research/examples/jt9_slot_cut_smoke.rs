//! jt9_slot_cut_smoke — verify the slot-cutting jt9 wrapper works on hard-200 WAVs.
//!
//! Picks the first 3 hard-200 WAVs (full multi-slot operator recordings) and
//! runs the Jt9Decoder with slot_cut enabled. Prints decode counts. Compares
//! against pancetta's decode count on the same WAVs (which slides its
//! spectrogram across the whole multi-slot WAV).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example jt9_slot_cut_smoke

use anyhow::Context;
use pancetta_research::{DecoderUnderTest, Ft8Decoder, Jt9Decoder};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf();
    let manifest_path = workspace.join("research/corpus/curated/ft8/hard_200.manifest.json");
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("no entries")?;

    let pancetta = Ft8Decoder::with_default_config();
    let jt9_cut = Jt9Decoder::default().with_slot_cut(true);
    let jt9_nocut = Jt9Decoder::default();

    println!("--- Hard-200 (multi-slot operator recordings; unaligned) ---");
    println!("WAV                          | panc | jt9 raw | jt9 slot-cut");
    for (i, entry) in entries.iter().take(3).enumerate() {
        let wav_path = entry
            .get("wav_path")
            .and_then(|p| p.as_str())
            .context("no path")?;
        let p = Path::new(wav_path);
        let pd = pancetta.decode_wav(p).map(|d| d.len()).unwrap_or(0);
        let jr = jt9_nocut.decode_wav(p).map(|d| d.len()).unwrap_or(0);
        let jc = jt9_cut.decode_wav(p).map(|d| d.len()).unwrap_or(0);
        println!(
            "{i}: {:<24} | {:>4} | {:>7} | {:>12}",
            wav_path.rsplit('/').next().unwrap_or("?"),
            pd,
            jr,
            jc
        );
    }

    println!();
    println!("--- Synth-clean (one slot per WAV; aligned) ---");
    println!("WAV                          | panc | jt9 raw | jt9 slot-cut");
    let synth = workspace.join("research/corpus/synth/manifests/clean.manifest.json");
    let sm: Value = serde_json::from_str(&std::fs::read_to_string(&synth)?)?;
    let se = sm.get("entries").and_then(|e| e.as_array()).unwrap();
    let mut tested = 0;
    for entry in se.iter() {
        let snr = entry.get("snr_db").and_then(|s| s.as_f64()).unwrap_or(0.0);
        if snr < -14.0 || snr > -10.0 {
            continue;
        }
        let wav_rel = entry
            .get("wav_path")
            .and_then(|p| p.as_str())
            .context("no path")?;
        let wav_abs = workspace.join(wav_rel);
        let p = Path::new(&wav_abs);
        let pd = pancetta.decode_wav(p).map(|d| d.len()).unwrap_or(0);
        let jr = jt9_nocut.decode_wav(p).map(|d| d.len()).unwrap_or(0);
        let jc = jt9_cut.decode_wav(p).map(|d| d.len()).unwrap_or(0);
        println!(
            "   {:<24}   | {:>4} | {:>7} | {:>12}",
            wav_rel.rsplit('/').next().unwrap_or("?"),
            pd,
            jr,
            jc
        );
        tested += 1;
        if tested >= 3 {
            break;
        }
    }
    Ok(())
}
