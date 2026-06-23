//! curate-chrono-replay — produce a chronological-replay manifest from
//! the operator's `~/.pancetta/recordings/` directory.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin curate_chrono_replay -- \
//!     --source-dir ~/.pancetta/recordings \
//!     --output research/corpus/curated/ft8/chrono_replay.manifest.json \
//!     --session-prefix ft8_20260530_ \
//!     --slots 300
//!
//! Algorithm:
//!   1. Scan `source-dir` for `<session-prefix>*.wav` files.
//!   2. Sort by filename (filenames are timestamp-based, so lex sort = time sort).
//!   3. Take the first `slots` entries (a *contiguous* block — no skipping —
//!      because the chrono-replay tier's semantics require temporal continuity).
//!   4. SHA-256 each WAV; parse its timestamp from the filename.
//!   5. Write the manifest in slot_index order.
//!
//! Filename format expected: `ft8_YYYYMMDD_HHMMSS.wav`.
//!
//! Baseline generation is intentionally NOT part of this binary — the
//! standard `baseline` binary (with a manifest pointer) handles jt9
//! generation for any tier. The chrono-replay tier reuses the same
//! cached-by-SHA baselines, so consecutive sessions that overlap with
//! existing hard-200/wild-100 WAVs get free truth.

use anyhow::Context;
use chrono::{TimeZone, Utc};
use pancetta_research::chrono_replay::{ChronoReplayEntry, ChronoReplayManifest};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Args {
    source_dir: PathBuf,
    output: PathBuf,
    session_prefix: String,
    slots: usize,
    label: Option<String>,
    /// Skip the first N matching WAVs before taking `slots`. Useful when
    /// the first few slots of a session are warm-up noise.
    skip: usize,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut source_dir: Option<PathBuf> = None;
        let mut output: Option<PathBuf> = None;
        let mut session_prefix: Option<String> = None;
        let mut slots: usize = 300;
        let mut label: Option<String> = None;
        let mut skip: usize = 0;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--source-dir" => {
                    source_dir = Some(iter.next().context("--source-dir needs a value")?.into())
                }
                "--output" => output = Some(iter.next().context("--output needs a value")?.into()),
                "--session-prefix" => {
                    session_prefix = Some(iter.next().context("--session-prefix needs a value")?)
                }
                "--slots" => {
                    slots = iter
                        .next()
                        .context("--slots needs a value")?
                        .parse()
                        .context("--slots must be a positive integer")?;
                }
                "--label" => label = Some(iter.next().context("--label needs a value")?),
                "--skip" => {
                    skip = iter
                        .next()
                        .context("--skip needs a value")?
                        .parse()
                        .context("--skip must be a non-negative integer")?;
                }
                "-h" | "--help" => {
                    eprintln!(
                        "usage: curate_chrono_replay --source-dir <dir> --output <path> \\\n\
                         \t--session-prefix <prefix> [--slots N=300] [--skip N=0] [--label LABEL]\n\n\
                         Selects a *contiguous block* of WAVs matching <prefix>*.wav from\n\
                         <source-dir>, sorted by filename (which is timestamp-based), and\n\
                         emits a chrono-replay manifest. Run `baseline --tier chrono-replay`\n\
                         afterward to generate jt9 truth for any new SHAs."
                    );
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            source_dir: source_dir.context("--source-dir required")?,
            output: output.context("--output required")?,
            session_prefix: session_prefix.context("--session-prefix required")?,
            slots,
            label,
            skip,
        })
    }
}

fn discover_session_wavs(dir: &Path, prefix: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading source dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.starts_with(prefix) && name.ends_with(".wav") {
            out.push(path);
        }
    }
    // Filenames embed the timestamp (ft8_YYYYMMDD_HHMMSS.wav) so lex sort
    // is exactly time sort.
    out.sort();
    Ok(out)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

/// Parse an ISO-8601 UTC timestamp from a `ft8_YYYYMMDD_HHMMSS.wav` filename.
/// Returns the original filename stem on failure so the manifest still
/// renders even if the filename diverges from the expected pattern.
fn parse_filename_timestamp(filename: &str) -> String {
    // Strip leading "ft8_" and trailing ".wav" if present, then parse
    // YYYYMMDD_HHMMSS into RFC3339.
    let stem = filename
        .strip_suffix(".wav")
        .unwrap_or(filename)
        .trim_start_matches("ft8_");
    if let Some((date, time)) = stem.split_once('_') {
        if date.len() == 8 && time.len() == 6 {
            let y: i32 = date[0..4].parse().unwrap_or(1970);
            let mo: u32 = date[4..6].parse().unwrap_or(1);
            let d: u32 = date[6..8].parse().unwrap_or(1);
            let h: u32 = time[0..2].parse().unwrap_or(0);
            let mi: u32 = time[2..4].parse().unwrap_or(0);
            let s: u32 = time[4..6].parse().unwrap_or(0);
            if let chrono::LocalResult::Single(dt) = Utc.with_ymd_and_hms(y, mo, d, h, mi, s) {
                return dt.to_rfc3339();
            }
        }
    }
    // Fallback — operator inspecting the manifest will see the raw name.
    format!("unparsed:{filename}")
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;

    // Resolve ~ in source-dir.
    let source_dir = if let Some(rest) = args.source_dir.to_str().and_then(|s| s.strip_prefix("~/"))
    {
        let home = std::env::var("HOME").context("HOME not set; cannot expand ~")?;
        PathBuf::from(home).join(rest)
    } else {
        args.source_dir.clone()
    };

    let all = discover_session_wavs(&source_dir, &args.session_prefix)?;
    println!(
        "scanned {}: {} WAVs match prefix {:?}",
        source_dir.display(),
        all.len(),
        args.session_prefix,
    );
    anyhow::ensure!(
        !all.is_empty(),
        "no WAVs found matching {} in {}",
        args.session_prefix,
        source_dir.display()
    );

    // Take a contiguous block starting at `skip`.
    let end = (args.skip + args.slots).min(all.len());
    anyhow::ensure!(
        args.skip < all.len(),
        "skip ({}) exceeds total matching WAVs ({})",
        args.skip,
        all.len()
    );
    let block = &all[args.skip..end];
    println!(
        "taking contiguous block [{}..{}) → {} slots (requested {})",
        args.skip,
        end,
        block.len(),
        args.slots,
    );

    // SHA each WAV, build entries.
    let mut entries: Vec<ChronoReplayEntry> = Vec::with_capacity(block.len());
    for (i, path) in block.iter().enumerate() {
        let sha = sha256_file(path)?;
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("?.wav");
        let ts = parse_filename_timestamp(filename);
        entries.push(ChronoReplayEntry {
            wav_path: path.clone(),
            wav_sha256: sha,
            slot_index: i as u32,
            wav_timestamp: ts,
        });
        if (i + 1) % 50 == 0 || i + 1 == block.len() {
            println!("  hashed {}/{}", i + 1, block.len());
        }
    }

    let first_ts = entries
        .first()
        .map(|e| e.wav_timestamp.clone())
        .unwrap_or_default();
    let last_ts = entries
        .last()
        .map(|e| e.wav_timestamp.clone())
        .unwrap_or_default();

    // Compute span_seconds when both timestamps parsed cleanly.
    let span_seconds = match (
        chrono::DateTime::parse_from_rfc3339(&first_ts),
        chrono::DateTime::parse_from_rfc3339(&last_ts),
    ) {
        (Ok(a), Ok(b)) => (b - a).num_seconds() as f64,
        _ => 0.0,
    };

    let label = args.label.unwrap_or_else(|| {
        // Derive label from prefix: "ft8_20260530_" → "chrono_ft8_20260530"
        let l = args.session_prefix.trim_end_matches('_');
        format!("chrono_{l}")
    });

    let manifest = ChronoReplayManifest {
        schema_version: ChronoReplayManifest::CURRENT_SCHEMA_VERSION,
        label,
        generated_at: Utc::now().to_rfc3339(),
        source_session_label: args.session_prefix.clone(),
        first_wav_timestamp: first_ts,
        last_wav_timestamp: last_ts,
        span_seconds,
        entries,
    };

    manifest.save(&args.output).with_context(|| {
        format!(
            "writing chrono-replay manifest to {}",
            args.output.display()
        )
    })?;
    println!(
        "wrote {} entries spanning {:.1}s ({:.1} min) → {}",
        manifest.entries.len(),
        span_seconds,
        span_seconds / 60.0,
        args.output.display(),
    );
    Ok(())
}
