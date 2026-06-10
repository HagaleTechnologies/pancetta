//! Batch 66 — hb-244 soft combiner re-measurement against a real-world
//! repeat-heavy corpus.
//!
//! Batch 62-63 measured hb-244 on synthetic Gaussian-noise repeats and
//! found it inert; root-cause analysis suggested the regime (sync
//! detects + LDPC fails + accumulation succeeds) requires natural
//! fading that synthetic noise doesn't reproduce. Batch 65's corpus
//! characterization confirmed real recordings DO have the regime:
//! raw_20260530 has max_repeats=324 by one callsign in a 500-slot
//! sample.
//!
//! This probe re-measures hb-244 with:
//!   - The `repeat_heavy_530` curated corpus (built by
//!     batch66_build_manifest.py from the slot scan).
//!   - ft8_lib FFI truth labels (Batch 66 baseline files at
//!     research/baselines/ft8/<sha>.ft8lib.json).
//!
//! Six configurations:
//!   1. baseline (combiner OFF, fresh decoder per slot)
//!   2. persistent decoder, combiner OFF (sanity control)
//!   3. persistent decoder, combiner ON, key_tolerance=0
//!   4. persistent decoder, combiner ON, key_tolerance=1
//!   5. persistent decoder, combiner ON, key_tolerance=2
//!
//! The decision: does (3/4/5) recover more total TPs than (1/2)?
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch66_hb244_real_corpus -- \
//!     --manifest research/corpus/curated/ft8/repeat_heavy_530.manifest.json

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
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

fn load_truth(ws: &Path, sha: &str, suffix: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{sha}.{suffix}.json"));
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

#[derive(Default, Debug, Clone)]
struct RunStats {
    decodes: usize,
    tps: usize,
    fps: usize,
    elapsed_secs: f64,
}

fn run_standalone(entries: &[Value], cfg: &Ft8Config) -> Result<RunStats> {
    let ws = workspace_root()?;
    let mut s = RunStats::default();
    let t0 = std::time::Instant::now();
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha, "ft8lib");
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        s.decodes += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                s.tps += 1;
            } else {
                s.fps += 1;
            }
        }
        if (i + 1) % 50 == 0 {
            eprintln!("  [standalone {}/{}] tps={}", i + 1, entries.len(), s.tps);
        }
    }
    s.elapsed_secs = t0.elapsed().as_secs_f64();
    Ok(s)
}

fn run_persistent(entries: &[Value], cfg: &Ft8Config) -> Result<RunStats> {
    let ws = workspace_root()?;
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let mut s = RunStats::default();
    let t0 = std::time::Instant::now();
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha, "ft8lib");
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        s.decodes += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                s.tps += 1;
            } else {
                s.fps += 1;
            }
        }
        if (i + 1) % 50 == 0 {
            eprintln!("  [persistent {}/{}] tps={}", i + 1, entries.len(), s.tps);
        }
    }
    s.elapsed_secs = t0.elapsed().as_secs_f64();
    Ok(s)
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
    let entries: Vec<Value> = m["entries"]
        .as_array()
        .context("manifest has no entries")?
        .iter()
        .cloned()
        .collect();
    println!("loaded manifest: {} entries", entries.len());

    let mk = |on: bool, tol: u32| Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        soft_combiner_enabled: on,
        soft_combiner_key_tolerance: tol,
        ..Ft8Config::default()
    };

    eprintln!("(1) standalone, combiner OFF (fresh decoder per slot)…");
    let s1 = run_standalone(&entries, &mk(false, 0))?;
    eprintln!("(2) persistent decoder, combiner OFF (sanity)…");
    let s2 = run_persistent(&entries, &mk(false, 0))?;
    eprintln!("(3) persistent decoder, combiner ON, tol=0…");
    let s3 = run_persistent(&entries, &mk(true, 0))?;
    eprintln!("(4) persistent decoder, combiner ON, tol=1…");
    let s4 = run_persistent(&entries, &mk(true, 1))?;
    eprintln!("(5) persistent decoder, combiner ON, tol=2…");
    let s5 = run_persistent(&entries, &mk(true, 2))?;

    println!("\n| Config | Decodes | TPs | FPs | Elapsed |");
    println!("|---|---:|---:|---:|---:|");
    println!(
        "| (1) standalone, OFF        | {} | {} | {} | {:.1}s |",
        s1.decodes, s1.tps, s1.fps, s1.elapsed_secs
    );
    println!(
        "| (2) persistent, OFF        | {} | {} | {} | {:.1}s |",
        s2.decodes, s2.tps, s2.fps, s2.elapsed_secs
    );
    println!(
        "| (3) persistent, ON, tol=0  | {} | {} | {} | {:.1}s |",
        s3.decodes, s3.tps, s3.fps, s3.elapsed_secs
    );
    println!(
        "| (4) persistent, ON, tol=1  | {} | {} | {} | {:.1}s |",
        s4.decodes, s4.tps, s4.fps, s4.elapsed_secs
    );
    println!(
        "| (5) persistent, ON, tol=2  | {} | {} | {} | {:.1}s |",
        s5.decodes, s5.tps, s5.fps, s5.elapsed_secs
    );

    let dtp3 = s3.tps as i64 - s2.tps as i64;
    let dtp4 = s4.tps as i64 - s2.tps as i64;
    let dtp5 = s5.tps as i64 - s2.tps as i64;
    let dfp3 = s3.fps as i64 - s2.fps as i64;
    let dfp4 = s4.fps as i64 - s2.fps as i64;
    let dfp5 = s5.fps as i64 - s2.fps as i64;
    println!(
        "\nΔ vs (2): tol=0: tp {dtp3:+}, fp {dfp3:+}; tol=1: tp {dtp4:+}, fp {dfp4:+}; tol=2: tp {dtp5:+}, fp {dfp5:+}"
    );

    let best_dtp = dtp3.max(dtp4).max(dtp5);
    let decision = if best_dtp >= 5 {
        format!(
            "**hb-244 shows real-world lift**: best Δ TPs = {best_dtp:+}. Investigation now warranted: is this graduation territory or a quirk of this corpus?"
        )
    } else if best_dtp >= 1 {
        format!(
            "**Marginal hb-244 lift on real corpus**: Δ TPs = {best_dtp:+}. Re-measure on a larger corpus before considering ship."
        )
    } else if best_dtp == 0 {
        "**hb-244 still inert on real corpus**: cache-key alignment problem persists even on natural-fading data. The mechanism appears to be fundamentally narrow in regime.".to_string()
    } else {
        format!(
            "**hb-244 regresses on real corpus**: {best_dtp:+}. The widened key tolerance brings in spurious cross-callsign matches."
        )
    };
    println!("\n## Decision\n\n{decision}\n");

    let notes_path = ws.join("research/notes/2026-06-09-batch66-hb244-real.md");
    let body = format!(
        "# Batch 66 — hb-244 on real-world repeat-heavy corpus\n\n\
         Manifest: {manifest}, {} slots.\n\
         Truth source: ft8_lib FFI (research/baselines/ft8/<sha>.ft8lib.json).\n\n\
         | Config | Decodes | TPs | FPs |\n|---|---:|---:|---:|\n\
         | (1) standalone, OFF        | {} | {} | {} |\n\
         | (2) persistent, OFF        | {} | {} | {} |\n\
         | (3) persistent, ON, tol=0  | {} | {} | {} |\n\
         | (4) persistent, ON, tol=1  | {} | {} | {} |\n\
         | (5) persistent, ON, tol=2  | {} | {} | {} |\n\n\
         Δ vs (2): tol=0 {dtp3:+}/{dfp3:+}; tol=1 {dtp4:+}/{dfp4:+}; tol=2 {dtp5:+}/{dfp5:+}.\n\n\
         {decision}\n",
        entries.len(),
        s1.decodes,
        s1.tps,
        s1.fps,
        s2.decodes,
        s2.tps,
        s2.fps,
        s3.decodes,
        s3.tps,
        s3.fps,
        s4.decodes,
        s4.tps,
        s4.fps,
        s5.decodes,
        s5.tps,
        s5.fps,
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
