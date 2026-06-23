//! hb-064 Session 3 — expand the trajectory dataset to ≥50k samples by
//! widening the WAV pool to the full curated tiers (hard_200,
//! hard_jt9_rich_200, chrono_replay) on top of fixtures + synth-clean.
//!
//! Lesson from hb-194 S2 (Wortsman §3.1): weight-space averaging
//! requires a shared init basin. Session 3's training script fine-tunes
//! the production single-model rather than training from scratch — that
//! requires a meaningfully larger dataset to avoid the 545-positive
//! overfit of Session 2.
//!
//! Run:
//!   cargo run --release -p pancetta-research \
//!     --example hb064_generate_trajectory_dataset_s3
//!
//! Output:
//!   research/experiments/2026-06-02-hb-064-session3/
//!     ├── trajectories.jsonl    — one sample per line
//!     ├── summary.json          — corpus + distinguishability stats
//!     └── README.txt            — usage notes

use anyhow::Context;
use pancetta_ft8::bp_trajectory_capture as bptc;
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// One JSONL row per captured (BP failure → OSD invocation) event.
#[derive(Serialize)]
struct TrajectoryRecord {
    wav: String,
    tier: String,
    channel_llrs: Vec<f32>,
    trajectory_flat: Vec<f32>,
    final_llrs: Vec<f32>,
    osd_recovered: bool,
    osd_codeword: Option<Vec<u8>>,
    bp_iters_run: u16,
    features: TrajectoryFeatures,
}

#[derive(Serialize, Clone, Copy, Debug)]
struct TrajectoryFeatures {
    final_mean_abs_llr: f32,
    final_stddev_abs_llr: f32,
    early_mean_abs_llr: f32,
    growth_ratio: f32,
    max_consecutive_sign_flips: u32,
    total_sign_flips: u32,
    parity_errors_final: u32,
}

#[derive(Serialize, Default)]
struct Summary {
    n_samples: usize,
    n_osd_recovered: usize,
    n_osd_failed: usize,
    wavs_decoded: usize,
    per_tier_counts: serde_json::Value,
}

fn main() -> anyhow::Result<()> {
    let repo_root = repo_root()?;
    let out_dir = repo_root.join("research/experiments/2026-06-02-hb-064-session3");
    std::fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;
    let jsonl_path = out_dir.join("trajectories.jsonl");
    let summary_path = out_dir.join("summary.json");
    let readme_path = out_dir.join("README.txt");

    let pool = build_wav_pool(&repo_root)?;
    eprintln!(
        "hb-064 Session 3 dataset gen: {} WAVs across {} tiers",
        pool.len(),
        pool.iter()
            .map(|w| w.tier.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );

    let mut writer = BufWriter::new(File::create(&jsonl_path)?);
    let start = Instant::now();
    let mut n_total = 0usize;
    let mut n_recovered = 0usize;
    let mut n_failed = 0usize;
    let mut per_tier: std::collections::BTreeMap<String, (usize, usize, usize)> =
        Default::default();

    for (idx, entry) in pool.iter().enumerate() {
        if idx % 25 == 0 {
            eprintln!(
                "  [{:>4}/{}] {} (tier={})  total samples so far: {}",
                idx + 1,
                pool.len(),
                entry.path.display(),
                entry.tier,
                n_total
            );
        }
        let records = decode_with_capture(&entry.path, &entry.tier)?;
        let mut tier_rec = 0usize;
        let mut tier_fail = 0usize;
        for rec in &records {
            if rec.osd_recovered {
                tier_rec += 1;
                n_recovered += 1;
            } else {
                tier_fail += 1;
                n_failed += 1;
            }
            n_total += 1;
            serde_json::to_writer(&mut writer, rec)?;
            writer.write_all(b"\n")?;
        }
        let counters = per_tier.entry(entry.tier.clone()).or_default();
        counters.0 += 1;
        counters.1 += tier_rec;
        counters.2 += tier_fail;
    }
    writer.flush()?;

    let per_tier_json: serde_json::Value = serde_json::Value::Object(
        per_tier
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    serde_json::json!({
                        "wavs": v.0,
                        "n_recovered": v.1,
                        "n_failed": v.2,
                    }),
                )
            })
            .collect(),
    );

    let summary = Summary {
        n_samples: n_total,
        n_osd_recovered: n_recovered,
        n_osd_failed: n_failed,
        wavs_decoded: pool.len(),
        per_tier_counts: per_tier_json,
    };
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)?;
    std::fs::write(
        &readme_path,
        format!(
            "hb-064 Session 3 dataset\n\
            ========================\n\
            Generated in: {elapsed:?}\n\
            WAVs decoded: {wavs}\n\
            Captured trajectories: {n_total}\n\
            OSD-recovered: {n_recovered}\n\
            OSD-failed: {n_failed}\n",
            elapsed = start.elapsed(),
            wavs = pool.len(),
            n_total = n_total,
            n_recovered = n_recovered,
            n_failed = n_failed,
        ),
    )?;

    eprintln!("\n=== hb-064 Session 3 dataset ===");
    eprintln!("  WAVs decoded: {}", pool.len());
    eprintln!("  Total samples: {}", n_total);
    eprintln!("  OSD recovered: {}", n_recovered);
    eprintln!("  OSD failed: {}", n_failed);
    eprintln!("  Elapsed: {:?}", start.elapsed());
    eprintln!("\nOutputs:");
    eprintln!("  {}", jsonl_path.display());
    eprintln!("  {}", summary_path.display());

    Ok(())
}

struct PoolEntry {
    path: PathBuf,
    tier: String,
}

fn build_wav_pool(repo_root: &Path) -> anyhow::Result<Vec<PoolEntry>> {
    let mut pool: Vec<PoolEntry> = Vec::new();

    // Tier 1: ft8_lib fixtures.
    let fixtures = repo_root.join("pancetta-ft8/tests/fixtures/wav");
    for sub in ["basicft8", "wsjt"] {
        let dir = fixtures.join(sub);
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir)? {
                let e = entry?;
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("wav") {
                    pool.push(PoolEntry {
                        path: p,
                        tier: format!("fixtures-{sub}"),
                    });
                }
            }
        }
    }

    // Tier 2: synth-clean. Same 40-WAV selection as Session 1 for
    // continuity.
    let synth = repo_root.join("research/corpus/synth/wavs/clean");
    if synth.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&synth)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wav"))
            .collect();
        entries.sort();
        for p in entries.into_iter().take(40) {
            pool.push(PoolEntry {
                path: p,
                tier: "synth-clean".into(),
            });
        }
    }

    // Tier 3-5: curated hard tiers. Each ~200-300 WAVs. Use the full
    // tier (no truncation) — Session 3 needs the volume.
    for (tier, file) in [
        ("hard-200", "hard_200.manifest.json"),
        ("hard-jt9-rich-200", "hard_jt9_rich_200.manifest.json"),
        ("chrono-replay", "chrono_replay.manifest.json"),
    ] {
        let manifest = repo_root.join("research/corpus/curated/ft8").join(file);
        if manifest.is_file() {
            let v: serde_json::Value = serde_json::from_reader(File::open(&manifest)?)?;
            if let Some(entries) = v["entries"].as_array() {
                for entry in entries {
                    if let Some(wav_path) = entry["wav_path"].as_str() {
                        let p = PathBuf::from(wav_path);
                        if p.is_file() {
                            pool.push(PoolEntry {
                                path: p,
                                tier: tier.into(),
                            });
                        }
                    }
                }
            }
        }
    }

    if pool.is_empty() {
        anyhow::bail!(
            "no WAVs found — corpus likely missing under {}/research/corpus",
            repo_root.display()
        );
    }

    Ok(pool)
}

fn decode_with_capture(wav: &Path, tier: &str) -> anyhow::Result<Vec<TrajectoryRecord>> {
    let mut reader =
        hound::WavReader::open(wav).with_context(|| format!("open {}", wav.display()))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12_000,
        "WAV {} not 12kHz mono",
        wav.display()
    );
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
    };

    let config = pancetta_ft8::Ft8Config::default();
    let mut decoder = pancetta_ft8::Ft8Decoder::new(config)
        .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;

    bptc::enable_local();
    let _ = decoder.decode_window(&samples);
    bptc::disable_local();
    let captured = bptc::drain_local();

    let wav_display = wav
        .strip_prefix(repo_root().unwrap_or_default())
        .unwrap_or(wav)
        .display()
        .to_string();

    let mut out = Vec::with_capacity(captured.len());
    for sample in captured {
        let features = compute_features(&sample);
        let mut trajectory_flat = Vec::with_capacity(25 * 174);
        for row in sample.trajectory.iter() {
            trajectory_flat.extend_from_slice(row);
        }
        out.push(TrajectoryRecord {
            wav: wav_display.clone(),
            tier: tier.to_string(),
            channel_llrs: sample.channel_llrs.to_vec(),
            trajectory_flat,
            final_llrs: sample.final_llrs.to_vec(),
            osd_recovered: sample.osd_recovered,
            osd_codeword: sample.osd_codeword.map(|c| c.to_vec()),
            bp_iters_run: sample.bp_iters_run,
            features,
        });
    }
    Ok(out)
}

fn compute_features(sample: &bptc::CapturedTrajectory) -> TrajectoryFeatures {
    let final_iter = (sample.bp_iters_run as usize).saturating_sub(1).min(24);

    let final_abs: Vec<f32> = sample.final_llrs.iter().map(|v| v.abs()).collect();
    let final_mean = mean(&final_abs);
    let final_var = variance(&final_abs, final_mean);
    let final_stddev = final_var.sqrt();

    let early_abs: Vec<f32> = sample.trajectory[0].iter().map(|v| v.abs()).collect();
    let early_mean = mean(&early_abs);
    let growth_ratio = if early_mean > 1e-6 {
        final_mean / early_mean
    } else {
        0.0
    };

    let mut max_flips: u32 = 0;
    let mut total_flips: u32 = 0;
    for it in 0..final_iter {
        let a = &sample.trajectory[it];
        let b = &sample.trajectory[it + 1];
        let mut flips: u32 = 0;
        for bit in 0..174 {
            let sa = a[bit] >= 0.0;
            let sb = b[bit] >= 0.0;
            if sa != sb {
                flips += 1;
            }
        }
        max_flips = max_flips.max(flips);
        total_flips += flips;
    }

    let parity_errors = count_parity_errors_approx(&sample.final_llrs);

    TrajectoryFeatures {
        final_mean_abs_llr: final_mean,
        final_stddev_abs_llr: final_stddev,
        early_mean_abs_llr: early_mean,
        growth_ratio,
        max_consecutive_sign_flips: max_flips,
        total_sign_flips: total_flips,
        parity_errors_final: parity_errors,
    }
}

fn count_parity_errors_approx(final_llrs: &[f32; 174]) -> u32 {
    let bits: [u8; 174] = std::array::from_fn(|i| u8::from(final_llrs[i] < 0.0));
    let mut errs = 0u32;
    for row in LDPC_NM.iter() {
        let mut acc = 0u8;
        for &v in row {
            if v == 0 {
                break;
            }
            acc ^= bits[v - 1];
        }
        if acc != 0 {
            errs += 1;
        }
    }
    errs
}

fn mean(xs: &[f32]) -> f32 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f32>() / xs.len() as f32
    }
}

fn variance(xs: &[f32], mean: f32) -> f32 {
    if xs.len() < 2 {
        0.0
    } else {
        xs.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / (xs.len() - 1) as f32
    }
}

fn repo_root() -> anyhow::Result<PathBuf> {
    let mut cur = std::env::current_dir().context("cwd")?;
    loop {
        let cargo = cur.join("Cargo.toml");
        if cargo.is_file() {
            let s = std::fs::read_to_string(&cargo).unwrap_or_default();
            if s.contains("[workspace]") {
                return Ok(cur);
            }
        }
        if !cur.pop() {
            anyhow::bail!("could not locate workspace root from cwd");
        }
    }
}

#[rustfmt::skip]
const LDPC_NM: [[usize; 7]; 83] = [
    [4, 31, 59, 91, 92, 96, 153],
    [5, 32, 60, 93, 115, 146, 0],
    [6, 24, 61, 94, 122, 151, 0],
    [7, 33, 62, 95, 96, 143, 0],
    [8, 25, 63, 83, 93, 96, 148],
    [6, 32, 64, 97, 126, 138, 0],
    [5, 34, 65, 78, 98, 107, 154],
    [9, 35, 66, 99, 139, 146, 0],
    [10, 36, 67, 100, 107, 126, 0],
    [11, 37, 67, 87, 101, 139, 158],
    [12, 38, 68, 102, 105, 155, 0],
    [13, 39, 69, 103, 149, 162, 0],
    [8, 40, 70, 82, 104, 114, 145],
    [14, 41, 71, 88, 102, 123, 156],
    [15, 42, 59, 106, 123, 159, 0],
    [1, 33, 72, 106, 107, 157, 0],
    [16, 43, 73, 108, 141, 160, 0],
    [17, 37, 74, 81, 109, 131, 154],
    [11, 44, 75, 110, 121, 166, 0],
    [45, 55, 64, 111, 130, 161, 173],
    [8, 46, 71, 112, 119, 166, 0],
    [18, 36, 76, 89, 113, 114, 143],
    [19, 38, 77, 104, 116, 163, 0],
    [20, 47, 70, 92, 138, 165, 0],
    [2, 48, 74, 113, 128, 160, 0],
    [21, 45, 78, 83, 117, 121, 151],
    [22, 47, 58, 118, 127, 164, 0],
    [16, 39, 62, 112, 134, 158, 0],
    [23, 43, 79, 120, 131, 145, 0],
    [19, 35, 59, 73, 110, 125, 161],
    [20, 36, 63, 94, 136, 161, 0],
    [14, 31, 79, 98, 132, 164, 0],
    [3, 44, 80, 124, 127, 169, 0],
    [19, 46, 81, 117, 135, 167, 0],
    [7, 49, 58, 90, 100, 105, 168],
    [12, 50, 61, 118, 119, 144, 0],
    [13, 51, 64, 114, 118, 157, 0],
    [24, 52, 76, 129, 148, 149, 0],
    [25, 53, 69, 90, 101, 130, 156],
    [20, 46, 65, 80, 120, 140, 170],
    [21, 54, 77, 100, 140, 171, 0],
    [35, 82, 133, 142, 171, 174, 0],
    [14, 30, 83, 113, 125, 170, 0],
    [4, 29, 68, 120, 134, 173, 0],
    [1, 4, 52, 57, 86, 136, 152],
    [26, 51, 56, 91, 122, 137, 168],
    [52, 84, 110, 115, 145, 168, 0],
    [7, 50, 81, 99, 132, 173, 0],
    [23, 55, 67, 95, 172, 174, 0],
    [26, 41, 77, 109, 141, 148, 0],
    [2, 27, 41, 61, 62, 115, 133],
    [27, 40, 56, 124, 125, 126, 0],
    [18, 49, 55, 124, 141, 167, 0],
    [6, 33, 85, 108, 116, 156, 0],
    [28, 48, 70, 85, 105, 129, 158],
    [9, 54, 63, 131, 147, 155, 0],
    [22, 53, 68, 109, 121, 174, 0],
    [3, 13, 48, 78, 95, 123, 0],
    [31, 69, 133, 150, 155, 169, 0],
    [12, 43, 66, 89, 97, 135, 159],
    [5, 39, 75, 102, 136, 167, 0],
    [2, 54, 86, 101, 135, 164, 0],
    [15, 56, 87, 108, 119, 171, 0],
    [10, 44, 82, 91, 111, 144, 149],
    [23, 34, 71, 94, 127, 153, 0],
    [11, 49, 88, 92, 142, 157, 0],
    [29, 34, 87, 97, 147, 162, 0],
    [30, 50, 60, 86, 137, 142, 162],
    [10, 53, 66, 84, 112, 128, 165],
    [22, 57, 85, 93, 140, 159, 0],
    [28, 32, 72, 103, 132, 166, 0],
    [28, 29, 84, 88, 117, 143, 150],
    [1, 26, 45, 80, 128, 147, 0],
    [17, 27, 89, 103, 116, 153, 0],
    [51, 57, 98, 163, 165, 172, 0],
    [21, 37, 73, 138, 152, 169, 0],
    [16, 47, 76, 130, 137, 154, 0],
    [3, 24, 30, 72, 104, 139, 0],
    [9, 40, 90, 106, 134, 151, 0],
    [15, 58, 60, 74, 111, 150, 163],
    [18, 42, 79, 144, 146, 152, 0],
    [25, 38, 65, 99, 122, 160, 0],
    [17, 42, 75, 129, 170, 172, 0],
];
