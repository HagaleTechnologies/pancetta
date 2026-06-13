//! Batch 54 — hb-237 cross-sequence A7 measurement with the production
//! trust gate engaged.
//!
//! Batch 53 measured the bare consumer on chrono_replay_mini33 (33 slots,
//! no trust filter) and saw 87 cs_emits / 0 TPs — pure FP amplification.
//! The hb-237 spec §FP-risk warned about exactly this without the trust
//! gate. This probe runs the production-shape path:
//!
//!   - `CallsignContinuityFilter` strict mode (rolling_cap = 200)
//!   - Slot 0 bootstraps the static_ref with its own decoded callsigns
//!     (simulates a non-empty ADIF / cqdx feed at session start; without
//!     this every slot's seeds would fail the trust gate cold)
//!   - For each subsequent slot:
//!       * harvest cross-sequence seeds from PRIOR slot's accepted
//!         decodes filtered through `filter.would_accept_callsign`
//!       * decode normally
//!       * if seeds non-empty, invoke `try_cross_sequence_decodes`
//!       * push this slot's accepted decodes into the rolling window
//!         (via `filter.accept(text)`)
//!
//! Corpus: chrono_replay full (300 sequenced slots).
//!
//! Two configs at `max_decode_passes = 2, ldpc_iterations = 200`:
//!   1. baseline (cross_seq OFF)
//!   2. cross_seq ON with trust-gated seeds (production shape)
//!
//! Reports total decodes, total TPs (matched against per-slot truth), and
//! per-config cs_emits + cs_recovered_TPs. The key question:
//!
//!   With the trust gate engaged, does cross_seq lift TPs (recover real
//!   decodes the standard pipeline missed) without amplifying FPs?
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch54_hb237_trust_gated

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

/// Build trust-filtered seeds from prior-slot decodes. Each
/// `from_callsign` / `to_callsign` is uppercased, deduped, and pushed
/// only if `filter.would_accept_callsign` returns true. Skips CQ tokens.
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
    seeds_offered_total: usize,
    seeds_passed_trust_total: usize,
    elapsed_secs: f64,
}

/// Walk slots in chronological order. When `cross_seq_enabled` is true,
/// seeds are harvested from the PRIOR slot's decodes through the
/// CallsignContinuityFilter. Slot 0 is used to bootstrap the filter:
/// every decoded callsign from slot 0 is added to `static_ref` so
/// downstream slots have a non-empty trust set.
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
            Err(_) => continue, // skip missing WAVs gracefully
        };
        let truth = load_truth(&ws, sha);

        // Build seeds for THIS slot from the PRIOR slot's decodes,
        // gated through the production trust filter. Slot 0 has no
        // prior decodes → seeds is empty (the consumer no-ops).
        let seeds = if cfg.cross_sequence_a7_enabled && !prev_decodes.is_empty() {
            // Count seeds offered (pre-filter) for telemetry.
            let mut offered = 0usize;
            for d in &prev_decodes {
                for c in [&d.message.from_callsign, &d.message.to_callsign] {
                    if c.is_some() {
                        offered += 1;
                    }
                }
            }
            stats.seeds_offered_total += offered;
            let s = trust_gated_seeds(&prev_decodes, &filter);
            stats.seeds_passed_trust_total += s.len();
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

        // Score this slot against truth.
        stats.total_decodes += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                stats.total_tps += 1;
            }
        }

        // Maintenance: push this slot's accepted decodes into the
        // rolling window so subsequent slots' trust gate has a basis.
        // For slot 0, also seed the static_ref so the trust set isn't
        // born empty (simulates a populated ADIF/cqdx feed).
        if idx == 0 {
            let mut bootstrap_calls = Vec::new();
            for d in &decoded {
                for c in [&d.message.from_callsign, &d.message.to_callsign] {
                    if let Some(cs) = c {
                        let up = cs.trim().to_uppercase();
                        if !up.is_empty() && !up.starts_with("CQ") {
                            bootstrap_calls.push(up);
                        }
                    }
                }
            }
            filter.extend_from_iter(bootstrap_calls);
        }
        // Update rolling for ALL slots (including 0) so subsequent slots
        // see prior decodes the way production does.
        for d in &decoded {
            // Use accept(text) for parity with production. We ignore the
            // return; the side effect (push to rolling) is what we need.
            let _ = filter.accept(&d.text);
        }

        prev_decodes = decoded;

        if (idx + 1) % 50 == 0 {
            eprintln!(
                "    [{}/{}] tps={} cs_emits={} cs_tp={} cs_fp={} seeds_pre={} seeds_post={}",
                idx + 1,
                entries.len(),
                stats.total_tps,
                stats.cs_emits,
                stats.cs_recovered_tps,
                stats.cs_emit_fps,
                stats.seeds_offered_total,
                stats.seeds_passed_trust_total,
            );
        }
    }

    stats.elapsed_secs = t0.elapsed().as_secs_f64();
    Ok(stats)
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/chrono_replay.manifest.json"),
    )?)?;
    let mut entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    entries.sort_by(|a, b| {
        a["slot_index"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&b["slot_index"].as_i64().unwrap_or(0))
    });
    println!(
        "loaded chrono_replay full: {} sequenced slots",
        entries.len()
    );

    let mk = |cs: bool| Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        cross_sequence_a7_enabled: cs,
        ..Ft8Config::default()
    };

    eprintln!("baseline (cross_seq OFF)…");
    let s_off = run(&entries, &mk(false))?;
    println!(
        "\nbaseline (cross_seq OFF): {} decodes / {} TPs ({:.1}s)",
        s_off.total_decodes, s_off.total_tps, s_off.elapsed_secs
    );

    eprintln!("cross_seq ON (trust-gated)…");
    let s_on = run(&entries, &mk(true))?;
    println!(
        "cross_seq ON (trust-gated): {} decodes / {} TPs / cs_emits={} cs_tp={} cs_fp={} ({:.1}s)",
        s_on.total_decodes,
        s_on.total_tps,
        s_on.cs_emits,
        s_on.cs_recovered_tps,
        s_on.cs_emit_fps,
        s_on.elapsed_secs
    );

    let delta_tps = s_on.total_tps as i64 - s_off.total_tps as i64;
    let delta_decodes = s_on.total_decodes as i64 - s_off.total_decodes as i64;
    let delta_fps = (s_on.total_decodes as i64 - s_on.total_tps as i64)
        - (s_off.total_decodes as i64 - s_off.total_tps as i64);

    let notes_path = ws.join("research/notes/2026-06-09-batch54-hb237-trust-gated.md");
    let prec_off = s_off.total_tps as f64 / s_off.total_decodes.max(1) as f64;
    let prec_on = s_on.total_tps as f64 / s_on.total_decodes.max(1) as f64;
    let trust_filter_pass_rate = if s_on.seeds_offered_total > 0 {
        s_on.seeds_passed_trust_total as f64 / s_on.seeds_offered_total as f64
    } else {
        0.0
    };

    let decision = if delta_tps > 0 && delta_fps <= 0 {
        "**Recommend default-ON** with the trust gate: TPs lift AND no net FP increase."
    } else if delta_tps > 0 && delta_fps > 0 && (delta_tps as f64 / delta_fps.max(1) as f64) > 2.0 {
        "**Consider default-ON** with trust gate: TPs lift > 2× FP cost. Run hard_1000 to confirm before flipping."
    } else if delta_tps == 0 && delta_fps < 0 {
        "**Borderline**: TPs flat but FPs drop. Defensible default-ON if precision matters more than recall."
    } else if delta_tps < 0 || delta_fps > 0 {
        "**Keep default-OFF**: net-negative or FP-amplifying even with trust gate engaged."
    } else {
        "**Net-zero**: mechanism is inert on this corpus + trust gate combination. Stays default-OFF."
    };

    let body = format!(
        "# Batch 54 — hb-237 trust-gated cross-sequence A7 on chrono_replay full\n\n\
         Production-shape probe: `CallsignContinuityFilter` strict mode \
         (rolling_cap=200), slot 0 bootstrap from own decodes. \
         300 sequenced slots from `chrono_replay.manifest.json`.\n\n\
         Config: `max_decode_passes = 2, ldpc_iterations = 200`. Only \
         `cross_sequence_a7_enabled` toggled.\n\n\
         | Config | Decodes | TPs | FPs | Precision | Elapsed |\n\
         |---|---:|---:|---:|---:|---:|\n\
         | baseline (cross_seq OFF) | {} | {} | {} | {:.4} | {:.1}s |\n\
         | cross_seq ON (trust-gated) | {} | {} | {} | {:.4} | {:.1}s |\n\n\
         **Δ TPs**: {:+}\n\
         **Δ FPs**: {:+}\n\
         **Δ Decodes**: {:+}\n\n\
         ## Cross-sequence telemetry\n\n\
         - `cs_emits` (recoveries added by cross-sequence consumer): **{}**\n\
         - `cs_recovered_tps` (of those, matched truth): **{}**\n\
         - `cs_emit_fps` (of those, did NOT match truth): **{}**\n\
         - Seeds offered by prior-slot decodes: **{}**\n\
         - Seeds passed trust gate: **{}** ({:.1}% trust-filter pass rate)\n\n\
         ## Decision\n\n{}\n\n\
         ## Comparison to Batch 53 mini33 (no trust gate)\n\n\
         - Batch 53 mini33: 87 cs_emits / 0 TPs / 87 FPs (precision crash 0.7650 → 0.6977)\n\
         - Batch 54 chrono_replay full: {} cs_emits / {} TPs / {} FPs\n\
         - Per-slot rate (33 vs 300 slots) makes the comparison apples-to-apples after normalization.\n",
        s_off.total_decodes,
        s_off.total_tps,
        s_off.total_decodes - s_off.total_tps,
        prec_off,
        s_off.elapsed_secs,
        s_on.total_decodes,
        s_on.total_tps,
        s_on.total_decodes - s_on.total_tps,
        prec_on,
        s_on.elapsed_secs,
        delta_tps,
        delta_fps,
        delta_decodes,
        s_on.cs_emits,
        s_on.cs_recovered_tps,
        s_on.cs_emit_fps,
        s_on.seeds_offered_total,
        s_on.seeds_passed_trust_total,
        trust_filter_pass_rate * 100.0,
        decision,
        s_on.cs_emits,
        s_on.cs_recovered_tps,
        s_on.cs_emit_fps,
    );
    std::fs::write(&notes_path, body)?;
    println!("\nDone. Results in {}", notes_path.display());
    Ok(())
}
