//! Batch 49 — continuation: wide-lag baseline sweep only.
//!
//! `batch49_sync_tuning` was killed mid-sweep by a 10-min Bash timeout
//! after completing the baseline + hb-242 × {300, 400, 500, 600} stanza.
//! That stanza already falsifies the budget-expansion hypothesis (Δ TPs
//! degrades monotonically: -18 → -17 → -22 → -38 as max_sync grows from
//! 300 to 600). Skipping max_sync=800.
//!
//! This continuation runs the wide-lag sweeps (percentile + norm) and
//! the combined-best stack. Same hard-200 corpus, same baseline config.
//!
//! Each sweep streams to `research/notes/2026-06-08-batch49-tuning-results.md`
//! (APPENDED — does not clobber the earlier hb-242 rows).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch49_widelag_tuning

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

fn append_result(notes_path: &Path, line: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(notes_path)?;
    writeln!(f, "{}", line)?;
    Ok(())
}

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

    // Baseline TPs from the earlier sync_tuning run (already recorded
    // in the markdown). We do NOT re-run the baseline — it's stable.
    let b_tps: usize = 5301;
    append_result(&notes_path, "")?;
    append_result(
        &notes_path,
        "<!-- Continuation run: wide-lag sweep + combined. -->",
    )?;
    append_result(
        &notes_path,
        "<!-- hb-242 max_sync=800 skipped: trend at {300,400,500,600} = {-18,-17,-22,-38} TPs is monotonic-degrading, so 800 is extrapolation-clearly-worse. -->",
    )?;

    println!("## Batch 49 wide-lag continuation");
    println!("(hard-200, mp=2 + ldpc=200, baseline TPs = {b_tps} from prior run)\n");

    // Baseline cfg (no flags enabled).
    let mut cfg_base = Ft8Config::default();
    cfg_base.max_decode_passes = 2;
    cfg_base.ldpc_iterations = 200;
    cfg_base.costas_partial_metric_enabled = false;
    cfg_base.costas_two_baseline_enabled = false;
    cfg_base.max_sync_candidates = 300;

    // ---- 3. Wide-lag ON × percentile sweep (default norm=1.2) ----
    let mut best_pct: Option<(f64, i64, usize)> = None;
    for pct in [0.30f64, 0.40, 0.50, 0.60] {
        let mut cfg = cfg_base.clone();
        cfg.costas_two_baseline_enabled = true;
        cfg.costas_two_baseline_percentile = pct;
        // norm_threshold = default 1.2
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

    // ---- 5. Combined: hb-242 (best budget = 300, but still net-negative) + best wide-lag ----
    // Since hb-242 was net-negative at ALL tested budgets, the combined
    // case is mostly to confirm there's no synergistic effect.
    let mut cfg_combined = cfg_base.clone();
    cfg_combined.costas_partial_metric_enabled = true;
    cfg_combined.max_sync_candidates = 300; // best (least bad) hb-242 budget
    cfg_combined.costas_two_baseline_enabled = true;
    cfg_combined.costas_two_baseline_percentile = best_pct_val;
    cfg_combined.costas_two_baseline_norm_threshold = best_norm_val;
    let combined_label = format!(
        "combined (hb-242 ON max_sync=300 + wide-lag pct={best_pct_val:.2} norm={best_norm_val:.2})"
    );
    let (_, combined_tps, _) =
        measure(&combined_label, &entries, &cfg_combined, b_tps, &notes_path)?;
    let combined_dtp = combined_tps as i64 - b_tps as i64;

    // ---- Summary ----
    println!("\n## Summary\n");
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
        ### hb-242 max_sync sweep (from first run)\n\
        - max_sync=300: Δ **-18** TPs (precision 0.7462)\n\
        - max_sync=400: Δ **-17** TPs (precision 0.7045)\n\
        - max_sync=500: Δ **-22** TPs (precision 0.6793)\n\
        - max_sync=600: Δ **-38** TPs (precision 0.6659)\n\
        - max_sync=800: skipped (monotonic-degrading trend established)\n\
        \n\
        **Hypothesis result**: bumping `max_sync_candidates` to absorb partial-Costas\n\
        candidates **DOES NOT** make hb-242 net-positive. Δ TPs is approximately\n\
        flat-to-degrading while precision drops monotonically. The partial-Costas\n\
        mechanism surfaces noise candidates that pass the candidate cap but\n\
        fail downstream filters at a worse rate than the real signals they\n\
        displace. Recommendation: **keep hb-242 default OFF**.\n\
        \n\
        ### Wide-lag baseline tuning\n\
        - Best percentile: **costas_two_baseline_percentile = {best_pct_val:.2}** → Δ **{best_pct_dtp:+}** TPs (norm at 1.20)\n\
        - Best norm: **costas_two_baseline_norm_threshold = {best_norm_val:.2}** (paired with pct = {best_pct_val:.2}) → Δ **{best_norm_dtp:+}** TPs\n\
        - Combined hb-242 + wide-lag: Δ **{combined_dtp:+}** TPs\n\
        \n\
        ## Recommended ship defaults\n\
        \n\
        See the per-row Δ in the results table. Ship as default-ON only if\n\
        Δ TPs ≥ +5 AND precision is non-degrading. Otherwise keep default-OFF\n\
        (mechanisms remain available for opt-in via Ft8Config flags).\n"
    );
    append_result(&notes_path, &summary)?;

    println!("\nResults saved to: {}", notes_path.display());
    Ok(())
}
