//! Batch 67 — hb-237 cross-sequence A7 re-measurement on real-world
//! QSO-continuous corpus.
//!
//! Batch 54 measured hb-237 on chrono_replay with the trust gate
//! engaged and saw 0 TPs from 796 cs_emits. Batch 65 surfaced that
//! the truth labels for chrono_replay came from pancetta-ft8
//! WITHOUT cross_seq A7, biasing the measurement by construction.
//!
//! This probe re-measures with:
//!   - `qso_continuous_530` curated corpus (250 adjacent slot pairs
//!     with shared callsigns, sampled from 5/30 raw recordings).
//!   - ft8_lib FFI truth labels (Batch 66 baseline files at
//!     research/baselines/ft8/<sha>.ft8lib.json).
//!   - The production-shape trust gate (CallsignContinuityFilter
//!     strict mode, slot-0 bootstrap from own decodes).
//!
//! Two configurations:
//!   1. cross_seq OFF (baseline)
//!   2. cross_seq ON with trust-gated seeds
//!
//! Compared to Batch 54: same architecture, different corpus + truth.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch67_hb237_real_corpus -- \
//!     --manifest research/corpus/curated/ft8/qso_continuous_530.manifest.json

use anyhow::{Context, Result};
use pancetta_ft8::{CrossSequenceSeed, DecodedMessage, Ft8Config, Ft8Decoder};
use pancetta_qso::callsign_continuity::CallsignContinuityFilter;
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
        .join(format!("{sha}.ft8lib.json"));
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

fn trust_gated_seeds(
    decodes: &[DecodedMessage],
    filter: &CallsignContinuityFilter,
) -> Vec<CrossSequenceSeed> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut seeds = Vec::new();
    for d in decodes {
        for call_opt in [&d.message.from_callsign, &d.message.to_callsign] {
            if let Some(call) = call_opt {
                let up = call.trim().to_uppercase();
                if up.is_empty() || up.starts_with("CQ") {
                    continue;
                }
                if !seen.insert(up.clone()) {
                    continue;
                }
                if filter.would_accept_callsign(&up) {
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

#[derive(Default, Debug)]
struct RunStats {
    total_decodes: usize,
    total_tps: usize,
    cs_emits: usize,
    cs_recovered_tps: usize,
    cs_emit_fps: usize,
    seeds_offered: usize,
    seeds_passed_trust: usize,
    elapsed_secs: f64,
}

fn run(entries: &[Value], cfg: &Ft8Config) -> Result<RunStats> {
    let ws = workspace_root()?;
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let mut filter = CallsignContinuityFilter::new(200);
    let mut prev_decodes: Vec<DecodedMessage> = Vec::new();
    let mut stats = RunStats::default();
    let t0 = std::time::Instant::now();

    for (idx, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = match load_wav(Path::new(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let truth = load_truth(&ws, sha);

        let seeds = if cfg.cross_sequence_a7_enabled && !prev_decodes.is_empty() {
            let mut offered = 0usize;
            for d in &prev_decodes {
                for c in [&d.message.from_callsign, &d.message.to_callsign] {
                    if c.is_some() {
                        offered += 1;
                    }
                }
            }
            stats.seeds_offered += offered;
            let s = trust_gated_seeds(&prev_decodes, &filter);
            stats.seeds_passed_trust += s.len();
            s
        } else {
            Vec::new()
        };

        let mut decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        if cfg.cross_sequence_a7_enabled && !seeds.is_empty() {
            let extras = decoder
                .try_cross_sequence_decodes(&samples, &seeds)
                .map_err(|e| anyhow::anyhow!("try_cross_sequence_decodes: {e}"))?;
            let already: HashSet<String> = decoded.iter().map(|d| d.text.clone()).collect();
            for e in extras {
                if !already.contains(&e.text) {
                    stats.cs_emits += 1;
                    if truth.contains(&e.text) {
                        stats.cs_recovered_tps += 1;
                    } else {
                        stats.cs_emit_fps += 1;
                    }
                    decoded.push(e);
                }
            }
        }

        stats.total_decodes += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                stats.total_tps += 1;
            }
        }

        if idx == 0 {
            let mut bootstrap = Vec::new();
            for d in &decoded {
                for c in [&d.message.from_callsign, &d.message.to_callsign] {
                    if let Some(cs) = c {
                        let up = cs.trim().to_uppercase();
                        if !up.is_empty() && !up.starts_with("CQ") {
                            bootstrap.push(up);
                        }
                    }
                }
            }
            filter.extend_from_iter(bootstrap);
        }
        for d in &decoded {
            let _ = filter.accept(&d.text);
        }

        prev_decodes = decoded;
        if (idx + 1) % 50 == 0 {
            eprintln!(
                "    [{}/{}] tps={} cs_emits={} cs_tp={} cs_fp={}",
                idx + 1,
                entries.len(),
                stats.total_tps,
                stats.cs_emits,
                stats.cs_recovered_tps,
                stats.cs_emit_fps,
            );
        }
    }
    stats.elapsed_secs = t0.elapsed().as_secs_f64();
    Ok(stats)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut manifest: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--manifest" {
            manifest = Some(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }
    let manifest = manifest.context("--manifest required")?;
    let ws = workspace_root()?;
    let manifest_path = if manifest.starts_with('/') {
        PathBuf::from(&manifest)
    } else {
        ws.join(&manifest)
    };
    let m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    let mut entries: Vec<Value> = m["entries"]
        .as_array()
        .context("manifest has no entries")?
        .iter()
        .cloned()
        .collect();
    // Sort by slot_index so pair-adjacent slots stay adjacent.
    entries.sort_by(|a, b| {
        a["slot_index"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&b["slot_index"].as_i64().unwrap_or(0))
    });
    println!("loaded manifest: {} entries", entries.len());

    let mk = |cs: bool| Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        cross_sequence_a7_enabled: cs,
        ..Ft8Config::default()
    };

    eprintln!("(1) cross_seq OFF (baseline)…");
    let s_off = run(&entries, &mk(false))?;
    println!(
        "\nbaseline (OFF): {} decodes / {} TPs ({:.1}s)",
        s_off.total_decodes, s_off.total_tps, s_off.elapsed_secs
    );

    eprintln!("(2) cross_seq ON (trust-gated)…");
    let s_on = run(&entries, &mk(true))?;
    let delta_tps = s_on.total_tps as i64 - s_off.total_tps as i64;
    let delta_decodes = s_on.total_decodes as i64 - s_off.total_decodes as i64;
    let delta_fps = (s_on.total_decodes as i64 - s_on.total_tps as i64)
        - (s_off.total_decodes as i64 - s_off.total_tps as i64);
    println!(
        "cross_seq ON: {} decodes / {} TPs / cs_emits={} (tp={} fp={}) ({:.1}s)",
        s_on.total_decodes,
        s_on.total_tps,
        s_on.cs_emits,
        s_on.cs_recovered_tps,
        s_on.cs_emit_fps,
        s_on.elapsed_secs
    );

    println!("\nΔ TPs: {delta_tps:+} | Δ FPs: {delta_fps:+} | Δ decodes: {delta_decodes:+}");
    println!(
        "Seeds: offered {} → passed trust {} ({:.1}%)",
        s_on.seeds_offered,
        s_on.seeds_passed_trust,
        100.0 * s_on.seeds_passed_trust as f64 / s_on.seeds_offered.max(1) as f64
    );

    let decision = if delta_tps >= 5 && delta_fps <= delta_tps {
        format!(
            "**hb-237 is NOT inert on real data**: Δ TPs = {delta_tps:+}, Δ FPs = {delta_fps:+}. Batch 54's truth-rigged measurement was wrong. Graduation territory."
        )
    } else if delta_tps > 0 && delta_fps > delta_tps * 3 {
        format!(
            "**Marginal real-world lift, FP-heavy**: Δ TPs = {delta_tps:+}, Δ FPs = {delta_fps:+}. Mechanism is alive but precision is the bottleneck."
        )
    } else if delta_tps == 0 {
        "**hb-237 still inert on real data with ft8_lib truth**: cross-sequence consumer produces emits, but ft8_lib doesn't recognize them as real signals either. May need pancetta-best-truth for a fair comparison.".to_string()
    } else {
        format!(
            "**hb-237 net-negative**: Δ TPs = {delta_tps:+}, Δ FPs = {delta_fps:+}. Stays default-OFF."
        )
    };
    println!("\n## Decision\n\n{decision}\n");

    let notes_path = ws.join("research/notes/2026-06-09-batch67-hb237-real.md");
    let body = format!(
        "# Batch 67 — hb-237 trust-gated on real QSO-continuous corpus\n\n\
         Manifest: {manifest}, {} entries.\n\
         Truth: ft8_lib FFI.\n\n\
         | Config | Decodes | TPs | FPs |\n|---|---:|---:|---:|\n\
         | baseline (OFF) | {} | {} | {} |\n\
         | cross_seq ON (trust-gated) | {} | {} | {} |\n\n\
         Δ TPs: {delta_tps:+} | Δ FPs: {delta_fps:+}\n\n\
         Cross-sequence telemetry: cs_emits={}, of which TPs={}, FPs={}.\n\
         Seeds: offered={}, passed_trust={}.\n\n\
         {decision}\n",
        entries.len(),
        s_off.total_decodes,
        s_off.total_tps,
        s_off.total_decodes - s_off.total_tps,
        s_on.total_decodes,
        s_on.total_tps,
        s_on.total_decodes - s_on.total_tps,
        s_on.cs_emits,
        s_on.cs_recovered_tps,
        s_on.cs_emit_fps,
        s_on.seeds_offered,
        s_on.seeds_passed_trust,
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
