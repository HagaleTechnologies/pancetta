//! Batch 49 — tune Batch 48's measurement-negative mechanisms.
//!
//! Batch 48 shipped two mechanisms default-OFF after hard-200 measurement:
//!   - hb-242 sync_bc (partial-Costas metric) — net -18 TPs at default-ON
//!     because partial-Costas surfaces additional noise candidates that
//!     eat into the `max_sync_candidates = 300` cap, displacing real TPs.
//!   - Wide-lag baseline (red2) — net -4 TPs at default-ON, closer to
//!     neutral; the percentile-normalization may need tuning.
//!
//! This probe sweeps:
//!   1. Baseline (status quo: hb-242 OFF, wide-lag OFF) on mp=2 + ldpc=200
//!   2. hb-242 ON + `max_sync_candidates` ∈ {300, 400, 500, 600, 800}
//!      to find the operating point where partial-Costas becomes
//!      net-positive (if any).
//!   3. Wide-lag ON + `costas_two_baseline_percentile` ∈ {0.30, 0.40,
//!      0.50, 0.60} (all other knobs at default).
//!   4. Wide-lag ON + `costas_two_baseline_norm_threshold` ∈ {1.0, 1.2,
//!      1.5, 2.0} (all other knobs at default).
//!   5. Combined: best hb-242 config + best wide-lag config, stacked.
//!
//! Results stream to `research/notes/2026-06-08-batch49-tuning-results.md`
//! as each configuration completes so partial data survives interruption.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch49_sync_tuning

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

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

/// Run a single configuration over the full hard-200 corpus.
/// Returns (decodes, TPs, elapsed_secs).
fn run(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, f64)> {
    let ws = workspace_root()?;
    let start = Instant::now();
    let mut total = 0usize;
    let mut tps = 0usize;
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
    Ok((total, tps, start.elapsed().as_secs_f64()))
}

/// Append a result row to the markdown results file (created on first write).
fn append_result(notes_path: &Path, line: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(notes_path)?;
    writeln!(f, "{}", line)?;
    Ok(())
}

/// One stanza: label, run, record, compare against baseline.
fn measure(
    label: &str,
    entries: &[Value],
    cfg: &Ft8Config,
    baseline_tps: usize,
    notes_path: &Path,
) -> Result<(usize, usize, f64)> {
    eprintln!(">>> {label}…");
    let (decodes, tps, secs) = run(entries, cfg)?;
    let delta = tps as i64 - baseline_tps as i64;
    let precision = tps as f64 / decodes.max(1) as f64;
    let line = format!("| {label} | {decodes} | {tps} | {delta:+} | {precision:.4} | {secs:.1}s |");
    println!("{line}");
    append_result(notes_path, &line)?;
    Ok((decodes, tps, secs))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();

    let notes_path = ws.join("research/notes/2026-06-08-batch49-tuning-results.md");

    // Fresh results file (the markdown header is rewritten per-run; bodies
    // are appended).
    std::fs::write(
        &notes_path,
        format!(
            "# Batch 49 — hb-242 + wide-lag baseline tuning results\n\
            \n\
            **Date**: 2026-06-08\n\
            **Branch**: iter/2026-06-08-batch-49\n\
            **Corpus**: hard-200 ({} WAVs)\n\
            **Probe**: `pancetta-research/examples/batch49_sync_tuning.rs`\n\
            \n\
            Per-row config:\n\
            - Base: `max_decode_passes = 2`, `ldpc_iterations = 200`\n\
            - Tuned: see column name (`max_sync_candidates`, `percentile`, `norm_threshold`).\n\
            \n\
            ## Results table\n\
            \n\
            | Config | Decodes | TPs | Δ TPs | Precision | Elapsed |\n\
            |--------|--------:|----:|------:|----------:|--------:|\n",
            entries.len()
        ),
    )?;

    println!("## Batch 49 — hb-242 + wide-lag baseline tuning sweep");
    println!("(hard-200, mp=2 + ldpc=200)\n");
    println!("| Config | Decodes | TPs | Δ TPs | Precision | Elapsed |");
    println!("|--------|--------:|----:|------:|----------:|--------:|");

    // ---- 1. Baseline ----
    let mut cfg_base = Ft8Config::default();
    cfg_base.max_decode_passes = 2;
    cfg_base.ldpc_iterations = 200;
    cfg_base.costas_partial_metric_enabled = false;
    cfg_base.costas_two_baseline_enabled = false;
    // Explicit baseline candidate budget (matches MAX_SYNC_CANDIDATES default).
    cfg_base.max_sync_candidates = 300;

    eprintln!(">>> baseline (hb-242 OFF, wide-lag OFF, max_sync=300)…");
    let t0 = Instant::now();
    let (b_decodes, b_tps, _) = run(&entries, &cfg_base)?;
    let b_secs = t0.elapsed().as_secs_f64();
    let b_precision = b_tps as f64 / b_decodes.max(1) as f64;
    let base_line = format!(
        "| baseline (mp=2, ldpc=200, max_sync=300) | {b_decodes} | {b_tps} | +0 | {b_precision:.4} | {b_secs:.1}s |"
    );
    println!("{base_line}");
    append_result(&notes_path, &base_line)?;

    // ---- 2. hb-242 ON × max_sync_candidates sweep ----
    let mut best_h242: Option<(usize, i64, usize)> = None; // (max_sync, dtp, tps)
    for budget in [300usize, 400, 500, 600, 800] {
        let mut cfg = cfg_base.clone();
        cfg.costas_partial_metric_enabled = true;
        cfg.max_sync_candidates = budget;
        let label = format!("hb-242 ON, max_sync={budget}");
        let (_, tps, _) = measure(&label, &entries, &cfg, b_tps, &notes_path)?;
        let dtp = tps as i64 - b_tps as i64;
        if best_h242.map(|(_, d, _)| dtp > d).unwrap_or(true) {
            best_h242 = Some((budget, dtp, tps));
        }
    }
    let (best_h242_budget, best_h242_dtp, _) = best_h242.expect("at least one h242 cfg ran");

    // ---- 3. Wide-lag ON × percentile sweep (default norm_threshold=1.2) ----
    let mut best_pct: Option<(f64, i64, usize)> = None;
    for pct in [0.30f64, 0.40, 0.50, 0.60] {
        let mut cfg = cfg_base.clone();
        cfg.costas_two_baseline_enabled = true;
        cfg.costas_two_baseline_percentile = pct;
        // Default norm_threshold = 1.2 (already in Default).
        let label = format!("wide-lag ON, percentile={pct:.2}, norm=1.20");
        let (_, tps, _) = measure(&label, &entries, &cfg, b_tps, &notes_path)?;
        let dtp = tps as i64 - b_tps as i64;
        if best_pct.map(|(_, d, _)| dtp > d).unwrap_or(true) {
            best_pct = Some((pct, dtp, tps));
        }
    }
    let (best_pct_val, best_pct_dtp, _) = best_pct.expect("at least one percentile ran");

    // ---- 4. Wide-lag ON × norm_threshold sweep (percentile = best from #3) ----
    let mut best_norm: Option<(f64, i64, usize)> = None;
    for norm in [1.0f64, 1.2, 1.5, 2.0] {
        let mut cfg = cfg_base.clone();
        cfg.costas_two_baseline_enabled = true;
        cfg.costas_two_baseline_percentile = best_pct_val;
        cfg.costas_two_baseline_norm_threshold = norm;
        let label = format!("wide-lag ON, percentile={best_pct_val:.2}, norm={norm:.2}");
        let (_, tps, _) = measure(&label, &entries, &cfg, b_tps, &notes_path)?;
        let dtp = tps as i64 - b_tps as i64;
        if best_norm.map(|(_, d, _)| dtp > d).unwrap_or(true) {
            best_norm = Some((norm, dtp, tps));
        }
    }
    let (best_norm_val, best_norm_dtp, _) = best_norm.expect("at least one norm cfg ran");

    // ---- 5. Combined: best hb-242 + best wide-lag stacked ----
    let mut cfg_combined = cfg_base.clone();
    cfg_combined.costas_partial_metric_enabled = true;
    cfg_combined.max_sync_candidates = best_h242_budget;
    cfg_combined.costas_two_baseline_enabled = true;
    cfg_combined.costas_two_baseline_percentile = best_pct_val;
    cfg_combined.costas_two_baseline_norm_threshold = best_norm_val;
    let combined_label = format!(
        "combined (hb-242 + wide-lag, max_sync={best_h242_budget}, pct={best_pct_val:.2}, norm={best_norm_val:.2})"
    );
    let (_, combined_tps, _) =
        measure(&combined_label, &entries, &cfg_combined, b_tps, &notes_path)?;
    let combined_dtp = combined_tps as i64 - b_tps as i64;

    // ---- Summary ----
    println!("\n## Summary\n");
    println!(
        "Baseline: {} decodes / {} TPs ({:.4} precision)",
        b_decodes, b_tps, b_precision
    );
    println!(
        "Best hb-242:        max_sync={}, Δ {:+} TPs",
        best_h242_budget, best_h242_dtp
    );
    println!(
        "Best wide-lag pct:  percentile={:.2}, Δ {:+} TPs",
        best_pct_val, best_pct_dtp
    );
    println!(
        "Best wide-lag norm: percentile={:.2}, norm={:.2}, Δ {:+} TPs",
        best_pct_val, best_norm_val, best_norm_dtp
    );
    println!("Combined:           Δ {:+} TPs", combined_dtp);

    let summary = format!(
        "\n## Summary\n\
        \n\
        - Baseline TPs: **{b_tps}** ({b_decodes} decodes, {b_precision:.4} precision)\n\
        - Best hb-242 budget: **max_sync_candidates = {best_h242_budget}** → Δ **{best_h242_dtp:+}** TPs\n\
        - Best wide-lag percentile: **costas_two_baseline_percentile = {best_pct_val:.2}** → Δ **{best_pct_dtp:+}** TPs (norm at default 1.20)\n\
        - Best wide-lag norm: **costas_two_baseline_norm_threshold = {best_norm_val:.2}** (paired with percentile = {best_pct_val:.2}) → Δ **{best_norm_dtp:+}** TPs\n\
        - Combined (best hb-242 + best wide-lag): Δ **{combined_dtp:+}** TPs\n\
        \n\
        ## Recommended defaults\n\
        \n\
        See the table above. Any configuration with Δ TPs ≥ +0 is a candidate for default-ON;\n\
        a configuration with Δ TPs ≥ +5 should be shipped as default-ON; otherwise keep default-OFF.\n"
    );
    append_result(&notes_path, &summary)?;

    println!("\nResults saved to: {}", notes_path.display());
    Ok(())
}
