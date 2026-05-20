//! gen-synth — generate a synth WAV corpus from a SynthConfig JSON.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin gen-synth -- \
//!     --config research/corpus/synth/manifests/clean.config.json \
//!     --output research/corpus/synth/manifests/clean.manifest.json

use anyhow::Context;
use hound::{SampleFormat, WavSpec, WavWriter};
use pancetta_ft8::{Ft8Encoder, Ft8Modulator};
use pancetta_research::synth::{SynthChannel, SynthConfig, SynthEntry, SynthManifest};
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};
use std::path::{Path, PathBuf};

/// Canonical FT8 sample rate (12 kHz, matches pancetta_ft8::SAMPLE_RATE)
const SAMPLE_RATE: u32 = 12_000;

#[derive(Debug)]
struct Args {
    config: PathBuf,
    output_manifest: PathBuf,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut config: Option<PathBuf> = None;
        let mut output: Option<PathBuf> = None;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--config" => config = Some(iter.next().context("--config needs a value")?.into()),
                "--output" => output = Some(iter.next().context("--output needs a value")?.into()),
                "-h" | "--help" => {
                    eprintln!("usage: gen-synth --config <config.json> --output <manifest.json>");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            config: config.context("--config required")?,
            output_manifest: output.context("--output required")?,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

/// Encode + modulate one FT8 message into 12 kHz mono f32 samples at the
/// canonical 1500 Hz base audio offset.
///
/// Uses the real pancetta-ft8 public API (behind the `transmit` feature):
///   1. `Ft8Encoder::new().encode_message(text, None)` → [u8; 79] tone symbols
///   2. `Ft8Modulator::new(…).modulate_symbols(&symbols: &[u8; NUM_SYMBOLS], 0.0)` → Vec<f32>
///
/// The modulator's default config is: sample_rate=12000, base_frequency=1500 Hz,
/// tx_power=1.0. We pass frequency_offset=0.0 so the signal sits at exactly 1500 Hz.
fn modulate_message(text: &str) -> anyhow::Result<Vec<f32>> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message(text, None)
        .map_err(|e| anyhow::anyhow!("Ft8Encoder::encode_message failed for '{text}': {e}"))?;

    let mut modulator = Ft8Modulator::new(SAMPLE_RATE, 1500.0, 1.0)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator::new failed: {e}"))?;
    let samples = modulator
        .modulate_symbols(&symbols, 0.0)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator::modulate_symbols failed for '{text}': {e}"))?;

    Ok(samples)
}

/// Mix AWGN at the target SNR. SNR is measured in dB relative to signal RMS.
fn add_awgn(samples: &mut [f32], snr_db: f64, rng_seed: u64) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(rng_seed);
    // Signal RMS:
    let signal_rms: f64 = (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
        / samples.len() as f64)
        .sqrt();
    // Target noise RMS so that 20*log10(signal_rms / noise_rms) = snr_db:
    let noise_rms = signal_rms / 10f64.powf(snr_db / 20.0);
    let normal = Normal::new(0.0_f64, noise_rms).expect("noise stddev must be finite");
    for s in samples.iter_mut() {
        *s += normal.sample(&mut rng) as f32;
    }
}

fn write_wav(path: &Path, samples: &[f32]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write as 16-bit PCM — matches the rest of the corpus (fixtures + operator
    // recordings are all 16-bit PCM mono 12 kHz).
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(path, spec)?;
    for &s in samples {
        // Clamp to [-1, 1] then scale to i16.
        let clamped = s.clamp(-1.0, 1.0);
        let i = (clamped * 32767.0) as i16;
        w.write_sample(i)?;
    }
    w.finalize()?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let workspace = workspace_root()?;
    let config_path = if args.config.is_absolute() {
        args.config.clone()
    } else {
        workspace.join(&args.config)
    };
    let config_text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    let config: SynthConfig = serde_json::from_str(&config_text)?;
    anyhow::ensure!(
        config.schema_version == SynthConfig::CURRENT_SCHEMA_VERSION,
        "SynthConfig schema_version {} not supported",
        config.schema_version
    );
    anyhow::ensure!(
        config.channel == SynthChannel::Awgn,
        "Plan 2 only supports channel=awgn; got {:?}",
        config.channel
    );

    let output_dir = workspace.join(&config.output_dir);
    let mut entries = Vec::new();
    let mut total = 0usize;
    for (msg_idx, msg) in config.messages.iter().enumerate() {
        let base_samples = modulate_message(msg)?;
        for snr_db in &config.snr_steps_db {
            // Per-wav seed is deterministic from (top-level seed, msg index, snr_db).
            let seed_for_this_wav = config
                .seed
                .wrapping_add(msg_idx as u64)
                .wrapping_mul(1_000_003)
                .wrapping_add((snr_db.to_bits() as u64).wrapping_mul(7));
            let mut samples = base_samples.clone();
            add_awgn(&mut samples, *snr_db, seed_for_this_wav);
            // Filename: <msg-slug>__<snr>dB.wav (slugify the message text).
            let slug: String = msg
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let filename = format!("{slug}__{:+.1}dB.wav", snr_db);
            let wav_path = output_dir.join(&filename);
            write_wav(&wav_path, &samples)?;
            entries.push(SynthEntry {
                wav_path: PathBuf::from(&config.output_dir).join(&filename),
                encoded_message: msg.clone(),
                snr_db: *snr_db,
                channel: config.channel,
                seed_for_this_wav,
            });
            total += 1;
        }
    }

    let manifest = SynthManifest {
        schema_version: SynthManifest::CURRENT_SCHEMA_VERSION,
        config: config.clone(),
        entries,
    };
    let manifest_path = if args.output_manifest.is_absolute() {
        args.output_manifest.clone()
    } else {
        workspace.join(&args.output_manifest)
    };
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    manifest.save(&manifest_path)?;
    println!(
        "gen-synth: wrote {} WAVs to {}; manifest at {}",
        total,
        output_dir.display(),
        manifest_path.display(),
    );
    Ok(())
}
