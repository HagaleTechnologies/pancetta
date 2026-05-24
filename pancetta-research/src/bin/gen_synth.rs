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

/// Apply crude linear frequency drift to a real signal by multiplicative
/// time-varying cosine. NOT true Doppler — true Doppler requires complex
/// analytic signal manipulation (Hilbert transform). This multiplicative
/// model perturbs the spectrogram (introduces AM-like sidebands and shifts
/// peak energy across bins as time progresses) which is sufficient as a
/// hb-015 unblock corpus. Rigorous Doppler evaluation needs a Watterson
/// channel implementation in a future iter.
fn apply_linear_drift_crude(samples: &mut [f32], drift_hz_per_sec: f64) {
    if drift_hz_per_sec.abs() < f64::EPSILON {
        return;
    }
    let dt = 1.0 / SAMPLE_RATE as f64;
    for (i, s) in samples.iter_mut().enumerate() {
        let t = i as f64 * dt;
        // Phase ramp: integral of 2π × drift × t dt = π × drift × t²
        let phase = std::f64::consts::PI * drift_hz_per_sec * t * t;
        *s = (*s as f64 * phase.cos()) as f32;
    }
}

/// Mix AWGN at the target SNR. SNR is measured in dB relative to signal RMS.
fn add_awgn(samples: &mut [f32], snr_db: f64, rng_seed: u64) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(rng_seed);
    // Signal RMS:
    let signal_rms: f64 =
        (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt();
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
        matches!(config.channel, SynthChannel::Awgn | SynthChannel::AwgnDrift),
        "Unsupported channel: {:?}",
        config.channel
    );

    // For Awgn channel, drift_steps is forced to [0.0] (no drift).
    // For AwgnDrift, use config.drift_steps_hz_per_sec; default to [0.0]
    // if empty (degenerates to Awgn behavior, useful for sanity).
    let drift_steps: Vec<f64> = match config.channel {
        SynthChannel::Awgn => vec![0.0],
        SynthChannel::AwgnDrift => {
            if config.drift_steps_hz_per_sec.is_empty() {
                vec![0.0]
            } else {
                config.drift_steps_hz_per_sec.clone()
            }
        }
    };

    let output_dir = workspace.join(&config.output_dir);
    let mut entries = Vec::new();
    let mut total = 0usize;
    for (msg_idx, msg) in config.messages.iter().enumerate() {
        let base_samples = modulate_message(msg)?;
        for snr_db in &config.snr_steps_db {
            for drift in &drift_steps {
                // Per-wav seed deterministic from (top-level seed, msg index, snr, drift).
                let seed_for_this_wav = config
                    .seed
                    .wrapping_add(msg_idx as u64)
                    .wrapping_mul(1_000_003)
                    .wrapping_add((snr_db.to_bits() as u64).wrapping_mul(7))
                    .wrapping_add((drift.to_bits() as u64).wrapping_mul(13));
                let mut samples = base_samples.clone();
                apply_linear_drift_crude(&mut samples, *drift);
                add_awgn(&mut samples, *snr_db, seed_for_this_wav);
                // Filename: <msg-slug>__<snr>dB[_<drift>Hzps].wav
                let slug: String = msg
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect();
                let filename = if matches!(config.channel, SynthChannel::AwgnDrift) {
                    format!("{slug}__{:+.1}dB_{:+.1}Hzps.wav", snr_db, drift)
                } else {
                    format!("{slug}__{:+.1}dB.wav", snr_db)
                };
                let wav_path = output_dir.join(&filename);
                write_wav(&wav_path, &samples)?;
                entries.push(SynthEntry {
                    wav_path: PathBuf::from(&config.output_dir).join(&filename),
                    encoded_message: msg.clone(),
                    snr_db: *snr_db,
                    channel: config.channel,
                    drift_hz_per_sec: *drift,
                    seed_for_this_wav,
                });
                total += 1;
            }
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
