//! Batch 53 — measure Batch 52 mechanisms (auto_passband + cross-sequence A7)
//!
//! Two probes:
//!
//! **Probe A — hard-200** (independent slots, no QSO sequencing):
//!   Matrix of 4 configs at `max_decode_passes = 2, ldpc_iterations = 200`:
//!   1. baseline (both OFF)            — reference 5301 TPs
//!   2. auto_passband ON                — measures auto-passband alone
//!   3. cross_sequence_a7 ON (empty seeds at each slot) — defense-in-depth
//!      smoke test; should be ~0 delta since hard-200 has no prior-slot
//!      context to seed from. Demonstrates the OFF-by-empty-seeds guard.
//!   4. both ON                          — sanity stack
//!
//! **Probe B — chrono_replay_mini33** (33 sequenced slots, 15s apart):
//!   Walks slots in chronological order. For each slot:
//!     - decode normally
//!     - if cross_seq enabled: harvest `(from_callsign, frequency_offset)`
//!       from prior slot's decodes as seeds, call `try_cross_sequence_decodes`
//!     - record seeds (callsigns + freqs) from this slot's decodes for the
//!       next slot to consume
//!   2 configs: cross_seq OFF (baseline) vs ON. Reports total unique TPs
//!   across all slots and the count of cross-sequence-provenance recoveries.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch53_b52_measure

use anyhow::{Context, Result};
use pancetta_ft8::{CrossSequenceSeed, DecodedMessage, Ft8Config, Ft8Decoder};
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
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

fn run_hard200(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, f64)> {
    let ws = workspace_root()?;
    let mut total = 0usize;
    let mut tps = 0usize;
    let t0 = std::time::Instant::now();
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                tps += 1;
            }
        }
    }
    Ok((total, tps, t0.elapsed().as_secs_f64()))
}

/// Build cross-sequence seeds from a slot's decoded messages. Harvests
/// `from_callsign` (and `to_callsign` when present) along with each
/// message's `frequency_offset`. Dedupes by uppercased callsign.
fn seeds_from_decodes(decodes: &[DecodedMessage]) -> Vec<CrossSequenceSeed> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut seeds = Vec::new();
    for d in decodes {
        for call_opt in [&d.message.from_callsign, &d.message.to_callsign] {
            if let Some(call) = call_opt {
                let up = call.trim().to_uppercase();
                if up.is_empty() || up.starts_with("CQ") {
                    continue;
                }
                if seen.insert(up.clone()) {
                    seeds.push(CrossSequenceSeed {
                        callsign: up,
                        partner_callsign: None,
                        freq_hz: d.frequency_offset,
                    });
                }
            }
        }
    }
    seeds
}

/// Walk chrono slots in order. When `cross_seq` is true, after each
/// standard decode, invoke `try_cross_sequence_decodes` with seeds
/// harvested from the prior slot. Returns (total_decodes, total_tps,
/// cross_seq_emit_count, wall_seconds).
fn run_chrono(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, usize, f64)> {
    let ws = workspace_root()?;
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let mut prev_seeds: Vec<CrossSequenceSeed> = Vec::new();
    let mut total = 0usize;
    let mut tps = 0usize;
    let mut cs_emits = 0usize;
    let t0 = std::time::Instant::now();
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

        let mut decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        if cfg.cross_sequence_a7_enabled && !prev_seeds.is_empty() {
            let extras = decoder
                .try_cross_sequence_decodes(&samples, &prev_seeds)
                .map_err(|e| anyhow::anyhow!("try_cross_sequence_decodes: {e}"))?;
            let already: HashSet<String> = decoded.iter().map(|d| d.text.clone()).collect();
            for e in extras {
                if !already.contains(&e.text) {
                    cs_emits += 1;
                    decoded.push(e);
                }
            }
        }

        total += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                tps += 1;
            }
        }

        // Update seeds for the next slot from THIS slot's decodes.
        prev_seeds = seeds_from_decodes(&decoded);
    }
    Ok((total, tps, cs_emits, t0.elapsed().as_secs_f64()))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;

    // ---- Probe A: hard-200 ----
    let manifest_a: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries_a: Vec<Value> = manifest_a["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    println!("loaded hard-200: {} entries", entries_a.len());

    let mk = |auto_pb: bool, cseq: bool| Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        auto_passband_enabled: auto_pb,
        cross_sequence_a7_enabled: cseq,
        ..Ft8Config::default()
    };

    println!("\n## Probe A — hard-200 (independent slots)\n");
    let cfgs_a = [
        ("baseline (both OFF)", mk(false, false)),
        ("auto_passband ON", mk(true, false)),
        ("cross_seq ON (empty seeds)", mk(false, true)),
        ("both ON", mk(true, true)),
    ];
    let mut results_a: Vec<(String, usize, usize, f64)> = Vec::new();
    for (label, cfg) in &cfgs_a {
        eprintln!("  running: {label}…");
        let (tot, tps, secs) = run_hard200(&entries_a, cfg)?;
        println!("  {label}: {tot} decodes / {tps} TPs ({secs:.1}s)");
        results_a.push((label.to_string(), tot, tps, secs));
    }

    // ---- Probe B: chrono_replay_mini33 ----
    let manifest_b: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/chrono_replay_mini33.manifest.json"),
    )?)?;
    let mut entries_b: Vec<Value> = manifest_b["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    entries_b.sort_by(|a, b| {
        a["slot_index"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&b["slot_index"].as_i64().unwrap_or(0))
    });
    println!(
        "\nloaded chrono_replay_mini33: {} sequenced entries",
        entries_b.len()
    );

    println!("\n## Probe B — chrono_replay_mini33 (end-to-end seed accumulation)\n");
    let cfgs_b = [
        ("cross_seq OFF (baseline)", mk(false, false)),
        ("cross_seq ON", mk(false, true)),
    ];
    let mut results_b: Vec<(String, usize, usize, usize, f64)> = Vec::new();
    for (label, cfg) in &cfgs_b {
        eprintln!("  running chrono: {label}…");
        let (tot, tps, cs, secs) = run_chrono(&entries_b, cfg)?;
        println!("  {label}: {tot} decodes / {tps} TPs / cs_emits={cs} ({secs:.1}s)");
        results_b.push((label.to_string(), tot, tps, cs, secs));
    }

    // ---- Write notes ----
    let notes_path = ws.join("research/notes/2026-06-09-batch53-b52-measurement.md");
    let mut body = String::new();
    body.push_str("# Batch 53 — Batch 52 mechanisms measurement\n\n");
    body.push_str("Auto_passband + cross-sequence A7 (hb-237 Session 3) measured on hard-200 and chrono_replay_mini33.\n\n");
    body.push_str(
        "Base config: `max_decode_passes = 2, ldpc_iterations = 200`. Reference baseline TPs on hard-200 from prior batches: **5301**.\n\n",
    );

    body.push_str("## Probe A — hard-200 (200 independent slots)\n\n");
    body.push_str("| Config | Decodes | TPs | Δ vs baseline | Precision | Elapsed |\n");
    body.push_str("|---|---:|---:|---:|---:|---:|\n");
    let base_tps_a = results_a[0].2 as i64;
    for (label, tot, tps, secs) in &results_a {
        let delta = *tps as i64 - base_tps_a;
        let prec = *tps as f64 / (*tot).max(1) as f64;
        body.push_str(&format!(
            "| {label} | {tot} | {tps} | {delta:+} | {prec:.4} | {secs:.1}s |\n"
        ));
    }

    body.push_str("\n## Probe B — chrono_replay_mini33 (33 sequenced slots)\n\n");
    body.push_str(
        "Slots walked in `slot_index` order. With cross_seq ON, seeds for slot N+1 are harvested from slot N's `from_callsign`/`to_callsign` (uppercased, dedup). `cs_emits` counts decodes EMITTED by `try_cross_sequence_decodes` that the standard pipeline didn't already produce.\n\n",
    );
    body.push_str("| Config | Decodes | TPs | cs_emits | Δ TPs | Precision | Elapsed |\n");
    body.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
    let base_tps_b = results_b[0].2 as i64;
    for (label, tot, tps, cs, secs) in &results_b {
        let delta = *tps as i64 - base_tps_b;
        let prec = *tps as f64 / (*tot).max(1) as f64;
        body.push_str(&format!(
            "| {label} | {tot} | {tps} | {cs} | {delta:+} | {prec:.4} | {secs:.1}s |\n"
        ));
    }

    body.push_str("\n## Interpretation guide\n\n");
    body.push_str("- Probe A row 3 (cross_seq ON, no seeds): the consumer's defense-in-depth empty-seed early-return should make this byte-identical to the baseline. Any Δ ≠ 0 indicates the pipeline isn't actually byte-identical even when the consumer no-ops — investigate.\n");
    body.push_str("- Probe A row 2 (auto_passband ON): isolates the auto-passband effect. The mechanism rebuilds the per-slot passband to the noise-floor 95th percentile; on hard-200 most slots are noise-floor-bounded already so the lift is expected to be small.\n");
    body.push_str("- Probe B Δ TPs measures the **end-to-end** value of cross-sequence A7: extra TPs recovered from prior-slot callsign seeds. The `cs_emits` column measures *attempts*; not all of those will be in truth.\n");
    std::fs::write(&notes_path, body)?;
    println!("\nDone. Results in {}", notes_path.display());
    Ok(())
}
