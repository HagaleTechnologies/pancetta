//! gen-synth — generate a synth WAV corpus from a SynthConfig JSON.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin gen-synth -- \
//!     --config research/corpus/synth/manifests/clean.config.json \
//!     --output research/corpus/synth/manifests/clean.manifest.json
//!
//! Task W0.2 (2026-07-06): the core generation logic (message modulation,
//! WSJT-X-2500Hz-calibrated AWGN, dt/lead-in slot placement) now lives in
//! `pancetta_research::synth` so `tests/snr_calibration_tests.rs` can
//! exercise it directly (mirrors the `gen_noise.rs` / `bin/gen_noise.rs`
//! split from Task W0.1). This binary is a thin CLI wrapper: parse args,
//! iterate the config's message/SNR/drift grid, draw a per-file
//! randomized base frequency (400-2600 Hz) and dt offset (-0.3..+0.3 s) so
//! decoders under test cannot overfit a single fixed grid position, write
//! WAVs, and save the manifest.

use anyhow::Context;
use hound::{SampleFormat, WavSpec, WavWriter};
use pancetta_ft8::ProtocolParams;
use pancetta_research::synth::{
    add_awgn_2500hz_ref, apply_linear_drift_crude, modulate_message_at_protocol, place_in_slot,
    signal_rms, SynthChannel, SynthConfig, SynthEntry, SynthManifest,
};
use pancetta_research::Mode;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use std::path::{Path, PathBuf};

/// Canonical FT8 sample rate (12 kHz, matches pancetta_ft8::SAMPLE_RATE)
const SAMPLE_RATE: u32 = 12_000;

/// Per-file randomized base-frequency range (Hz). Task W0.2: previously
/// every synth WAV was modulated at a single fixed 1500 Hz, which let a
/// decoder overfit that exact grid position rather than demonstrating
/// real sensitivity across the band. Reused as-is for FT4 (Task W0.4):
/// FT4's 4 tones span only 3*20.83 ≈ 62.5 Hz, comfortably inside this
/// range regardless of mode.
const BASE_FREQ_RANGE_HZ: std::ops::RangeInclusive<f64> = 400.0..=2600.0;

/// Per-file randomized dt (decode-time offset, seconds) range for FT8's
/// 15 s slot. Task W0.2: previously every synth WAV placed the signal at
/// exactly sample 0 (dt = 0 implicitly, no lead-in silence at all).
const DT_RANGE_S_FT8: std::ops::RangeInclusive<f64> = -0.3..=0.3;

/// Silence before the earliest possible dt (so `dt = DT_RANGE_S.start()`
/// still has margin before the buffer start). 1.0 s comfortably covers a
/// ±0.3 s dt range plus room to trim edge effects when measuring noise.
const LEAD_IN_S_FT8: f64 = 1.0;

/// Total per-WAV slot length: 15.0 s, matching real FT8 capture windows
/// (fixture/curated WAVs are already this length) — the ~12.64 s message
/// plus lead-in plus trailing margin fits comfortably within it for the
/// whole dt range.
const SLOT_LEN_SAMPLES_FT8: usize = 180_000;

/// Task W0.4 (2026-07-07): FT4's dt range, halved from FT8's `DT_RANGE_S_FT8`
/// to match FT4's halved (7.5s vs 15s) slot length.
const DT_RANGE_S_FT4: std::ops::RangeInclusive<f64> = -0.15..=0.15;

/// Task W0.4: FT4 lead-in, halved from FT8's to match the halved slot.
const LEAD_IN_S_FT4: f64 = 0.5;

/// Task W0.4: FT4 slot length — 7.5 s at 12 kHz, matching FT4's real
/// on-air T/R period (vs FT8's 15 s). FT4's active message is 5.04 s
/// (105 symbols * 0.048s), leaving ~2.46 s margin for lead-in + dt —
/// proportionally the same margin FT8 has (12.64s active / 15s slot).
const SLOT_LEN_SAMPLES_FT4: usize = 90_000;

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

    // Task W0.4: select protocol params + slot geometry by the config's
    // mode. Ft8 keeps the exact pre-W0.4 constants (byte-identical
    // corpora for every existing committed config, which has no `mode`
    // key and defaults to Ft8 per `SynthConfig::mode`'s `#[serde(default)]`).
    let (protocol_params, slot_len_samples, lead_in_s, dt_range_s) = match config.mode {
        Mode::Ft8 => (
            ProtocolParams::ft8(),
            SLOT_LEN_SAMPLES_FT8,
            LEAD_IN_S_FT8,
            DT_RANGE_S_FT8,
        ),
        Mode::Ft4 => (
            ProtocolParams::ft4(),
            SLOT_LEN_SAMPLES_FT4,
            LEAD_IN_S_FT4,
            DT_RANGE_S_FT4,
        ),
    };

    let output_dir = workspace.join(&config.output_dir);
    let mut entries = Vec::new();
    let mut total = 0usize;
    for (msg_idx, msg) in config.messages.iter().enumerate() {
        for snr_db in &config.snr_steps_db {
            for drift in &drift_steps {
                // Per-wav seed deterministic from (top-level seed, msg index, snr, drift).
                let seed_for_this_wav = config
                    .seed
                    .wrapping_add(msg_idx as u64)
                    .wrapping_mul(1_000_003)
                    .wrapping_add(snr_db.to_bits().wrapping_mul(7))
                    .wrapping_add(drift.to_bits().wrapping_mul(13));

                // Task W0.2: per-file base frequency + dt, drawn from a
                // seed DERIVED from (but distinct from) the noise seed so
                // the freq/dt draw doesn't consume the same RNG stream as
                // the AWGN fill (keeps the two independent while staying
                // fully deterministic for a given top-level config seed).
                let mut meta_rng = StdRng::seed_from_u64(seed_for_this_wav ^ 0xA5A5_A5A5_A5A5_A5A5);
                let base_freq_hz: f64 = meta_rng.random_range(BASE_FREQ_RANGE_HZ);
                let dt_s: f64 = meta_rng.random_range(dt_range_s.clone());

                let mut base_signal =
                    modulate_message_at_protocol(msg, base_freq_hz, &protocol_params)?;
                apply_linear_drift_crude(&mut base_signal, *drift);
                let rms = signal_rms(&base_signal);

                let mut samples = place_in_slot(&base_signal, dt_s, lead_in_s, slot_len_samples);
                add_awgn_2500hz_ref(&mut samples, rms, *snr_db, seed_for_this_wav);

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
                    base_freq_hz,
                    dt_s,
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
