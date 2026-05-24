//! doppler_smoke — quick check of pancetta + jt9 on the new Doppler corpus
use anyhow::Context;
use pancetta_research::{DecoderUnderTest, Ft8Decoder, Jt9Decoder};
use serde_json::Value;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf();
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        workspace.join("research/corpus/synth/manifests/doppler.manifest.json"),
    )?)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("no entries")?;
    let p = Ft8Decoder::with_default_config();
    let j = Jt9Decoder::default();
    println!("snr  drift  | panc | jt9 | encoded");
    println!("------------+------+-----+--------");
    for entry in entries.iter() {
        let snr = entry.get("snr_db").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let drift = entry
            .get("drift_hz_per_sec")
            .and_then(|s| s.as_f64())
            .unwrap_or(0.0);
        let msg = entry
            .get("encoded_message")
            .and_then(|s| s.as_str())
            .unwrap_or("?");
        if msg != "CQ K1ABC FN42" {
            continue;
        } // just one message per (snr, drift)
        let wav = workspace.join(
            entry
                .get("wav_path")
                .and_then(|p| p.as_str())
                .context("no path")?,
        );
        let pd = p.decode_wav(&wav).map(|d| d.len()).unwrap_or(999);
        let jd = j.decode_wav(&wav).map(|d| d.len()).unwrap_or(999);
        println!("{snr:>4.1} {drift:>+5.1}  |  {pd:>3} | {jd:>3} | {msg}");
    }
    Ok(())
}
