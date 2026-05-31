//! score_all_today — score every today's WAV in `~/.pancetta/recordings/`
//! (filtered to `ft8_20260530_*.wav`) using the same `interest_score`
//! formula as `curate` (see SCORE_W_* constants below).
//!
//! Output: `research/corpus/surveys/2026-05-30/all_wavs_scored.json` —
//! one JSON object per WAV with sha256, decode_count, mean_snr_db,
//! noise_floor_db, and interest_score. Used as input to the
//! `merge_hard_200` step in the corpus refresh ingestion (see
//! `research/experiments/2026-05-31-corpus-refresh-ingestion.md`).
//!
//! Parallel via N worker threads (default: cpus). Skips files smaller
//! than 300_000 bytes (partial slots).
//!
//! Pancetta is still recording: file-not-found errors are tolerated
//! (file may have been rolled out under our feet); other errors are
//! reported but skipped.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example score_all_today
//!   PANCETTA_RECORDINGS_DIR=/path SAMPLE_PREFIX=ft8_20260530_ THREADS=8 \
//!     cargo run --release -p pancetta-research --example score_all_today

use anyhow::Context;
use pancetta_research::curated::ScoreBreakdown;
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use pancetta_research::noise::estimate_noise_floor_db;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

// Same constants as `pancetta-research/src/bin/curate.rs`.
const SCORE_W_DECODE_COUNT: f64 = 1.0;
const SCORE_W_NOISE_FLOOR: f64 = 0.05;
const SCORE_W_SNR_DIVERSITY: f64 = 0.5;

#[derive(Clone, Debug, Serialize)]
struct ScoredWav {
    wav_path: String,
    wav_sha256: String,
    file_bytes: u64,
    interest_score: f64,
    score_breakdown: ScoreBreakdown,
    /// Slot timestamp parsed from filename (HHMMSS) — used for hour
    /// stratification in wild-100 sampling.
    slot_hhmmss: String,
    error: Option<String>,
}

fn read_wav_samples(path: &Path) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "WAV not 12kHz mono: {}",
        path.display()
    );
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
    };
    Ok(samples)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

fn score_one(decoder: &dyn DecoderUnderTest, path: &Path) -> ScoredWav {
    let wav_path = path.to_string_lossy().into_owned();
    let slot_hhmmss = path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| {
            s.strip_prefix("ft8_20260530_")
                .and_then(|s| s.strip_suffix(".wav"))
        })
        .unwrap_or("")
        .to_string();
    let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    let samples = match read_wav_samples(path) {
        Ok(s) => s,
        Err(e) => {
            return ScoredWav {
                wav_path,
                wav_sha256: String::new(),
                file_bytes,
                interest_score: f64::NAN,
                score_breakdown: ScoreBreakdown::default(),
                slot_hhmmss,
                error: Some(format!("read_wav: {e:#}")),
            };
        }
    };
    let noise = estimate_noise_floor_db(&samples);
    let sha = match sha256_file(path) {
        Ok(s) => s,
        Err(e) => {
            return ScoredWav {
                wav_path,
                wav_sha256: String::new(),
                file_bytes,
                interest_score: f64::NAN,
                score_breakdown: ScoreBreakdown::default(),
                slot_hhmmss,
                error: Some(format!("sha256: {e:#}")),
            };
        }
    };
    let decodes = decoder.decode_wav(path).unwrap_or_default();
    let decode_count = decodes.len() as u32;
    let mean_snr = if decodes.is_empty() {
        None
    } else {
        let sum: f64 = decodes.iter().map(|d| d.snr_db).sum();
        Some(sum / decodes.len() as f64)
    };
    let snr_score = mean_snr.map_or(0.0, |m| (-m / 20.0).max(0.0));
    let score = SCORE_W_DECODE_COUNT * (decode_count as f64)
        + SCORE_W_NOISE_FLOOR * noise
        + SCORE_W_SNR_DIVERSITY * snr_score;

    ScoredWav {
        wav_path,
        wav_sha256: sha,
        file_bytes,
        interest_score: score,
        score_breakdown: ScoreBreakdown {
            pancetta_decode_count: decode_count,
            noise_floor_db: noise,
            mean_decoded_snr_db: mean_snr,
        },
        slot_hhmmss,
        error: None,
    }
}

fn main() -> anyhow::Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf();
    let out_dir = workspace.join("research/corpus/surveys/2026-05-30");
    std::fs::create_dir_all(&out_dir)?;

    let recordings_dir = std::env::var("PANCETTA_RECORDINGS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/thagale".to_string());
            PathBuf::from(home).join(".pancetta/recordings")
        });
    let prefix = std::env::var("SAMPLE_PREFIX").unwrap_or_else(|_| "ft8_20260530_".to_string());

    let mut all_today: Vec<PathBuf> = std::fs::read_dir(&recordings_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with(&prefix) && s.ends_with(".wav"))
                .unwrap_or(false)
        })
        .collect();
    all_today.sort();
    let raw_count = all_today.len();

    // Filter out tiny / partial WAVs (< 300 KB) AND files that may have
    // disappeared since the readdir (rolling-cap-aware).
    all_today.retain(|p| {
        std::fs::metadata(p)
            .map(|m| m.len() >= 300_000)
            .unwrap_or(false)
    });
    println!(
        "Found {} WAVs (prefix={}), kept {} after >=300KB filter",
        raw_count,
        prefix,
        all_today.len(),
    );

    let threads: usize = std::env::var("THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        });
    println!("Scoring with {threads} threads");

    let work: Arc<Vec<PathBuf>> = Arc::new(all_today);
    let cursor = Arc::new(AtomicUsize::new(0));
    let total = work.len();
    let t_start = Instant::now();

    let mut handles: Vec<std::thread::JoinHandle<Vec<ScoredWav>>> = Vec::new();
    for tid in 0..threads {
        let work_t = Arc::clone(&work);
        let cursor_t = Arc::clone(&cursor);
        handles.push(std::thread::spawn(move || {
            let decoder = Ft8Decoder::with_default_config();
            let mut local: Vec<ScoredWav> = Vec::new();
            loop {
                let idx = cursor_t.fetch_add(1, Ordering::Relaxed);
                if idx >= total {
                    break;
                }
                let path = &work_t[idx];
                let r = score_one(&decoder, path);
                // Lightweight progress every 64 (across all threads).
                if idx % 64 == 0 {
                    eprintln!(
                        "[tid={tid}] {}/{total} scored ({:.1}/min) - last={}",
                        idx + 1,
                        (idx + 1) as f64 / t_start.elapsed().as_secs_f64().max(0.01) * 60.0,
                        path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                    );
                }
                local.push(r);
            }
            local
        }));
    }

    let mut all_results: Vec<ScoredWav> = Vec::with_capacity(total);
    for h in handles {
        match h.join() {
            Ok(mut v) => all_results.append(&mut v),
            Err(_) => eprintln!("warn: a worker thread panicked"),
        }
    }
    all_results.sort_by(|a, b| {
        b.interest_score
            .partial_cmp(&a.interest_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let elapsed_s = t_start.elapsed().as_secs_f64();
    let n_ok = all_results.iter().filter(|r| r.error.is_none()).count();
    let n_err = all_results.iter().filter(|r| r.error.is_some()).count();

    let out_path = out_dir.join("all_wavs_scored.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&all_results)?)?;

    println!();
    println!(
        "Scored {}/{} WAVs ({} errors) in {:.1}s ({:.1}/min) → {}",
        n_ok,
        total,
        n_err,
        elapsed_s,
        total as f64 / elapsed_s.max(0.01) * 60.0,
        out_path.display(),
    );
    if let Some(top) = all_results.first() {
        println!(
            "Top score: {:.3} (decodes={} noise={:.1} mean_snr={:?}) — {}",
            top.interest_score,
            top.score_breakdown.pancetta_decode_count,
            top.score_breakdown.noise_floor_db,
            top.score_breakdown.mean_decoded_snr_db,
            top.wav_path,
        );
    }
    if let Some(bot) = all_results
        .iter()
        .rev()
        .find(|r| r.error.is_none() && !r.interest_score.is_nan())
    {
        println!(
            "Bottom score: {:.3} (decodes={}) — {}",
            bot.interest_score, bot.score_breakdown.pancetta_decode_count, bot.wav_path,
        );
    }
    Ok(())
}
