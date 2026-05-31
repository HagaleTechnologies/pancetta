//! hb-064 Session 1 — generate BP trajectory dataset + run the
//! distinguishability diagnostic.
//!
//! For each WAV in a small mixed pool (synth-clean tier + ft8_lib
//! fixtures + a handful of hard-200 entries), enable per-thread BP
//! trajectory capture, run the production-config decoder, drain the
//! captured (channel_llrs, trajectory, final_llrs, OSD-outcome)
//! samples to a JSONL file, and aggregate statistics.
//!
//! Then run the **kill-switch diagnostic** specified in the hb-064
//! brief: for the captured OSD-eligible BP failures, measure what
//! fraction has a clearly-distinguishable trajectory signature
//! separating OSD-recovered from OSD-failed cases. If <20% have
//! distinguishable signatures, the brief instructs SHELVE.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb064_generate_trajectory_dataset
//!
//! Output:
//!   research/experiments/2026-05-31-hb-064-dia-osd-session1/
//!     ├── trajectories.jsonl    — one sample per line
//!     ├── summary.json          — corpus + distinguishability stats
//!     └── README.txt            — usage notes
//!
//! Costs: ~minutes on a small mixed pool (≈50-100 WAVs). Each WAV
//! produces 0-30 BP-failure samples; expect a few thousand total.

use anyhow::Context;
use pancetta_ft8::bp_trajectory_capture as bptc;
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Maximum hard-200 WAVs to sample from (deterministic head). Kept
/// small so Session 1 runs quickly; Session 2 can rerun against the
/// full corpus once the model architecture is fixed.
const HARD_200_SAMPLE: usize = 25;

/// One JSONL row per captured (BP failure → OSD invocation) event.
#[derive(Serialize)]
struct TrajectoryRecord {
    /// WAV the trajectory came from (relative when possible).
    wav: String,
    /// Tier label for downstream stratification.
    tier: String,
    /// Channel LLRs (pre-BP, post-normalization). Length 174.
    channel_llrs: Vec<f32>,
    /// Per-iteration BP LLRs flattened row-major as [25 × 174]
    /// (`trajectory[iter * 174 + bit]`).
    trajectory_flat: Vec<f32>,
    /// Final-iteration LLRs. Length 174.
    final_llrs: Vec<f32>,
    /// True iff OSD found a CRC-valid codeword.
    osd_recovered: bool,
    /// CRC-valid codeword when `osd_recovered`. Length 174 (0/1).
    osd_codeword: Option<Vec<u8>>,
    /// BP iterations actually run before exit.
    bp_iters_run: u16,
    /// hb-064 features computed online (also re-computable from raw
    /// trajectory at any time; cached here for fast post-hoc analysis).
    features: TrajectoryFeatures,
}

/// Per-trajectory summary features the diagnostic operates over. These
/// are the candidate "distinguishability signatures" the paper relies
/// on; if they don't separate OSD-recovered from OSD-failed cases,
/// the data carries no signal and the model will not learn one.
#[derive(Serialize, Clone, Copy, Debug)]
struct TrajectoryFeatures {
    /// Mean |LLR| at final iteration. High = confident posterior.
    final_mean_abs_llr: f32,
    /// Stddev of |LLR| at final iteration. High = uneven confidence —
    /// the paper's "trapping-set" signature.
    final_stddev_abs_llr: f32,
    /// Mean |LLR| at iteration 0 (or 1 — see init notes in code).
    early_mean_abs_llr: f32,
    /// Ratio final/early mean |LLR|. <1 indicates BP REGRESSED
    /// (posterior softened) — the paper's strongest trapping-set
    /// indicator.
    growth_ratio: f32,
    /// Maximum number of bits that flipped sign between any two
    /// consecutive iterations. The paper reports this peaks for
    /// hard-to-decode trapping-set patterns.
    max_consecutive_sign_flips: u32,
    /// Total sign flips summed across the whole 24 iter→iter pairs.
    /// Captures total instability.
    total_sign_flips: u32,
    /// Final-iter hard-decision parity error count (0..83). Tighter
    /// bound than the parity gate; available for free.
    parity_errors_final: u32,
}

#[derive(Serialize, Default)]
struct DistinguishabilitySummary {
    n_samples: usize,
    n_osd_recovered: usize,
    n_osd_failed: usize,
    /// Per-feature mean for recovered & failed pools, plus a
    /// separability score (|mean_diff| / sqrt(var_a + var_b)) — a
    /// Welch-like d' statistic. d' > ~0.5 suggests the feature carries
    /// usable training signal.
    feature_separability: Vec<FeatureStats>,
    /// Fraction of samples that are "clearly distinguishable" by AT
    /// LEAST ONE feature exceeding |d'| = 0.5. The brief's gate.
    distinguishable_fraction: f64,
    distinguishable_gate: f64,
    decision: String,
    notes: String,
}

#[derive(Serialize)]
struct FeatureStats {
    name: String,
    mean_recovered: f64,
    mean_failed: f64,
    stddev_recovered: f64,
    stddev_failed: f64,
    /// Welch-like d' on this feature.
    d_prime: f64,
}

fn main() -> anyhow::Result<()> {
    let repo_root = repo_root()?;
    let out_dir = repo_root.join("research/experiments/2026-05-31-hb-064-dia-osd-session1");
    std::fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;
    let jsonl_path = out_dir.join("trajectories.jsonl");
    let summary_path = out_dir.join("summary.json");
    let readme_path = out_dir.join("README.txt");

    let pool = build_wav_pool(&repo_root)?;
    eprintln!(
        "hb-064 Session 1 dataset gen: {} WAVs across {} tiers",
        pool.len(),
        pool.iter()
            .map(|w| w.tier.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );

    let mut writer = BufWriter::new(File::create(&jsonl_path)?);
    let mut all_records: Vec<TrajectoryRecord> = Vec::new();
    let start = Instant::now();

    for (idx, entry) in pool.iter().enumerate() {
        if idx % 10 == 0 {
            eprintln!(
                "  [{:>3}/{}] {} (tier={})",
                idx + 1,
                pool.len(),
                entry.path.display(),
                entry.tier
            );
        }
        let records = decode_with_capture(&entry.path, &entry.tier)?;
        for rec in records {
            serde_json::to_writer(&mut writer, &rec)?;
            writer.write_all(b"\n")?;
            all_records.push(rec);
        }
    }
    writer.flush()?;

    let summary = run_diagnostic(&all_records);
    let json = serde_json::to_string_pretty(&summary)?;
    std::fs::write(&summary_path, json)?;
    let readme = format!(
        "hb-064 Session 1 dataset\n\
        =========================\n\
        Generated: {start:?}\n\
        WAVs: {n_wavs}\n\
        Captured BP failures (OSD-eligible): {n_samples}\n\
        OSD recovered: {n_recovered}\n\
        OSD failed: {n_failed}\n\
        Distinguishable fraction: {dist_pct:.1}%\n\
        Decision: {decision}\n\n\
        See ../{rel_summary}\n",
        start = start.elapsed(),
        n_wavs = pool.len(),
        n_samples = summary.n_samples,
        n_recovered = summary.n_osd_recovered,
        n_failed = summary.n_osd_failed,
        dist_pct = summary.distinguishable_fraction * 100.0,
        decision = summary.decision,
        rel_summary = "summary.json",
    );
    std::fs::write(&readme_path, readme)?;

    eprintln!("\n=== hb-064 Session 1 diagnostic ===");
    eprintln!("  N samples: {}", summary.n_samples);
    eprintln!("  OSD recovered: {}", summary.n_osd_recovered);
    eprintln!("  OSD failed: {}", summary.n_osd_failed);
    eprintln!(
        "  Distinguishable fraction: {:.1}% (gate {:.0}%)",
        summary.distinguishable_fraction * 100.0,
        summary.distinguishable_gate * 100.0
    );
    eprintln!("  Decision: {}", summary.decision);
    eprintln!("\nOutputs:");
    eprintln!("  {}", jsonl_path.display());
    eprintln!("  {}", summary_path.display());
    eprintln!("  {}", readme_path.display());

    Ok(())
}

struct PoolEntry {
    path: PathBuf,
    tier: String,
}

fn build_wav_pool(repo_root: &Path) -> anyhow::Result<Vec<PoolEntry>> {
    let mut pool: Vec<PoolEntry> = Vec::new();

    // Tier 1: ft8_lib fixtures (small, fast, deterministic). Skip
    // generated/encoded ones since those should converge in BP — we
    // need failures.
    let fixtures = repo_root.join("pancetta-ft8/tests/fixtures/wav");
    let basicft8 = fixtures.join("basicft8");
    if basicft8.is_dir() {
        for entry in std::fs::read_dir(&basicft8)? {
            let e = entry?;
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("wav") {
                pool.push(PoolEntry {
                    path: p,
                    tier: "fixtures-basicft8".into(),
                });
            }
        }
    }
    let wsjt = fixtures.join("wsjt");
    if wsjt.is_dir() {
        for entry in std::fs::read_dir(&wsjt)? {
            let e = entry?;
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("wav") {
                pool.push(PoolEntry {
                    path: p,
                    tier: "fixtures-wsjt".into(),
                });
            }
        }
    }

    // Tier 2: synth-clean — small, fast, controlled SNR sweep. Good
    // for confirming the trajectory pipeline behaves on known truth.
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

    // Tier 3: hard-200 head — real K5ARH recordings, multiple OSD
    // failures per WAV. The interesting signal lives here.
    let hard_200 = repo_root.join("research/corpus/curated/ft8/hard_200.manifest.json");
    if hard_200.is_file() {
        let manifest: serde_json::Value = serde_json::from_reader(File::open(&hard_200)?)?;
        if let Some(entries) = manifest["entries"].as_array() {
            for entry in entries.iter().take(HARD_200_SAMPLE) {
                if let Some(wav_path) = entry["wav_path"].as_str() {
                    let p = PathBuf::from(wav_path);
                    if p.is_file() {
                        pool.push(PoolEntry {
                            path: p,
                            tier: "hard-200".into(),
                        });
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

    // Use iter 0 (first recorded) as the "early" baseline.
    let early_abs: Vec<f32> = sample.trajectory[0].iter().map(|v| v.abs()).collect();
    let early_mean = mean(&early_abs);
    let growth_ratio = if early_mean > 1e-6 {
        final_mean / early_mean
    } else {
        0.0
    };

    // Sign-flip counts across consecutive iterations 0..final_iter.
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

    // Hard-decision parity error count from final LLRs against the
    // FT8 parity matrix. Reuse the existing pancetta-ft8 helper via
    // an inline sparse-row check. We don't have public access; do a
    // lightweight ad-hoc count using the well-known LDPC_NM rows
    // hard-coded for the diagnostic — the magnitude is what matters,
    // not bit-exactness with the production count.
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
    // Hard-decision bits: positive LLR = 0, negative = 1.
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

fn run_diagnostic(records: &[TrajectoryRecord]) -> DistinguishabilitySummary {
    let mut summary = DistinguishabilitySummary {
        distinguishable_gate: 0.20,
        ..Default::default()
    };
    summary.n_samples = records.len();
    summary.n_osd_recovered = records.iter().filter(|r| r.osd_recovered).count();
    summary.n_osd_failed = records.iter().filter(|r| !r.osd_recovered).count();

    if summary.n_osd_recovered == 0 || summary.n_osd_failed == 0 {
        summary.decision = "INCONCLUSIVE — corpus has only one outcome class".into();
        summary.notes = format!(
            "Need both recovered ({}) and failed ({}) samples to compute separability. \
             Try expanding the WAV pool.",
            summary.n_osd_recovered, summary.n_osd_failed
        );
        return summary;
    }

    let mut stats: Vec<FeatureStats> = Vec::new();
    for (name, extract) in feature_extractors() {
        let rec: Vec<f64> = records
            .iter()
            .filter(|r| r.osd_recovered)
            .map(|r| extract(&r.features))
            .collect();
        let fail: Vec<f64> = records
            .iter()
            .filter(|r| !r.osd_recovered)
            .map(|r| extract(&r.features))
            .collect();
        let m_r = mean_f64(&rec);
        let s_r = stddev_f64(&rec, m_r);
        let m_f = mean_f64(&fail);
        let s_f = stddev_f64(&fail, m_f);
        let denom = (s_r.powi(2) + s_f.powi(2)).sqrt();
        let d_prime = if denom > 1e-9 {
            (m_r - m_f).abs() / denom
        } else {
            0.0
        };
        stats.push(FeatureStats {
            name: name.into(),
            mean_recovered: m_r,
            mean_failed: m_f,
            stddev_recovered: s_r,
            stddev_failed: s_f,
            d_prime,
        });
    }

    // Per-sample distinguishability: count a sample as "distinguishable"
    // if its feature vector falls in a region where at least one
    // feature has d' >= 0.5 (rough effect-size threshold). To make
    // this concrete: for each feature with d' >= 0.5, compute the
    // midpoint between class means; a sample is "called" by that
    // feature if it sits on its true side. Count samples that are
    // correctly called by >=1 high-d' feature. This is the brief's
    // proxy for "trajectory carries a learnable signal" — without
    // building a model.
    let mut hi_d: Vec<(usize, f64, &FeatureStats)> = Vec::new();
    for (i, fs) in stats.iter().enumerate() {
        if fs.d_prime >= 0.5 {
            let mid = (fs.mean_recovered + fs.mean_failed) / 2.0;
            hi_d.push((i, mid, fs));
        }
    }

    let extractors = feature_extractors();
    let extractors_vec: Vec<fn(&TrajectoryFeatures) -> f64> =
        extractors.iter().map(|(_, e)| *e).collect();

    let mut distinguishable = 0usize;
    for rec in records {
        let mut called_correctly = false;
        for (i, mid, fs) in hi_d.iter() {
            let val = extractors_vec[*i](&rec.features);
            // Decide which side of the midpoint corresponds to
            // "recovered" by checking sign of (mean_recovered - mid).
            let recovered_side_is_above = fs.mean_recovered > *mid;
            let predicted_recovered = if recovered_side_is_above {
                val > *mid
            } else {
                val < *mid
            };
            if predicted_recovered == rec.osd_recovered {
                called_correctly = true;
                break;
            }
        }
        if called_correctly {
            distinguishable += 1;
        }
    }

    summary.feature_separability = stats;
    summary.distinguishable_fraction = if summary.n_samples > 0 {
        distinguishable as f64 / summary.n_samples as f64
    } else {
        0.0
    };

    summary.decision = if summary.distinguishable_fraction >= summary.distinguishable_gate {
        "PROCEED — trajectory signature distinguishes >=20% of OSD-eligible BP failures; \
         Session 2 should train the trajectory-aware model"
            .into()
    } else {
        "SHELVE — distinguishable fraction below 20% gate; the paper's TEP-pruning gains \
         won't translate to pancetta's BP-failure signal class. Production OSD remains \
         the right tool."
            .into()
    };

    if summary.n_samples < 200 {
        summary.notes = format!(
            "Small N ({}) — diagnostic is exploratory; rerun with HARD_200_SAMPLE expanded \
             before treating decision as final.",
            summary.n_samples
        );
    }

    summary
}

fn feature_extractors() -> Vec<(&'static str, fn(&TrajectoryFeatures) -> f64)> {
    vec![
        ("final_mean_abs_llr", |f| f.final_mean_abs_llr as f64),
        ("final_stddev_abs_llr", |f| f.final_stddev_abs_llr as f64),
        ("early_mean_abs_llr", |f| f.early_mean_abs_llr as f64),
        ("growth_ratio", |f| f.growth_ratio as f64),
        ("max_consecutive_sign_flips", |f| {
            f.max_consecutive_sign_flips as f64
        }),
        ("total_sign_flips", |f| f.total_sign_flips as f64),
        ("parity_errors_final", |f| f.parity_errors_final as f64),
    ]
}

fn mean_f64(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn stddev_f64(xs: &[f64], mean: f64) -> f64 {
    if xs.len() < 2 {
        0.0
    } else {
        (xs.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
    }
}

fn repo_root() -> anyhow::Result<PathBuf> {
    // Walk up from manifest dir until we find Cargo.toml with
    // [workspace]. Robust to running inside worktrees.
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

// LDPC parity-check matrix in sparse 1-origin format, copied from
// `pancetta-ft8/src/ldpc.rs` (and matching the Python data generator).
// Used only by the cheap parity-error feature counter; if it ever
// drifts, the feature value drifts proportionally — magnitude
// monotonicity is preserved.
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
