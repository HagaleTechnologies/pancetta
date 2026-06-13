//! Batch 50 — hard-200 toggle measurement of the three Batch 49 mechanisms
//! that landed default-OFF:
//!
//! 1. **hb-226 Gaussian-ramp subtract**
//!    (`gaussian_ramp_subtract_enabled = true`, default fraction 0.11)
//! 2. **JS8Call-Improved LDPC feedback refinement**
//!    (`ldpc_feedback_refinement_enabled = true`, default boost/attenuate/erase)
//! 3. **hb-244 soft combiner**
//!    (`soft_combiner_enabled = true`, default capacity/TTL)
//! 4. **All three ON together** — interaction probe.
//!
//! Baseline: `Ft8Config::default()` with `max_decode_passes = 2` and
//! `ldpc_iterations = 200` (the established Batch 48/49 5301-TP baseline
//! on hard-200). All three new flags explicitly OFF.
//!
//! Stop conditions:
//! - If baseline ≠ 5301 ± 5 TPs, something regressed in Batch 49 commits;
//!   the controller will investigate before trusting per-config deltas.
//!
//! Expected behavioural priors:
//! - hb-244 soft combiner: ~no-op on hard-200. The corpus is largely
//!   single-reception per signal, so the cross-reception LLR accumulation
//!   has nothing to combine. A small positive or null result is the
//!   honest call. A large negative would indicate the off-path branch is
//!   leaking into normal decodes (regression).
//! - hb-226 Gaussian-ramp subtract: amplifies subtraction quality in the
//!   multipass loop; could go either way depending on whether splatter
//!   was hiding real candidates or surfacing FPs.
//! - LDPC feedback refinement: adds one extra BP retry with refined LLRs
//!   before OSD on failures. Could surface a few new decodes (positive)
//!   or destabilise already-marginal CRCs (negative).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch50_b49_measure

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
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

fn run(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize)> {
    let ws = workspace_root()?;
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
    Ok((total, tps))
}

/// Construct the established Batch 48/49 baseline: mp=2 + ldpc=200, with
/// every Batch 49 mechanism explicitly OFF. Explicit-OFF instead of
/// "trust the default" so future default flips don't silently invalidate
/// the baseline column.
fn baseline_cfg() -> Ft8Config {
    let mut cfg = Ft8Config::default();
    cfg.max_decode_passes = 2;
    cfg.ldpc_iterations = 200;
    // Batch 49 mechanisms — all default OFF after the landed commits, but
    // pin explicitly so a future default flip doesn't shift the baseline.
    cfg.gaussian_ramp_subtract_enabled = false;
    cfg.ldpc_feedback_refinement_enabled = false;
    cfg.soft_combiner_enabled = false;
    cfg
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest_path = ws.join("research/corpus/curated/ft8/hard_200.manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "failed to load hard-200 manifest at {} — STOP and report",
            manifest_path.display()
        )
    })?;
    let manifest: Value = serde_json::from_str(&manifest_text)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    eprintln!("loaded hard-200 manifest: {} entries", entries.len());

    println!("## Batch 50 — hard-200 toggle measurement of Batch 49 mechanisms");
    println!("(mp=2, ldpc=200; each mechanism toggled ON individually then all-on)");

    // 1. Baseline.
    let cfg_base = baseline_cfg();
    eprintln!("baseline (all Batch 49 mechanisms OFF)…");
    let t0 = Instant::now();
    let (b_total, b_tps) = run(&entries, &cfg_base)?;
    let dt_base = t0.elapsed();
    println!(
        "\n### Baseline (mp=2, ldpc=200, all Batch 49 OFF): {} decodes / {} TPs  [{:.1}s]",
        b_total,
        b_tps,
        dt_base.as_secs_f64()
    );
    // Stop-condition gate: warn loudly but keep running so the user can
    // see the full table even if baseline shifted.
    let drift = (b_tps as i64 - 5301).abs();
    if drift > 5 {
        eprintln!(
            "WARN: baseline {} TPs is {} TPs off the expected 5301 ± 5 — \
             check Batch 49 commits for unintended regression",
            b_tps, drift
        );
    }

    // 2. hb-226 Gaussian-ramp subtract ON only.
    let mut cfg_ramp = cfg_base.clone();
    cfg_ramp.gaussian_ramp_subtract_enabled = true;
    eprintln!("hb-226 Gaussian-ramp subtract ON…");
    let t0 = Instant::now();
    let (t_ramp, p_ramp) = run(&entries, &cfg_ramp)?;
    let dt_ramp = t0.elapsed();
    println!(
        "\n### hb-226 ramp ON: {} decodes / {} TPs (Δ {:+})  [{:.1}s]",
        t_ramp,
        p_ramp,
        p_ramp as i64 - b_tps as i64,
        dt_ramp.as_secs_f64()
    );

    // 3. LDPC feedback refinement ON only.
    let mut cfg_fb = cfg_base.clone();
    cfg_fb.ldpc_feedback_refinement_enabled = true;
    eprintln!("LDPC feedback refinement ON…");
    let t0 = Instant::now();
    let (t_fb, p_fb) = run(&entries, &cfg_fb)?;
    let dt_fb = t0.elapsed();
    println!(
        "\n### LDPC feedback ON: {} decodes / {} TPs (Δ {:+})  [{:.1}s]",
        t_fb,
        p_fb,
        p_fb as i64 - b_tps as i64,
        dt_fb.as_secs_f64()
    );

    // 4. hb-244 soft combiner ON only.
    let mut cfg_sc = cfg_base.clone();
    cfg_sc.soft_combiner_enabled = true;
    eprintln!("hb-244 soft combiner ON…");
    let t0 = Instant::now();
    let (t_sc, p_sc) = run(&entries, &cfg_sc)?;
    let dt_sc = t0.elapsed();
    println!(
        "\n### hb-244 soft combiner ON: {} decodes / {} TPs (Δ {:+})  [{:.1}s]",
        t_sc,
        p_sc,
        p_sc as i64 - b_tps as i64,
        dt_sc.as_secs_f64()
    );

    // 5. All three ON together.
    let mut cfg_all = cfg_base.clone();
    cfg_all.gaussian_ramp_subtract_enabled = true;
    cfg_all.ldpc_feedback_refinement_enabled = true;
    cfg_all.soft_combiner_enabled = true;
    eprintln!("all three Batch 49 mechanisms ON…");
    let t0 = Instant::now();
    let (t_all, p_all) = run(&entries, &cfg_all)?;
    let dt_all = t0.elapsed();
    println!(
        "\n### All three ON: {} decodes / {} TPs (Δ {:+})  [{:.1}s]",
        t_all,
        p_all,
        p_all as i64 - b_tps as i64,
        dt_all.as_secs_f64()
    );

    println!("\n### Summary table");
    println!("| Config              | Decodes | TPs | Δ TPs | Wall-clock |");
    println!("|---------------------|--------:|----:|------:|-----------:|");
    println!(
        "| Baseline            | {:>7} | {:>3} |   {:+3} | {:>9.1}s |",
        b_total,
        b_tps,
        0,
        dt_base.as_secs_f64()
    );
    println!(
        "| hb-226 ramp ON      | {:>7} | {:>3} | {:+5} | {:>9.1}s |",
        t_ramp,
        p_ramp,
        p_ramp as i64 - b_tps as i64,
        dt_ramp.as_secs_f64()
    );
    println!(
        "| LDPC feedback ON    | {:>7} | {:>3} | {:+5} | {:>9.1}s |",
        t_fb,
        p_fb,
        p_fb as i64 - b_tps as i64,
        dt_fb.as_secs_f64()
    );
    println!(
        "| hb-244 combiner ON  | {:>7} | {:>3} | {:+5} | {:>9.1}s |",
        t_sc,
        p_sc,
        p_sc as i64 - b_tps as i64,
        dt_sc.as_secs_f64()
    );
    println!(
        "| All three ON        | {:>7} | {:>3} | {:+5} | {:>9.1}s |",
        t_all,
        p_all,
        p_all as i64 - b_tps as i64,
        dt_all.as_secs_f64()
    );

    Ok(())
}
