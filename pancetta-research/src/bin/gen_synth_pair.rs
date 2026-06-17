//! gen-synth-pair — hb-146 adversarial mutual-masking pair corpus generator.
//!
//! Each WAV contains TWO FT8 signals at controlled (ΔSNR, Δf, Δt). The
//! strong signal sits at 1500 Hz; the weak signal is offset by Δf in
//! frequency and Δt in time. AWGN is added so the strong signal's
//! SNR-vs-noise = `strong_snr_db`. The weak signal's effective SNR is
//! (`strong_snr_db` - `delta_snr_db`).
//!
//! This corpus targets shelved hb-086 V2 (joint LLR with soft cancellation)
//! and V3 (subtract-aware sync relaxation). V2/V3 were shelved because the
//! organic hard-200 corpus contained no marginal-SNR pair structure — every
//! decoded neighbor was already strong. This generator builds the missing
//! regime on demand at controlled grid points.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin gen-synth-pair -- \
//!     --config research/corpus/synth/manifests/synth_pair_200.config.json \
//!     --output research/corpus/synth/manifests/synth_pair_200.manifest.json

use anyhow::Context;
use hound::{SampleFormat, WavSpec, WavWriter};
use pancetta_ft8::{Ft8Encoder, Ft8Modulator};
use pancetta_research::synth::{SynthPairConfig, SynthPairEntry, SynthPairManifest};
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};
use std::path::{Path, PathBuf};

/// Canonical FT8 sample rate (12 kHz, matches pancetta_ft8::SAMPLE_RATE).
const SAMPLE_RATE: u32 = 12_000;
/// Generated slot length. The FT8 transmission itself is ~12.64 s; we use
/// a 15 s buffer to give the (delta_time_s) sweep room without clipping
/// either signal's tail.
const SLOT_SECONDS: f64 = 15.0;
/// Slot length in samples.
const SLOT_SAMPLES: usize = (SLOT_SECONDS * SAMPLE_RATE as f64) as usize;

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
                    eprintln!(
                        "usage: gen-synth-pair --config <config.json> --output <manifest.json>"
                    );
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

/// Encode + modulate a message at the canonical 1500 Hz base with the
/// given additional frequency_offset. Returns ~12.64 s of f32 samples.
fn modulate_message(text: &str, frequency_offset_hz: f64) -> anyhow::Result<Vec<f32>> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message(text, None)
        .map_err(|e| anyhow::anyhow!("Ft8Encoder::encode_message failed for '{text}': {e}"))?;

    let mut modulator = Ft8Modulator::new(SAMPLE_RATE, 1500.0, 1.0)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator::new failed: {e}"))?;
    let samples = modulator
        .modulate_symbols(&symbols, frequency_offset_hz)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator::modulate_symbols failed for '{text}': {e}"))?;
    Ok(samples)
}

/// Compute the RMS of a sample buffer.
fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64).powi(2)).sum();
    (sum_sq / samples.len() as f64).sqrt()
}

/// Place `signal` into `slot` starting at `start_sample`, scaled by `gain`.
/// Out-of-bounds samples are dropped. Mixing is additive.
fn place_signal(slot: &mut [f32], signal: &[f32], start_sample: isize, gain: f32) {
    for (i, &s) in signal.iter().enumerate() {
        let dst = start_sample + i as isize;
        if dst < 0 || dst as usize >= slot.len() {
            continue;
        }
        slot[dst as usize] += s * gain;
    }
}

/// Mix Gaussian noise of the given RMS into `slot` in place.
fn add_awgn_with_rms(slot: &mut [f32], noise_rms: f64, rng_seed: u64) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(rng_seed);
    let normal = Normal::new(0.0_f64, noise_rms).expect("noise stddev must be finite");
    for s in slot.iter_mut() {
        *s += normal.sample(&mut rng) as f32;
    }
}

fn write_wav(path: &Path, samples: &[f32]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(path, spec)?;
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let i = (clamped * 32767.0) as i16;
        w.write_sample(i)?;
    }
    w.finalize()?;
    Ok(())
}

/// Build a deterministic per-WAV seed from the run-level seed + parameter
/// tuple. Same inputs → byte-identical WAV.
fn per_wav_seed(
    base_seed: u64,
    pair_idx: usize,
    delta_snr_db: f64,
    delta_freq_hz: f64,
    delta_time_s: f64,
) -> u64 {
    base_seed
        .wrapping_add(pair_idx as u64)
        .wrapping_mul(1_000_003)
        .wrapping_add(delta_snr_db.to_bits().wrapping_mul(7))
        .wrapping_add(delta_freq_hz.to_bits().wrapping_mul(13))
        .wrapping_add(delta_time_s.to_bits().wrapping_mul(19))
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
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
    let config: SynthPairConfig = serde_json::from_str(&config_text)?;
    anyhow::ensure!(
        config.schema_version == SynthPairConfig::CURRENT_SCHEMA_VERSION,
        "SynthPairConfig schema_version {} not supported",
        config.schema_version
    );
    anyhow::ensure!(
        config.message_templates.len() >= 2,
        "Need at least 2 message templates to form pairs"
    );

    let output_dir = workspace.join(&config.output_dir);
    let lead_in_samples = (config.slot_lead_in_s * SAMPLE_RATE as f64) as isize;

    // Build the parameter sweep: full Cartesian product, then optionally
    // subsample down to max_wavs.
    let mut sweep: Vec<(usize, f64, f64, f64)> = Vec::new();
    for pair_idx in 0..config.message_templates.len() {
        for &dsnr in &config.delta_snr_db_steps {
            for &df in &config.delta_freq_hz_steps {
                for &dt in &config.delta_time_s_steps {
                    sweep.push((pair_idx, dsnr, df, dt));
                }
            }
        }
    }
    eprintln!(
        "gen-synth-pair: full sweep = {} WAVs ({} templates × {} ΔSNR × {} Δf × {} Δt)",
        sweep.len(),
        config.message_templates.len(),
        config.delta_snr_db_steps.len(),
        config.delta_freq_hz_steps.len(),
        config.delta_time_s_steps.len(),
    );
    if config.max_wavs > 0 && sweep.len() > config.max_wavs {
        // Stride-subsample so the parameter sweep stays well-covered.
        let stride = (sweep.len() as f64 / config.max_wavs as f64).ceil() as usize;
        let mut subsampled = Vec::with_capacity(config.max_wavs);
        for (i, item) in sweep.iter().enumerate() {
            if i % stride == 0 {
                subsampled.push(*item);
            }
            if subsampled.len() >= config.max_wavs {
                break;
            }
        }
        sweep = subsampled;
        eprintln!(
            "gen-synth-pair: subsampled to {} WAVs (stride={stride})",
            sweep.len()
        );
    }

    let mut entries: Vec<SynthPairEntry> = Vec::with_capacity(sweep.len());
    for (pair_idx, dsnr, df, dt) in sweep {
        // Build the pair: strong = template[i], weak = template[(i+1) % len].
        let msg_strong = config.message_templates[pair_idx].clone();
        let msg_weak =
            config.message_templates[(pair_idx + 1) % config.message_templates.len()].clone();

        // Modulate each signal. The strong signal goes at 1500 Hz (offset=0);
        // the weak signal goes at 1500+df Hz.
        let strong_samples = modulate_message(&msg_strong, 0.0)?;
        let weak_samples = modulate_message(&msg_weak, df)?;

        // Gain ratio: weak is `dsnr` dB below strong. ΔSNR=0 means equal amplitude.
        let weak_gain = 10f64.powf(-dsnr / 20.0) as f32;

        // Allocate 15 s slot buffer.
        let mut slot = vec![0.0f32; SLOT_SAMPLES];

        // Place strong at lead_in; weak at lead_in + dt.
        let weak_start = lead_in_samples + (dt * SAMPLE_RATE as f64) as isize;
        place_signal(&mut slot, &strong_samples, lead_in_samples, 1.0);
        place_signal(&mut slot, &weak_samples, weak_start, weak_gain);

        // Add AWGN. Target noise RMS = strong_signal_rms / 10^(strong_snr/20).
        // Strong signal RMS is computed from the modulator output BEFORE mixing
        // (so noise level is independent of how the two signals interact).
        let signal_rms = rms(&strong_samples);
        let noise_rms = signal_rms / 10f64.powf(config.strong_snr_db / 20.0);
        let seed = per_wav_seed(config.seed, pair_idx, dsnr, df, dt);
        add_awgn_with_rms(&mut slot, noise_rms, seed);

        // Filename encodes the parameter tuple.
        let strong_slug = slugify(&msg_strong);
        let filename =
            format!("p{pair_idx:02}_{strong_slug}__dSNR{dsnr:+.1}_dF{df:+.1}_dT{dt:+.2}.wav",);
        let wav_path = output_dir.join(&filename);
        write_wav(&wav_path, &slot)?;

        entries.push(SynthPairEntry {
            wav_path: PathBuf::from(&config.output_dir).join(&filename),
            message_strong: msg_strong,
            message_weak: msg_weak,
            strong_snr_db: config.strong_snr_db,
            delta_snr_db: dsnr,
            delta_freq_hz: df,
            delta_time_s: dt,
            seed_for_this_wav: seed,
        });
    }

    let total = entries.len();
    let manifest = SynthPairManifest {
        schema_version: SynthPairManifest::CURRENT_SCHEMA_VERSION,
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
        "gen-synth-pair: wrote {} WAVs to {}; manifest at {}",
        total,
        output_dir.display(),
        manifest_path.display(),
    );
    Ok(())
}
