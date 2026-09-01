//! baseline — runs jt9 (WSJT-X CLI) over fixture and synth WAVs;
//! caches decodes to JSON for the eval binary to consume.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin baseline -- \
//!     --tier fixtures --mode ft8

use anyhow::Context;
use pancetta_research::corpus::{load_ft8_fixtures, load_synth_corpus};
use pancetta_research::{BaselineCache, BaselineDecode, Mode};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
struct Args {
    tier: String,
    mode: Mode,
    jt9_path: PathBuf,
    synth_manifest: Option<PathBuf>,
    manifest: Option<PathBuf>,
    force: bool,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut tier: Option<String> = None;
        let mut mode: Option<Mode> = None;
        let mut jt9_path: Option<PathBuf> = None;
        let mut synth_manifest: Option<PathBuf> = None;
        let mut manifest: Option<PathBuf> = None;
        let mut force = false;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--tier" => tier = Some(iter.next().context("--tier needs a value")?),
                "--mode" => {
                    mode = Some(
                        iter.next()
                            .context("--mode needs a value")?
                            .parse::<Mode>()
                            .map_err(|e| anyhow::anyhow!("{e}"))?,
                    );
                }
                "--jt9-path" => {
                    jt9_path = Some(iter.next().context("--jt9-path needs a value")?.into())
                }
                "--synth-manifest" => {
                    synth_manifest = Some(
                        iter.next()
                            .context("--synth-manifest needs a value")?
                            .into(),
                    )
                }
                "--manifest" => {
                    manifest = Some(iter.next().context("--manifest needs a value")?.into())
                }
                "--force" => force = true,
                "-h" | "--help" => {
                    eprintln!("usage: baseline --tier <fixtures|synth|curated-hard-200|curated-hard-1000|wild-50|wild-100|chrono-replay> --mode ft8 [--jt9-path PATH] [--synth-manifest PATH] [--manifest PATH] [--force]");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        // Default jt9 path: try `which jt9` then macOS WSJT-X install path.
        let jt9_path = jt9_path
            .or_else(|| {
                Command::new("which")
                    .arg("jt9")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| PathBuf::from(s.trim()))
                    .filter(|p| p.exists())
            })
            .unwrap_or_else(|| PathBuf::from("/Applications/wsjtx.app/Contents/MacOS/jt9"));
        anyhow::ensure!(
            jt9_path.exists(),
            "jt9 not found at {}; install WSJT-X or pass --jt9-path",
            jt9_path.display()
        );
        Ok(Self {
            tier: tier.context("--tier required")?,
            mode: mode.context("--mode required")?,
            jt9_path,
            synth_manifest,
            manifest,
            force,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Task W0.4 (2026-07-07): the submode marker column (5th field) jt9
/// emits differs per mode — confirmed empirically against the installed
/// WSJT-X binary: FT8 decodes are marked `~`, FT4 decodes `+` (e.g.
/// `000000 -14 -0.1 1552 +  CQ K1AAA FN42`). Not documented in `jt9
/// --help`; verified by running `jt9 -8`/`jt9 -5` over real generated
/// WAVs of each mode during this task.
fn jt9_submode_marker(mode: Mode) -> &'static str {
    match mode {
        Mode::Ft8 => "~",
        Mode::Ft4 => "+",
    }
}

/// Parse one line of jt9 stdout. Typical format: `120000  5  0.4 1500 ~  CQ K1ABC FN42`
/// (FT8) or `000000 -14 -0.1 1552 +  CQ K1AAA FN42` (FT4 — Task W0.4).
/// Returns None for non-decode lines.
fn parse_jt9_line(line: &str, mode: Mode) -> Option<BaselineDecode> {
    // jt9's output starts with a 6-digit time (HHMMSS) followed by SNR, DT, freq, marker, then message.
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    // Expect: time, snr, dt, freq, marker, message...
    let snr: f64 = parts[1].parse().ok()?;
    let dt: f64 = parts[2].parse().ok()?;
    let freq: f64 = parts[3].parse().ok()?;
    if parts[4] != jt9_submode_marker(mode) {
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

/// Task W0.4 (2026-07-07): jt9 mode flag, confirmed against the installed
/// WSJT-X binary's `jt9 --help` (`-8`/`--ft8` for FT8, `-5`/`--ft4` for
/// FT4 — both accept the same `-d DEPTH` decoding-depth flag).
fn jt9_mode_flag(mode: Mode) -> &'static str {
    match mode {
        Mode::Ft8 => "-8",
        Mode::Ft4 => "-5",
    }
}

fn run_jt9(
    jt9_path: &Path,
    wav_path: &Path,
    mode: Mode,
) -> anyhow::Result<(Vec<BaselineDecode>, f64)> {
    let started = std::time::Instant::now();
    let output = Command::new(jt9_path)
        .args([jt9_mode_flag(mode), "-d", "3"])
        .arg(wav_path)
        .output()
        .with_context(|| format!("running {} on {}", jt9_path.display(), wav_path.display()))?;
    let elapsed = started.elapsed().as_secs_f64();
    if !output.status.success() {
        anyhow::bail!(
            "jt9 failed on {}: {}",
            wav_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let decodes = stdout
        .lines()
        .filter_map(|line| parse_jt9_line(line, mode))
        .collect();
    Ok((decodes, elapsed))
}

fn cache_path(workspace: &Path, mode: Mode, wav_hash: &str) -> PathBuf {
    workspace
        .join("research/baselines")
        .join(mode.as_str())
        .join(format!("{wav_hash}.json"))
}

fn process_wav(
    workspace: &Path,
    mode: Mode,
    wav_path: &Path,
    jt9_path: &Path,
    force: bool,
) -> anyhow::Result<()> {
    let wav_sha = sha256_file(wav_path)?;
    let out = cache_path(workspace, mode, &wav_sha);
    if out.exists() && !force {
        println!(
            "baseline: cached  {} -> {}",
            wav_path
                .strip_prefix(workspace)
                .unwrap_or(wav_path)
                .display(),
            out.strip_prefix(workspace).unwrap_or(&out).display(),
        );
        return Ok(());
    }
    let (decodes, elapsed) = run_jt9(jt9_path, wav_path, mode)?;
    let cache = BaselineCache {
        schema_version: BaselineCache::CURRENT_SCHEMA_VERSION,
        wav_path: wav_path
            .strip_prefix(workspace)
            .unwrap_or(wav_path)
            .to_string_lossy()
            .into_owned(),
        wav_sha256: wav_sha,
        decoder_identity: format!("jt9 ({})", jt9_path.display()),
        decodes,
        elapsed_seconds: elapsed,
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, serde_json::to_string_pretty(&cache)?)?;
    println!(
        "baseline: {} decodes from {} ({:.2}s) -> {}",
        cache.decodes.len(),
        wav_path
            .strip_prefix(workspace)
            .unwrap_or(wav_path)
            .display(),
        cache.elapsed_seconds,
        out.strip_prefix(workspace).unwrap_or(&out).display(),
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let workspace = workspace_root()?;
    let wavs: Vec<PathBuf> = match args.tier.as_str() {
        "fixtures" => load_ft8_fixtures(&workspace)?
            .into_iter()
            .map(|f| f.wav_path)
            .collect(),
        "synth" => {
            let manifest = args
                .synth_manifest
                .clone()
                .unwrap_or_else(|| {
                    workspace.join("research/corpus/synth/manifests/clean.manifest.json")
                });
            load_synth_corpus(&workspace, &manifest)?
                .into_iter()
                .map(|e| e.wav_path)
                .collect()
        }
        "chrono-replay" => {
            let manifest = args.manifest.clone().unwrap_or_else(|| {
                workspace
                    .join("research/corpus/curated/ft8/chrono_replay.manifest.json")
            });
            pancetta_research::chrono_replay::load_chrono_replay_corpus(&manifest)?
                .into_iter()
                .map(|e| e.wav_path)
                .collect()
        }
        "curated-hard-200" | "curated-hard-1000" | "wild-50" | "wild-100" => {
            let label = match args.tier.as_str() {
                "curated-hard-200" => "hard_200",
                "curated-hard-1000" => "hard_1000",
                "wild-50" => "wild_50",
                "wild-100" => "wild_100",
                _ => unreachable!(),
            };
            let manifest = args.manifest.clone().unwrap_or_else(|| {
                workspace
                    .join("research/corpus/curated/ft8")
                    .join(format!("{label}.manifest.json"))
            });
            pancetta_research::curated::load_curated_corpus(&manifest)?
                .into_iter()
                .map(|e| e.wav_path)
                .collect()
        }
        other => anyhow::bail!(
            "unknown tier '{other}'. Use 'fixtures', 'synth', 'curated-hard-200', 'curated-hard-1000', 'wild-50', or 'chrono-replay'."
        ),
    };
    println!(
        "baseline: processing {} WAVs (tier={}, mode={})",
        wavs.len(),
        args.tier,
        args.mode
    );
    for wav in &wavs {
        process_wav(&workspace, args.mode, wav, &args.jt9_path, args.force)?;
    }
    println!("baseline: done.");
    Ok(())
}
