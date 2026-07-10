//! gen-noise — generate a deterministic pure-noise (+ birdie) WAV corpus
//! and its manifest, for the `noise_1000` eval tier's FP-on-noise
//! guardrail (Workstream 0, decoder-tp-sensitivity plan).
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin gen-noise -- \
//!     --count 1000 --seed 20260706 --birdie-fraction 0.3 \
//!     --output-dir ~/.pancetta/recordings/noise_1000 \
//!     --manifest research/corpus/curated/noise/noise_1000.manifest.json \
//!     --label noise_1000

use anyhow::Context;
use pancetta_research::gen_noise::{generate_noise_corpus_with_manifest, NoiseConfig};
use std::path::PathBuf;

#[derive(Debug)]
struct Args {
    count: usize,
    seed: u64,
    birdie_fraction: f32,
    output_dir: PathBuf,
    manifest: PathBuf,
    label: String,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut count: Option<usize> = None;
        let mut seed: Option<u64> = None;
        let mut birdie_fraction: f32 = 0.0;
        let mut output_dir: Option<PathBuf> = None;
        let mut manifest: Option<PathBuf> = None;
        let mut label: String = "noise".to_string();
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--count" => count = Some(iter.next().context("--count needs a value")?.parse()?),
                "--seed" => seed = Some(iter.next().context("--seed needs a value")?.parse()?),
                "--birdie-fraction" => {
                    birdie_fraction = iter
                        .next()
                        .context("--birdie-fraction needs a value (0.0-1.0)")?
                        .parse()?;
                }
                "--output-dir" => {
                    output_dir = Some(iter.next().context("--output-dir needs a value")?.into());
                }
                "--manifest" => {
                    manifest = Some(iter.next().context("--manifest needs a value")?.into());
                }
                "--label" => {
                    label = iter.next().context("--label needs a value")?;
                }
                "-h" | "--help" => {
                    eprintln!(
                        "usage: gen-noise --count N --seed S --birdie-fraction F --output-dir DIR --manifest PATH [--label NAME]"
                    );
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            count: count.context("--count required")?,
            seed: seed.context("--seed required")?,
            birdie_fraction,
            output_dir: output_dir.context("--output-dir required")?,
            manifest: manifest.context("--manifest required")?,
            label,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

/// Expand a leading `~` to the user's home directory. `output_dir` for
/// this corpus conventionally lives under `~/.pancetta/recordings/...`,
/// outside the repo, matching the curated real-recording corpora.
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs_home() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    anyhow::ensure!(
        (0.0..=1.0).contains(&args.birdie_fraction),
        "--birdie-fraction must be in [0.0, 1.0], got {}",
        args.birdie_fraction
    );
    let workspace = workspace_root()?;
    let output_dir = expand_tilde(&args.output_dir);
    let config = NoiseConfig {
        count: args.count,
        seed: args.seed,
        birdie_fraction: args.birdie_fraction,
    };
    println!(
        "gen-noise: generating {} WAVs (seed={}, birdie_fraction={}) into {}",
        config.count,
        config.seed,
        config.birdie_fraction,
        output_dir.display(),
    );
    let manifest = generate_noise_corpus_with_manifest(&output_dir, config, &args.label)?;
    let n_birdie = manifest.entries.iter().filter(|e| e.has_birdie).count();

    let manifest_path = if args.manifest.is_absolute() {
        args.manifest.clone()
    } else {
        workspace.join(&args.manifest)
    };
    manifest.save(&manifest_path)?;
    println!(
        "gen-noise: wrote {} WAVs ({} with birdies) to {}; manifest at {}",
        manifest.entries.len(),
        n_birdie,
        output_dir.display(),
        manifest_path.display(),
    );
    Ok(())
}
