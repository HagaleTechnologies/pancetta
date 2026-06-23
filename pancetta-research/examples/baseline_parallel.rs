//! baseline_parallel — generate jt9 baselines for a manifest in parallel.
//!
//! Same on-disk format as `pancetta-research --bin baseline`. Skips WAVs
//! whose sha256 already has a cached baseline at
//! `research/baselines/ft8/<sha>.json`. Runs N worker threads (default 6;
//! jt9 itself is single-threaded so this scales linearly to core count).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example baseline_parallel -- \
//!     <manifest-path> [THREADS=6]
//!
//! Environment:
//!   THREADS       — worker thread count (default 6; tune for core count)
//!   JT9_PATH      — override jt9 binary path
//!                   (default: /Applications/wsjtx.app/Contents/MacOS/jt9)

use anyhow::Context;
use pancetta_research::curated::load_curated_corpus;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug, Serialize)]
struct BaselineDecode {
    message: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
}

#[derive(Clone, Debug, Serialize)]
struct BaselineCache {
    schema_version: u32,
    wav_path: String,
    wav_sha256: String,
    decoder_identity: String,
    decodes: Vec<BaselineDecode>,
    elapsed_seconds: f64,
}

fn parse_jt9_line(line: &str) -> Option<BaselineDecode> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let snr: f64 = parts[1].parse().ok()?;
    let dt: f64 = parts[2].parse().ok()?;
    let freq: f64 = parts[3].parse().ok()?;
    if parts[4] != "~" {
        return None;
    }
    let message = parts[5..].join(" ");
    Some(BaselineDecode {
        message,
        freq_hz: freq,
        dt_s: dt,
        snr_db: snr,
    })
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

fn run_jt9(jt9_path: &Path, wav: &Path) -> anyhow::Result<(Vec<BaselineDecode>, f64)> {
    let started = Instant::now();
    let output = Command::new(jt9_path)
        .args(["-8", "-d", "3"])
        .arg(wav)
        .output()
        .with_context(|| format!("running jt9 on {}", wav.display()))?;
    let elapsed = started.elapsed().as_secs_f64();
    if !output.status.success() {
        anyhow::bail!(
            "jt9 failed on {}: {}",
            wav.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let decodes = stdout.lines().filter_map(parse_jt9_line).collect();
    Ok((decodes, elapsed))
}

fn process_one(
    workspace: &Path,
    jt9_path: &Path,
    wav: &Path,
    sha_hint: Option<&str>,
) -> anyhow::Result<(bool, usize)> {
    let sha = if let Some(s) = sha_hint {
        s.to_string()
    } else {
        sha256_file(wav)?
    };
    let out = workspace
        .join("research/baselines/ft8")
        .join(format!("{sha}.json"));
    if out.exists() {
        return Ok((false, 0)); // skipped
    }
    let (decodes, elapsed) = run_jt9(jt9_path, wav)?;
    let cache = BaselineCache {
        schema_version: 1,
        wav_path: wav
            .strip_prefix(workspace)
            .unwrap_or(wav)
            .to_string_lossy()
            .into_owned(),
        wav_sha256: sha,
        decoder_identity: format!("jt9 ({})", jt9_path.display()),
        decodes,
        elapsed_seconds: elapsed,
    };
    let count = cache.decodes.len();
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, serde_json::to_string_pretty(&cache)?)?;
    Ok((true, count))
}

fn main() -> anyhow::Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let manifest_path: PathBuf = args
        .first()
        .map(PathBuf::from)
        .context("usage: baseline_parallel <manifest-path>")?;
    let manifest_path = if manifest_path.is_absolute() {
        manifest_path
    } else {
        workspace.join(manifest_path)
    };

    let jt9_path: PathBuf = std::env::var("JT9_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/Applications/wsjtx.app/Contents/MacOS/jt9"));
    anyhow::ensure!(jt9_path.exists(), "jt9 not found at {}", jt9_path.display());

    let threads: usize = std::env::var("THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);

    let entries = load_curated_corpus(&manifest_path)?;
    println!(
        "baseline_parallel: {} entries from {}, jt9={}, threads={}",
        entries.len(),
        manifest_path.display(),
        jt9_path.display(),
        threads
    );

    let work: Arc<Vec<(PathBuf, String)>> = Arc::new(
        entries
            .into_iter()
            .map(|e| (e.wav_path, e.wav_sha256))
            .collect(),
    );
    let total = work.len();
    let cursor = Arc::new(AtomicUsize::new(0));
    let skipped = Arc::new(AtomicUsize::new(0));
    let generated = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let t_start = Instant::now();

    let mut handles = Vec::new();
    for tid in 0..threads {
        let work_t = Arc::clone(&work);
        let cursor_t = Arc::clone(&cursor);
        let skipped_t = Arc::clone(&skipped);
        let generated_t = Arc::clone(&generated);
        let failed_t = Arc::clone(&failed);
        let workspace_t = workspace.clone();
        let jt9_t = jt9_path.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let idx = cursor_t.fetch_add(1, Ordering::Relaxed);
                if idx >= total {
                    break;
                }
                let (wav, sha) = &work_t[idx];
                match process_one(&workspace_t, &jt9_t, wav, Some(sha)) {
                    Ok((true, n)) => {
                        let g = generated_t.fetch_add(1, Ordering::Relaxed) + 1;
                        if g % 10 == 0 {
                            eprintln!(
                                "[tid={tid}] generated {g} ({n} decodes), {} skipped, {} failed, {}/{total} done",
                                skipped_t.load(Ordering::Relaxed),
                                failed_t.load(Ordering::Relaxed),
                                idx + 1,
                            );
                        }
                    }
                    Ok((false, _)) => {
                        skipped_t.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        failed_t.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "[tid={tid}] FAIL {}: {:#}",
                            wav.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                            e
                        );
                    }
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }

    let elapsed = t_start.elapsed().as_secs_f64();
    let g = generated.load(Ordering::Relaxed);
    let s = skipped.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);
    println!(
        "baseline_parallel: {} generated, {} skipped (cached), {} failed in {:.1}s",
        g, s, f, elapsed
    );
    Ok(())
}
