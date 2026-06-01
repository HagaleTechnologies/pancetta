//! curate_jt9_rich — score baselined WAVs by their "jt9-novel-density"
//! (jt9 decode count minus pancetta decode count) and emit a curated tier
//! manifest of the top-N WAVs where jt9 finds meaningfully more than
//! pancetta. The inverse of the existing `curate` binary (which picks
//! WAVs by pancetta's own difficulty heuristic).
//!
//! This tier is hb-150's deliverable: a corpus that stresses pancetta's
//! recall gap vs jt9, unblocking sync-related hypotheses (hb-015 family)
//! and bias-detection work. See `docs/superpowers/specs/` for context.
//!
//! Inputs
//! ------
//! * `--baselines-dir`: directory of jt9 baseline cache files
//!   (`<wav_sha256>.json`). Each file's decode count is the jt9 truth.
//! * `--manifest-snapshot`: existing curated manifests
//!   (hard_1000.manifest.json, wild_100.manifest.json) used to harvest
//!   per-WAV pancetta_decode_count without re-decoding.
//! * For any baselined WAV not present in the snapshot, this binary
//!   re-decodes the WAV with the current pancetta-ft8 default config to
//!   obtain a fresh count.
//!
//! Output
//! ------
//! `hard_jt9_rich_200.manifest.json` (top 200 by gap = jt9 - pancetta),
//! same schema as the existing manifests.
//!
//! Usage
//! -----
//! ```
//! cargo run --release -p pancetta-research --bin curate_jt9_rich -- \
//!     --baselines-dir research/baselines/ft8 \
//!     --manifest-snapshot research/corpus/curated/ft8/hard_1000.manifest.json \
//!     --manifest-snapshot research/corpus/curated/ft8/wild_100.manifest.json \
//!     --output research/corpus/curated/ft8/hard_jt9_rich_200.manifest.json \
//!     --top-n 200
//! ```

use anyhow::Context;
use chrono::Utc;
use pancetta_research::curated::{CuratedEntry, CuratedManifest, ScoreBreakdown};
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Args {
    baselines_dir: PathBuf,
    manifest_snapshots: Vec<PathBuf>,
    output: PathBuf,
    top_n: usize,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut baselines_dir: Option<PathBuf> = None;
        let mut manifest_snapshots: Vec<PathBuf> = Vec::new();
        let mut output: Option<PathBuf> = None;
        let mut top_n: usize = 200;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--baselines-dir" => {
                    baselines_dir =
                        Some(iter.next().context("--baselines-dir needs a value")?.into());
                }
                "--manifest-snapshot" => {
                    manifest_snapshots.push(
                        iter.next()
                            .context("--manifest-snapshot needs a value")?
                            .into(),
                    );
                }
                "--output" => {
                    output = Some(iter.next().context("--output needs a value")?.into());
                }
                "--top-n" => {
                    top_n = iter.next().context("--top-n needs a value")?.parse()?;
                }
                "-h" | "--help" => {
                    eprintln!(
                        "usage: curate_jt9_rich --baselines-dir <dir> --manifest-snapshot <file> [--manifest-snapshot <file> ...] --output <file> [--top-n N]"
                    );
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            baselines_dir: baselines_dir.context("--baselines-dir required")?,
            manifest_snapshots,
            output: output.context("--output required")?,
            top_n,
        })
    }
}

#[derive(Debug, Deserialize)]
struct BaselineCache {
    wav_path: PathBuf,
    wav_sha256: String,
    decodes: Vec<serde_json::Value>,
}

#[derive(Clone, Debug)]
struct WavInfo {
    wav_path: PathBuf,
    wav_sha256: String,
    jt9_count: u32,
    /// From manifest snapshot (curate-time) or freshly decoded if missing.
    pancetta_decode_count: Option<u32>,
    /// Mean SNR from snapshot (None if not in any snapshot).
    snapshot_mean_snr_db: Option<f64>,
    /// Noise floor from snapshot (placeholder 0.0 if not present).
    snapshot_noise_floor_db: Option<f64>,
}

fn load_baselines(dir: &Path) -> anyhow::Result<Vec<WavInfo>> {
    let mut out = Vec::new();
    let mut bad = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.extension().map_or(true, |e| e != "json") {
            continue;
        }
        match std::fs::read_to_string(&p) {
            Ok(s) => match serde_json::from_str::<BaselineCache>(&s) {
                Ok(b) => out.push(WavInfo {
                    wav_path: b.wav_path,
                    wav_sha256: b.wav_sha256,
                    jt9_count: b.decodes.len() as u32,
                    pancetta_decode_count: None,
                    snapshot_mean_snr_db: None,
                    snapshot_noise_floor_db: None,
                }),
                Err(_) => bad += 1,
            },
            Err(_) => bad += 1,
        }
    }
    if bad > 0 {
        eprintln!("warn: {bad} baseline file(s) failed to parse — skipped");
    }
    Ok(out)
}

fn merge_snapshot(infos: &mut [WavInfo], manifest_path: &Path) -> anyhow::Result<usize> {
    let m = CuratedManifest::load(manifest_path)?;
    let mut by_sha: HashMap<String, &pancetta_research::curated::CuratedEntry> = HashMap::new();
    for e in &m.entries {
        by_sha.entry(e.wav_sha256.clone()).or_insert(e);
    }
    let mut hits = 0usize;
    for info in infos.iter_mut() {
        if info.pancetta_decode_count.is_some() {
            continue;
        }
        if let Some(e) = by_sha.get(&info.wav_sha256) {
            info.pancetta_decode_count = Some(e.score_breakdown.pancetta_decode_count);
            info.snapshot_mean_snr_db = e.score_breakdown.mean_decoded_snr_db;
            info.snapshot_noise_floor_db = Some(e.score_breakdown.noise_floor_db);
            hits += 1;
        }
    }
    Ok(hits)
}

fn decode_count_for_missing(infos: &mut [WavInfo]) -> anyhow::Result<()> {
    let decoder = Ft8Decoder::with_default_config();
    let missing: Vec<usize> = infos
        .iter()
        .enumerate()
        .filter(|(_, i)| i.pancetta_decode_count.is_none())
        .map(|(idx, _)| idx)
        .collect();
    let total = missing.len();
    println!("decoding {total} WAVs not present in any snapshot manifest...");
    for (i, idx) in missing.iter().enumerate() {
        let info = &mut infos[*idx];
        match decoder.decode_wav(&info.wav_path) {
            Ok(decodes) => {
                info.pancetta_decode_count = Some(decodes.len() as u32);
                if !decodes.is_empty() {
                    let sum: f64 = decodes.iter().map(|d| d.snr_db).sum();
                    info.snapshot_mean_snr_db = Some(sum / decodes.len() as f64);
                }
            }
            Err(e) => {
                eprintln!("warn: decode failed for {}: {e}", info.wav_path.display());
                info.pancetta_decode_count = Some(0);
            }
        }
        if (i + 1) % 25 == 0 || i + 1 == total {
            println!("  decoded {}/{}", i + 1, total);
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;

    // 1. Load all baselines.
    let mut infos = load_baselines(&args.baselines_dir)?;
    println!(
        "loaded {} baselines from {}",
        infos.len(),
        args.baselines_dir.display()
    );

    // 2. Merge in pancetta_decode_count from each provided manifest snapshot.
    for m in &args.manifest_snapshots {
        let hits = merge_snapshot(&mut infos, m)?;
        println!("  merged {hits} entries from {}", m.display());
    }
    let after_snapshot = infos
        .iter()
        .filter(|i| i.pancetta_decode_count.is_some())
        .count();
    println!(
        "  {} of {} WAVs have a pancetta decode count after snapshot merge",
        after_snapshot,
        infos.len()
    );

    // 3. Decode the remaining WAVs with the current default-config decoder.
    decode_count_for_missing(&mut infos)?;

    // 4. Score: jt9-novel-density = jt9_count - pancetta_decode_count.
    //    Only retain WAVs with jt9_count > 0 (otherwise the gap is degenerate).
    let mut scored: Vec<(WavInfo, i64)> = infos
        .into_iter()
        .filter(|i| i.jt9_count > 0)
        .map(|i| {
            let gap = i.jt9_count as i64 - i.pancetta_decode_count.unwrap_or(0) as i64;
            (i, gap)
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));

    let top: Vec<&(WavInfo, i64)> = scored.iter().take(args.top_n).collect();
    if top.is_empty() {
        anyhow::bail!("no WAVs with jt9 decodes found — cannot build tier");
    }
    let gap_max = top.first().map(|(_, g)| *g).unwrap_or(0);
    let gap_min = top.last().map(|(_, g)| *g).unwrap_or(0);
    let gap_mean: f64 = top.iter().map(|(_, g)| *g as f64).sum::<f64>() / top.len() as f64;
    println!(
        "selected top-{} WAVs by jt9-novel-density: gap range {}..{}, mean {:.2}",
        top.len(),
        gap_min,
        gap_max,
        gap_mean,
    );

    // 5. Emit manifest.
    let entries: Vec<CuratedEntry> = top
        .iter()
        .map(|(info, gap)| CuratedEntry {
            wav_path: info.wav_path.clone(),
            wav_sha256: info.wav_sha256.clone(),
            // interest_score = the gap itself (so higher means jt9 finds more).
            // Independent of the standard curate scoring formula.
            interest_score: *gap as f64,
            score_breakdown: ScoreBreakdown {
                pancetta_decode_count: info.pancetta_decode_count.unwrap_or(0),
                noise_floor_db: info.snapshot_noise_floor_db.unwrap_or(0.0),
                mean_decoded_snr_db: info.snapshot_mean_snr_db,
            },
        })
        .collect();

    let manifest = CuratedManifest {
        schema_version: CuratedManifest::CURRENT_SCHEMA_VERSION,
        label: "hard_jt9_rich_200".to_string(),
        generated_at: Utc::now().to_rfc3339(),
        scoring_decoder: format!("pancetta-ft8@default (jt9-novel-density curation, hb-150)"),
        entries,
    };
    manifest.save(&args.output)?;
    println!(
        "wrote {} entries to {}",
        manifest.entries.len(),
        args.output.display()
    );
    Ok(())
}
