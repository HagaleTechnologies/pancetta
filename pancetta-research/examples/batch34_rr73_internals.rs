//! Batch 34 / Phase 1C — dump internal `Ft8Message` debug for RR73-truth-
//! position decodes on one missed WAV.
//!
//! For sha=17c4b25a (WB3FME KB9AVX RR73 truth missed), decode and print
//! the full `Debug` output of every `Ft8Message` whose freq is within
//! ±15 Hz of an RR73 truth. Reveals message_type, standard_type,
//! contest_exchange, and other fields that to_string() consults.
//!
//! Confirms whether pancetta is parsing the codeword as:
//!   (a) Standard / Reply with no token (igrid4=32401 path), or
//!   (b) NonStdCall with nrpt=0, or
//!   (c) something else.
//!
//! Once the parse-path is known, the fix surface is bounded.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch34_rr73_internals

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
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
    let target_sha: String =
        std::env::var("BATCH34_SHA").unwrap_or_else(|_| "17c4b25a".to_string());

    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let entry = entries
        .iter()
        .find(|e| {
            e["wav_sha256"]
                .as_str()
                .unwrap_or("")
                .starts_with(&target_sha)
        })
        .context("sha not in manifest")?;
    let wav_path = entry["wav_path"].as_str().context("wav_path")?;
    let full_sha = entry["wav_sha256"].as_str().context("sha")?;

    println!("## Batch 34 / Phase 1C — internal Ft8Message dump");
    println!("  sha: {}", full_sha);

    // Pull RR73 truth positions from baseline.
    let bpath = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", full_sha));
    let bv: Value = serde_json::from_str(&std::fs::read_to_string(&bpath)?)?;
    let truths = bv["decodes"].as_array().context("decodes")?;
    let rr73_positions: Vec<(String, f64, f64, f64)> = truths
        .iter()
        .filter_map(|t| {
            let text = t["message"].as_str()?;
            if !text.ends_with(" RR73") {
                return None;
            }
            Some((
                text.to_string(),
                t["freq_hz"].as_f64()?,
                t["dt_s"].as_f64()?,
                t["snr_db"].as_f64()?,
            ))
        })
        .collect();
    println!("  RR73 truths on this WAV: {}", rr73_positions.len());
    for (text, freq, dt, snr) in &rr73_positions {
        println!(
            "    truth: {} @ {} Hz, {} s, {:+.0} dB",
            text, freq, dt, snr
        );
    }

    let samples = load_wav(Path::new(wav_path))?;
    let cfg = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(cfg).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(&samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

    println!("\n### pancetta decodes near RR73 truth positions (±15 Hz)");
    for (truth_text, truth_freq, _truth_dt, _truth_snr) in &rr73_positions {
        println!("\n  ----- truth: {} @ {} Hz -----", truth_text, truth_freq);
        for d in &decoded {
            if (d.frequency_offset - truth_freq).abs() > 15.0 {
                continue;
            }
            println!("    text:            {:?}", d.text);
            println!("    freq:            {}", d.frequency_offset);
            println!("    dt:              {}", d.time_offset);
            println!("    snr:             {}", d.snr_db);
            println!("    message_type:    {:?}", d.message.message_type);
            println!("    standard_type:   {:?}", d.message.standard_type);
            println!("    to_callsign:     {:?}", d.message.to_callsign);
            println!("    from_callsign:   {:?}", d.message.from_callsign);
            println!("    grid_square:     {:?}", d.message.grid_square);
            println!("    signal_report:   {:?}", d.message.signal_report);
            println!("    contest_exchange:{:?}", d.message.contest_exchange);
            println!("    special_op:      {:?}", d.message.special_operation);
            // Print bits 70-77 of payload (igrid4 LSB + i3 region)
            let pl = &d.message.payload_bits;
            if pl.len() >= 77 {
                let bits59_77: String = (59..77).map(|i| if pl[i] { '1' } else { '0' }).collect();
                println!("    payload[59..77]: {}", bits59_77);
                // igrid4 is bits 59..74 in Standard parse (15 bits)
                let mut igrid4 = 0u16;
                for i in 59..74 {
                    igrid4 = (igrid4 << 1) | (if pl[i] { 1 } else { 0 });
                }
                println!("    igrid4 (Std):    {}", igrid4);
                // i3 is bits 74..77 in payload
                let mut i3 = 0u8;
                for i in 74..77 {
                    i3 = (i3 << 1) | (if pl[i] { 1 } else { 0 });
                }
                println!("    i3:              {}", i3);
            }
            println!();
        }
    }

    Ok(())
}
