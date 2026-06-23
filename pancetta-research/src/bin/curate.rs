//! curate — score operator recording WAVs and produce three manifests:
//!   - hard_200.manifest.json (top 200 by interest score)
//!   - hard_1000.manifest.json (top 1000)
//!   - wild_50.manifest.json (50 random from full corpus)
//!
//! Scoring uses pancetta-only signals (decode count, noise floor) — no jt9
//! call. The baseline binary runs jt9 over the curated set as a separate
//! step.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin curate -- \
//!     --source-dir ~/.pancetta/recordings \
//!     --output-prefix research/corpus/curated/ft8

use anyhow::Context;
use chrono::Utc;
use pancetta_research::curated::{CuratedEntry, CuratedManifest, ScoreBreakdown};
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use pancetta_research::noise::estimate_noise_floor_db;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const SCORE_W_DECODE_COUNT: f64 = 1.0;
const SCORE_W_NOISE_FLOOR: f64 = 0.05; // dB scaled: -20 dB → +1.0 boost
const SCORE_W_SNR_DIVERSITY: f64 = 0.5;

#[derive(Debug)]
struct Args {
    source_dir: PathBuf,
    output_prefix: PathBuf,
    sample_size: Option<usize>, // limit for fast iteration; None = full corpus
    seed: u64,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut source_dir: Option<PathBuf> = None;
        let mut output_prefix: Option<PathBuf> = None;
        let mut sample_size: Option<usize> = None;
        let mut seed: u64 = 42;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--source-dir" => {
                    source_dir = Some(iter.next().context("--source-dir needs a value")?.into())
                }
                "--output-prefix" => {
                    output_prefix =
                        Some(iter.next().context("--output-prefix needs a value")?.into())
                }
                "--sample-size" => {
                    sample_size = Some(
                        iter.next()
                            .context("--sample-size needs a value")?
                            .parse()?,
                    )
                }
                "--seed" => seed = iter.next().context("--seed needs a value")?.parse()?,
                "-h" | "--help" => {
                    eprintln!("usage: curate --source-dir <dir> --output-prefix <dir> [--sample-size N] [--seed N]");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            source_dir: source_dir.context("--source-dir required")?,
            output_prefix: output_prefix.context("--output-prefix required")?,
            sample_size,
            seed,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn discover_wavs(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "wav") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
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

fn score_wav(
    decoder: &dyn DecoderUnderTest,
    path: &Path,
) -> anyhow::Result<(f64, ScoreBreakdown, String)> {
    let samples = read_wav_samples(path)?;
    let noise = estimate_noise_floor_db(&samples);
    let sha = sha256_file(path)?;
    let decodes = decoder.decode_wav(path).unwrap_or_default();
    let decode_count = decodes.len() as u32;
    let mean_snr = if decodes.is_empty() {
        None
    } else {
        let sum: f64 = decodes.iter().map(|d| d.snr_db).sum();
        Some(sum / decodes.len() as f64)
    };
    // SNR-diversity proxy: lower mean SNR (more weak decodes) = more interesting.
    let snr_score = mean_snr.map_or(0.0, |m| (-m / 20.0).max(0.0));
    let score = SCORE_W_DECODE_COUNT * (decode_count as f64)
        // Busier band (noise floor closer to 0 dB) = larger contribution.
        // noise_floor_db is negative (e.g. -30 dB clean, -20 dB busy);
        // adding it directly so busier bands (less-negative noise) score higher.
        + SCORE_W_NOISE_FLOOR * noise
        + SCORE_W_SNR_DIVERSITY * snr_score;
    Ok((
        score,
        ScoreBreakdown {
            pancetta_decode_count: decode_count,
            noise_floor_db: noise,
            mean_decoded_snr_db: mean_snr,
        },
        sha,
    ))
}

fn write_manifest(
    label: &str,
    entries: Vec<CuratedEntry>,
    scoring_decoder: &str,
    output_path: &Path,
) -> anyhow::Result<()> {
    let manifest = CuratedManifest {
        schema_version: CuratedManifest::CURRENT_SCHEMA_VERSION,
        label: label.to_string(),
        generated_at: Utc::now().to_rfc3339(),
        scoring_decoder: scoring_decoder.to_string(),
        entries,
    };
    manifest.save(output_path)?;
    println!(
        "wrote {} entries to {}",
        manifest.entries.len(),
        output_path.display()
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let workspace = workspace_root()?;

    // 1. Discover WAVs.
    let mut wavs = discover_wavs(&args.source_dir)?;
    println!(
        "discovered {} WAVs in {}",
        wavs.len(),
        args.source_dir.display()
    );
    if let Some(n) = args.sample_size {
        let mut rng = rand::rngs::StdRng::seed_from_u64(args.seed);
        wavs.shuffle(&mut rng);
        wavs.truncate(n);
        println!("sampled {} for scoring", wavs.len());
    }
    let total = wavs.len();

    // 2. Score each. Use pancetta decoder with default config.
    let decoder = Ft8Decoder::with_default_config();
    let mut scored: Vec<(PathBuf, String, f64, ScoreBreakdown)> = Vec::with_capacity(total);
    for (i, wav) in wavs.iter().enumerate() {
        match score_wav(&decoder, wav) {
            Ok((score, breakdown, sha)) => scored.push((wav.clone(), sha, score, breakdown)),
            Err(e) => {
                eprintln!("warn: scoring {} failed: {e}", wav.display());
            }
        }
        if (i + 1) % 100 == 0 || i + 1 == total {
            println!("  scored {}/{}", i + 1, total);
        }
    }

    // 3. Sort by score descending; emit Hard-200 + Hard-1000.
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let mk_entry = |(p, sha, s, b): &(PathBuf, String, f64, ScoreBreakdown)| CuratedEntry {
        wav_path: p.clone(),
        wav_sha256: sha.clone(),
        interest_score: *s,
        score_breakdown: b.clone(),
    };
    let scoring_decoder = decoder.identity();

    let output_prefix = if args.output_prefix.is_absolute() {
        args.output_prefix.clone()
    } else {
        workspace.join(&args.output_prefix)
    };
    let hard_200: Vec<_> = scored.iter().take(200).map(mk_entry).collect();
    let hard_1000: Vec<_> = scored.iter().take(1000).map(mk_entry).collect();
    write_manifest(
        "hard_200",
        hard_200,
        &scoring_decoder,
        &output_prefix.join("hard_200.manifest.json"),
    )?;
    write_manifest(
        "hard_1000",
        hard_1000,
        &scoring_decoder,
        &output_prefix.join("hard_1000.manifest.json"),
    )?;

    // 4. Random Wild-50 sample (different seed branch for diversity).
    let mut rng = rand::rngs::StdRng::seed_from_u64(args.seed.wrapping_add(13));
    let mut wild_pool: Vec<_> = scored.iter().collect();
    wild_pool.shuffle(&mut rng);
    let wild_50: Vec<_> = wild_pool.iter().take(50).map(|t| mk_entry(t)).collect();
    write_manifest(
        "wild_50",
        wild_50,
        &scoring_decoder,
        &output_prefix.join("wild_50.manifest.json"),
    )?;

    Ok(())
}
