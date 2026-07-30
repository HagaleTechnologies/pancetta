//! Pure-noise (+ optional birdie) corpus generator.
//!
//! This is the FP-on-noise measurement guardrail for Workstream 0 of the
//! decoder-true-positive-sensitivity plan
//! (`docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`,
//! decision D0(a)). Every WAV this module produces contains **no FT8
//! signal whatsoever** — seeded white Gaussian noise, optionally with
//! sine-carrier "birdie" interference layered on top. Any decode a
//! decoder under test returns against one of these WAVs is, by
//! construction, a false positive: there is nothing valid to decode.
//!
//! Determinism is the whole point: the same [`NoiseConfig`] must produce
//! byte-identical WAVs on every run (no wall-clock, no filesystem-order
//! dependence) so the FP-on-noise number is reproducible and diffable
//! across decoder changes.

use anyhow::Context;
use hound::{SampleFormat, WavSpec, WavWriter};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, Normal};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::f64::consts::{PI, TAU};
use std::path::{Path, PathBuf};

/// Canonical FT8 sample rate (12 kHz), matches `pancetta_ft8::SAMPLE_RATE`.
pub const SAMPLE_RATE: u32 = 12_000;

/// One FT8 slot duration, in seconds.
pub const SLOT_SECONDS: f64 = 15.0;

/// Target noise RMS amplitude (full-scale = 1.0). ~-30 dBFS, matching the
/// "clean-band" reference documented in `crate::noise::estimate_noise_floor_db`.
pub const TARGET_NOISE_RMS: f64 = 0.03;

/// Config for [`generate_noise_corpus`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoiseConfig {
    /// Number of WAVs to generate.
    pub count: usize,
    /// Deterministic seed. Same seed + same `count`/`birdie_fraction` →
    /// byte-identical WAVs.
    pub seed: u64,
    /// Fraction (0.0-1.0) of generated files that additionally get birdie
    /// interference (1-3 steady carriers + one slowly-drifting carrier)
    /// layered onto the noise floor.
    pub birdie_fraction: f32,
}

/// One generated noise-corpus WAV entry — enough metadata for a manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoiseEntry {
    /// Absolute path to the WAV on disk.
    pub wav_path: PathBuf,
    /// SHA-256 hex of the WAV file content.
    pub wav_sha256: String,
    /// Whether this file has birdie interference layered on the noise.
    pub has_birdie: bool,
    /// The per-file seed used to generate this WAV (for reproducibility).
    pub seed_for_this_wav: u64,
}

/// Noise-corpus manifest. Same shape convention as `curated::CuratedManifest`
/// (`schema_version` + `label` + `generated_at` + `entries` carrying
/// `wav_path` + `wav_sha256`), specialized with this module's own
/// config/entry types. `wav_path` entries are absolute — the noise corpus
/// lives under `~/.pancetta/recordings/`, outside the workspace, exactly
/// like the curated real-recording corpora.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoiseManifest {
    pub schema_version: u32,
    pub label: String,
    pub generated_at: String,
    pub config: NoiseConfig,
    pub entries: Vec<NoiseEntry>,
}

impl NoiseManifest {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn save<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<()> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let m: NoiseManifest = serde_json::from_str(&s)?;
        anyhow::ensure!(
            m.schema_version == Self::CURRENT_SCHEMA_VERSION,
            "NoiseManifest schema_version {} not supported (expected {})",
            m.schema_version,
            Self::CURRENT_SCHEMA_VERSION,
        );
        Ok(m)
    }
}

/// Load a noise manifest from disk and return its entries.
pub fn load_noise_corpus(manifest_path: &Path) -> anyhow::Result<Vec<NoiseEntry>> {
    let manifest = NoiseManifest::load(manifest_path)?;
    Ok(manifest.entries)
}

/// Derive a deterministic per-file seed from the corpus seed + file index.
/// Same scheme `gen_synth.rs` uses for its per-WAV seeds.
fn seed_for_index(base_seed: u64, index: usize) -> u64 {
    base_seed.wrapping_add(index as u64).wrapping_mul(1_000_003)
}

/// Deterministically select which file indices get birdie interference.
/// Uses a Fisher-Yates shuffle seeded independently of the per-file noise
/// RNGs (so toggling birdie selection never perturbs the noise itself,
/// and vice versa) to pick exactly `round(count * birdie_fraction)`
/// indices — an exact count regardless of how the RNG happens to land.
fn select_birdie_indices(count: usize, birdie_fraction: f32, seed: u64) -> Vec<bool> {
    let n_birdie = ((count as f32) * birdie_fraction).round() as usize;
    let n_birdie = n_birdie.min(count);
    let mut indices: Vec<usize> = (0..count).collect();
    // XOR with an arbitrary constant so this selection RNG never shares a
    // seed with any per-file noise RNG derived from the same base seed.
    let mut rng = StdRng::seed_from_u64(seed ^ 0xB1DD_1E5E_ED00_u64);
    for i in (1..indices.len()).rev() {
        let j = rng.random_range(0..=i);
        indices.swap(i, j);
    }
    let mut flags = vec![false; count];
    for &idx in indices.iter().take(n_birdie) {
        flags[idx] = true;
    }
    flags
}

/// Generate one WAV's worth of pure-noise (+ optional birdie) samples.
fn generate_one_wav(n_samples: usize, file_seed: u64, has_birdie: bool) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(file_seed);
    let normal = Normal::new(0.0_f64, TARGET_NOISE_RMS).expect("noise stddev must be finite");
    let mut samples: Vec<f32> = (0..n_samples)
        .map(|_| normal.sample(&mut rng) as f32)
        .collect();
    if has_birdie {
        add_birdies(&mut samples, &mut rng);
    }
    samples
}

/// Layer 1-3 steady sine carriers (random freq 300-2900 Hz, random level
/// 0 to +20 dB over the noise floor) plus one slowly drifting carrier
/// (±0.5 Hz/s) onto `samples`. Draws from `rng` so the whole WAV
/// (noise + birdies) is reproducible from a single per-file seed.
fn add_birdies(samples: &mut [f32], rng: &mut StdRng) {
    let dt = 1.0 / SAMPLE_RATE as f64;
    let n_steady = rng.random_range(1..=3u32);
    for _ in 0..n_steady {
        let freq = rng.random_range(300.0_f64..=2900.0);
        let level_db = rng.random_range(0.0_f64..=20.0);
        let phase0 = rng.random_range(0.0..TAU);
        let amp = carrier_amplitude(level_db);
        for (i, s) in samples.iter_mut().enumerate() {
            let t = i as f64 * dt;
            *s += (amp * (2.0 * PI * freq * t + phase0).cos()) as f32;
        }
    }
    // One slowly-drifting carrier, independent of the steady carriers above.
    let center_freq = rng.random_range(300.0_f64..=2900.0);
    let level_db = rng.random_range(0.0_f64..=20.0);
    let drift_hz_per_sec = rng.random_range(-0.5_f64..=0.5);
    let phase0 = rng.random_range(0.0..TAU);
    let amp = carrier_amplitude(level_db);
    for (i, s) in samples.iter_mut().enumerate() {
        let t = i as f64 * dt;
        // Instantaneous freq = center + drift*t; phase = integral of
        // 2*pi*freq(t) dt = 2*pi*(center*t + drift*t^2/2).
        let phase = 2.0 * PI * (center_freq * t + 0.5 * drift_hz_per_sec * t * t) + phase0;
        *s += (amp * phase.cos()) as f32;
    }
}

/// Sine-carrier peak amplitude that puts the tone's RMS `level_db` above
/// the noise-floor RMS (`TARGET_NOISE_RMS`). A sine of peak amplitude `a`
/// has RMS `a / sqrt(2)`, so `a = TARGET_NOISE_RMS * 10^(level_db/20) * sqrt(2)`.
fn carrier_amplitude(level_db: f64) -> f64 {
    TARGET_NOISE_RMS * 10f64.powf(level_db / 20.0) * std::f64::consts::SQRT_2
}

fn write_wav(path: &Path, samples: &[f32]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 16-bit PCM mono 12 kHz — matches the rest of the corpus (fixtures,
    // synth, operator recordings are all this format).
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(path, spec)
        .with_context(|| format!("creating WAV writer for {}", path.display()))?;
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let i = (clamped * 32767.0) as i16;
        w.write_sample(i)?;
    }
    w.finalize()?;
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

/// Generate `config.count` deterministic pure-noise (+ birdie) WAVs into
/// `dir`. Returns the WAV paths (absolute, in generation order). Files
/// are named `noise_XXXX.wav` (0-padded to the width of `count`).
///
/// Determinism contract: for a fixed `config`, re-running this function
/// (even into a fresh directory, even on a different machine) produces
/// byte-identical WAV files — every RNG draw derives only from
/// `(config.seed, file index)`, never from wall-clock time or
/// filesystem/OS state.
pub fn generate_noise_corpus(dir: &Path, config: &NoiseConfig) -> anyhow::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating noise corpus dir {}", dir.display()))?;
    let n_samples = (SAMPLE_RATE as f64 * SLOT_SECONDS).round() as usize;
    let birdie_flags = select_birdie_indices(config.count, config.birdie_fraction, config.seed);
    let width = config.count.max(1).to_string().len().max(4);
    let mut paths = Vec::with_capacity(config.count);
    for (i, &has_birdie) in birdie_flags.iter().enumerate() {
        let file_seed = seed_for_index(config.seed, i);
        let samples = generate_one_wav(n_samples, file_seed, has_birdie);
        let filename = format!("noise_{i:0width$}.wav", width = width);
        let path = dir.join(&filename);
        write_wav(&path, &samples)?;
        paths.push(path);
    }
    Ok(paths)
}

/// Generate the corpus AND build the accompanying manifest (with SHA-256
/// hashes) in one call — the convenience entry point the `gen-noise`
/// binary uses.
pub fn generate_noise_corpus_with_manifest(
    dir: &Path,
    config: NoiseConfig,
    label: &str,
) -> anyhow::Result<NoiseManifest> {
    let birdie_flags = select_birdie_indices(config.count, config.birdie_fraction, config.seed);
    let paths = generate_noise_corpus(dir, &config)?;
    let mut entries = Vec::with_capacity(paths.len());
    for (i, path) in paths.iter().enumerate() {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()?.join(path)
        };
        let sha = sha256_file(path)?;
        entries.push(NoiseEntry {
            wav_path: abs,
            wav_sha256: sha,
            has_birdie: birdie_flags[i],
            seed_for_this_wav: seed_for_index(config.seed, i),
        });
    }
    Ok(NoiseManifest {
        schema_version: NoiseManifest::CURRENT_SCHEMA_VERSION,
        label: label.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        config,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(samples: &[f32]) -> f64 {
        let sum_sq: f64 = samples.iter().map(|&s| (s as f64).powi(2)).sum();
        (sum_sq / samples.len() as f64).sqrt()
    }

    #[test]
    fn same_seed_produces_byte_identical_wavs() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let config = NoiseConfig {
            count: 5,
            seed: 12345,
            birdie_fraction: 0.4,
        };
        let paths_a = generate_noise_corpus(dir_a.path(), &config).unwrap();
        let paths_b = generate_noise_corpus(dir_b.path(), &config).unwrap();
        assert_eq!(paths_a.len(), 5);
        assert_eq!(paths_a.len(), paths_b.len());
        for (a, b) in paths_a.iter().zip(paths_b.iter()) {
            let bytes_a = std::fs::read(a).unwrap();
            let bytes_b = std::fs::read(b).unwrap();
            assert_eq!(
                bytes_a, bytes_b,
                "same seed must produce byte-identical WAVs ({a:?} vs {b:?})"
            );
        }
    }

    #[test]
    fn different_seed_produces_different_wavs() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let config_a = NoiseConfig {
            count: 2,
            seed: 111,
            birdie_fraction: 0.0,
        };
        let config_b = NoiseConfig {
            count: 2,
            seed: 222,
            birdie_fraction: 0.0,
        };
        let paths_a = generate_noise_corpus(dir_a.path(), &config_a).unwrap();
        let paths_b = generate_noise_corpus(dir_b.path(), &config_b).unwrap();
        let bytes_a = std::fs::read(&paths_a[0]).unwrap();
        let bytes_b = std::fs::read(&paths_b[0]).unwrap();
        assert_ne!(bytes_a, bytes_b, "different seeds must diverge");
    }

    #[test]
    fn no_birdie_rms_within_5_percent_of_target() {
        let dir = tempfile::tempdir().unwrap();
        let config = NoiseConfig {
            count: 3,
            seed: 999,
            birdie_fraction: 0.0,
        };
        let paths = generate_noise_corpus(dir.path(), &config).unwrap();
        for path in &paths {
            let mut reader = hound::WavReader::open(path).unwrap();
            let samples: Vec<f32> = reader
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect();
            let measured = rms(&samples);
            let rel_err = (measured - TARGET_NOISE_RMS).abs() / TARGET_NOISE_RMS;
            assert!(
                rel_err < 0.05,
                "RMS {measured} not within 5% of target {TARGET_NOISE_RMS} (rel_err={rel_err})"
            );
        }
    }

    #[test]
    fn birdie_fraction_selects_exact_count() {
        let flags = select_birdie_indices(20, 0.3, 42);
        assert_eq!(flags.len(), 20);
        assert_eq!(flags.iter().filter(|&&b| b).count(), 6); // round(20*0.3)=6
    }

    // ---------------------------------------------------------------------
    // Noise-generator statistical invariants (PAN-1).
    //
    // The existing tests above compare two runs *within the same build*, so
    // they pass vacuously across a `rand` major bump. These four assert
    // distribution and exact-count properties instead: the noise floor level,
    // actual Gaussianity, birdie energy, and Fisher-Yates selection counts.
    // Nothing here can only pass on one `rand` version.
    // ---------------------------------------------------------------------

    /// The generated noise floor must sit at `TARGET_NOISE_RMS` (0.03), the
    /// value the whole FP-on-noise metric is calibrated against.
    #[test]
    fn generated_noise_hits_target_rms() {
        // 180k samples → RMS relative std err ≈ 1/sqrt(2n) ≈ 0.17%; the 2%
        // bound is ~12σ.
        let samples = generate_one_wav(180_000, 7, false);
        let n = samples.len() as f64;
        let measured = (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / n).sqrt();
        assert!(
            (measured - TARGET_NOISE_RMS).abs() / TARGET_NOISE_RMS < 0.02,
            "noise RMS {measured} deviates >2% from TARGET_NOISE_RMS {TARGET_NOISE_RMS}"
        );
    }

    /// The AWGN must actually be Gaussian, not merely the right RMS — this is
    /// the assertion that catches a distribution regression across the `rand`
    /// major (e.g. a uniform generator with a matching variance would pass an
    /// RMS check and fail here).
    #[test]
    fn generated_noise_is_approximately_gaussian() {
        // Std err of a proportion at p≈0.68, n=180k is sqrt(p(1-p)/n) ≈ 0.0011,
        // so the 0.01 bound is ~9σ. A uniform distribution of the same variance
        // would give a 1σ fraction of 0.577 — 10× outside the tolerance.
        let samples = generate_one_wav(180_000, 11, false);
        let n = samples.len() as f64;
        let within = |k: f64| {
            samples
                .iter()
                .filter(|&&s| (s as f64).abs() < k * TARGET_NOISE_RMS)
                .count() as f64
                / n
        };
        let (one_sigma, two_sigma) = (within(1.0), within(2.0));
        assert!(
            (one_sigma - 0.6827).abs() < 0.01,
            "1σ fraction {one_sigma}, expected ≈0.6827"
        );
        assert!(
            (two_sigma - 0.9545).abs() < 0.01,
            "2σ fraction {two_sigma}, expected ≈0.9545"
        );
    }

    /// Birdie carriers sit 0..+20 dB over the noise floor, so their presence
    /// must raise total energy for the same seed. Even the weakest case (one
    /// carrier at 0 dB) adds in quadrature for a sqrt(2)× rise.
    #[test]
    fn birdies_raise_energy_above_pure_noise() {
        let plain = generate_one_wav(60_000, 4242, false);
        let birdied = generate_one_wav(60_000, 4242, true);
        let (plain_rms, birdied_rms) = (rms(&plain), rms(&birdied));
        assert!(
            birdied_rms > plain_rms * 1.05,
            "birdied RMS {birdied_rms} not meaningfully above plain RMS {plain_rms}"
        );
    }

    /// The Fisher-Yates selection must yield an EXACT count for every
    /// fraction, regardless of how the RNG lands. Extends
    /// `birdie_fraction_selects_exact_count` across the range, including both
    /// endpoints and a case where `round()` breaks a tie upward
    /// (round(7 * 0.5) = round(3.5) = 4).
    #[test]
    fn birdie_selection_count_is_exact_across_fractions() {
        for (count, frac, expected) in [
            (20, 0.0_f32, 0),
            (20, 0.25, 5),
            (20, 0.5, 10),
            (20, 1.0, 20),
            (7, 0.5, 4),
        ] {
            let flags = select_birdie_indices(count, frac, 31337);
            assert_eq!(flags.len(), count);
            assert_eq!(
                flags.iter().filter(|&&f| f).count(),
                expected,
                "count={count} frac={frac} produced the wrong number of birdies"
            );
        }
    }

    #[test]
    fn birdie_files_have_higher_rms_than_clean_noise() {
        // A birdie file has extra sinusoidal energy on top of the same
        // noise-floor RMS, so it must measure louder than a clean file
        // generated from a neighboring (birdie-free) seed with the same
        // config.
        let dir = tempfile::tempdir().unwrap();
        let config = NoiseConfig {
            count: 10,
            seed: 555,
            birdie_fraction: 1.0, // force every file to have a birdie
        };
        let paths = generate_noise_corpus(dir.path(), &config).unwrap();
        for path in &paths {
            let mut reader = hound::WavReader::open(path).unwrap();
            let samples: Vec<f32> = reader
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect();
            let measured = rms(&samples);
            assert!(
                measured > TARGET_NOISE_RMS * 1.05,
                "birdie file should measure louder than clean noise floor; got {measured}"
            );
        }
    }

    #[test]
    fn generate_noise_corpus_with_manifest_populates_sha256_and_entries() {
        let dir = tempfile::tempdir().unwrap();
        let config = NoiseConfig {
            count: 4,
            seed: 77,
            birdie_fraction: 0.5,
        };
        let manifest =
            generate_noise_corpus_with_manifest(dir.path(), config, "noise_test").unwrap();
        assert_eq!(manifest.entries.len(), 4);
        assert_eq!(manifest.entries.iter().filter(|e| e.has_birdie).count(), 2);
        for entry in &manifest.entries {
            assert_eq!(entry.wav_sha256.len(), 64, "sha256 hex must be 64 chars");
            assert!(entry.wav_path.is_absolute());
            assert!(entry.wav_path.exists());
        }
    }
}
