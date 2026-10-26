//! PAN-162 — per-slot impulsiveness instrument and per-day survey.
//!
//! hb-256's robust-noise LLR knee (`Ft8Config::impulse_robust_llr`) acts on
//! FT8 symbols (160 ms) whose broadband power jumps far above the rest of
//! the candidate. This module measures the same thing on raw audio, without
//! decoding, so whole station archives can be ranked by how impulsive
//! (lightning-static-affected) each capture day was.
//!
//! Definition (pre-registered in the PAN-162 plan; pilot table in
//! `research/experiments/*-pan162-impulse-llr-storm.md`):
//!   - P[b] = mean x² over each full 1920-sample block (one FT8 symbol at
//!     12 kHz); a trailing partial block is ignored;
//!   - ref[b] = median of P over a 25-block (≈4 s) window centred on b and
//!     shifted to stay inside the slot (never truncated at an edge);
//!   - block b is impulsive iff ref[b] > 0 and P[b] > 3·ref[b] (the k=3 knee);
//!   - the slot's `impulsive_block_fraction` is impulsive blocks / blocks.
//!
//! Two failure modes observed on real off-air audio drove this choice:
//!   - a slot-global median is fooled by slow band-wide level drift across
//!     the slot (2× swings within 15 s were measured), and per-bin spectral
//!     variants flag dense bands where hopping 8-FSK tones fill most bins;
//!   - a window truncated at the slot edge flags the next slot's
//!     transmissions starting at t ≥ 14.4 s.
//!
//! The metric's blind spot: in very busy bands a burst weaker than the
//! total signal power is under-counted. The block size is FT8-specific.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One FT8 symbol at 12 kHz (160 ms) — the time scale hb-256's knee acts on.
pub const IMPULSE_BLOCK_SAMPLES: usize = 1920;
/// Local reference: 25 blocks (≈4 s), shifted to stay inside the slot.
pub const IMPULSE_WINDOW_BLOCKS: usize = 25;
/// Block power above this multiple of the local median is impulsive (= k=3).
pub const IMPULSE_KNEE: f64 = 3.0;
/// Median block power below this (≈ −80 dBFS) marks a silent slot.
pub const SILENT_BLOCK_POWER: f64 = 1e-8;
/// The only sample rate the instrument is defined for.
pub const IMPULSE_SAMPLE_RATE: u32 = 12_000;

/// Per-slot impulsiveness.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlotImpulsiveness {
    pub blocks: usize,
    pub impulsive_blocks: usize,
    pub impulsive_block_fraction: f64,
    pub peak_excess_db: f64,
    pub silent: bool,
}

fn median_of(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    values[values.len() / 2]
}

/// Measures one slot. See the module doc for the definition.
pub fn slot_impulsiveness(samples: &[f32]) -> SlotImpulsiveness {
    let nb = samples.len() / IMPULSE_BLOCK_SAMPLES;
    let powers: Vec<f64> = samples
        .as_chunks::<IMPULSE_BLOCK_SAMPLES>()
        .0
        .iter()
        .map(|block| {
            let sum: f64 = block
                .iter()
                .map(|&x| {
                    let x = if x.is_finite() { x as f64 } else { 0.0 };
                    x * x
                })
                .sum();
            sum / IMPULSE_BLOCK_SAMPLES as f64
        })
        .collect();
    let silent = nb == 0 || median_of(&mut powers.clone()) < SILENT_BLOCK_POWER;
    let mut out = SlotImpulsiveness {
        blocks: nb,
        impulsive_blocks: 0,
        impulsive_block_fraction: 0.0,
        peak_excess_db: 0.0,
        silent,
    };
    if nb < 2 || silent {
        return out;
    }
    let w = IMPULSE_WINDOW_BLOCKS.min(nb);
    let mut window = Vec::with_capacity(w);
    for (b, &p) in powers.iter().enumerate() {
        let lo = b.saturating_sub(IMPULSE_WINDOW_BLOCKS / 2).min(nb - w);
        window.clear();
        window.extend_from_slice(&powers[lo..lo + w]);
        let reference = median_of(&mut window);
        if reference <= 0.0 {
            continue;
        }
        let ratio = p / reference;
        if ratio > IMPULSE_KNEE {
            out.impulsive_blocks += 1;
        }
        let excess_db = 10.0 * ratio.log10();
        if excess_db.is_finite() && excess_db > out.peak_excess_db {
            out.peak_excess_db = excess_db;
        }
    }
    out.impulsive_block_fraction = out.impulsive_blocks as f64 / nb as f64;
    out
}

fn valid_date(y: u32, m: u32, d: u32) -> bool {
    chrono::NaiveDate::from_ymd_opt(y as i32, m, d).is_some()
}

/// Capture day (`YYYYMMDD`) from a recording file name: pancetta's
/// `ft8_YYYYMMDD_HHMMSS[_band].wav` or WSJT-X's `YYMMDD_HHMMSS.wav`.
pub fn day_key(file_name: &str) -> Option<String> {
    let stem = file_name.rsplit(['/', '\\']).next()?;
    let stem = match stem.len().checked_sub(4) {
        Some(i) if stem.is_char_boundary(i) && stem[i..].eq_ignore_ascii_case(".wav") => &stem[..i],
        _ => stem,
    };
    let parts: Vec<&str> = stem.split('_').collect();
    let all_digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|c| c.is_ascii_digit());
    let num = |s: &str| s.parse::<u32>().ok();
    if parts.len() >= 3 && parts[0] == "ft8" && all_digits(parts[1], 8) && all_digits(parts[2], 6) {
        let d = parts[1];
        let (y, m, dd) = (num(&d[..4])?, num(&d[4..6])?, num(&d[6..])?);
        return valid_date(y, m, dd).then(|| d.to_string());
    }
    if parts.len() == 2 && all_digits(parts[0], 6) && all_digits(parts[1], 6) {
        let d = parts[0];
        let (y, m, dd) = (2000 + num(&d[..2])?, num(&d[2..4])?, num(&d[4..])?);
        return valid_date(y, m, dd).then(|| format!("{y:04}{m:02}{dd:02}"));
    }
    None
}

/// Outcome of reading one WAV for the instrument.
pub enum WavRead {
    Ok(Vec<f32>),
    WrongRate(u32),
    Unreadable(String),
}

/// Reads a mono WAV as f32 in [-1, 1] (16-bit int or float), refusing any
/// rate other than 12 kHz.
pub fn read_wav_12k(path: &Path) -> WavRead {
    let mut r = match hound::WavReader::open(path) {
        Ok(r) => r,
        Err(e) => return WavRead::Unreadable(e.to_string()),
    };
    let spec = r.spec();
    if spec.sample_rate != IMPULSE_SAMPLE_RATE {
        return WavRead::WrongRate(spec.sample_rate);
    }
    if spec.channels != 1 {
        return WavRead::Unreadable(format!("{} channels", spec.channels));
    }
    let samples: std::result::Result<Vec<f32>, _> = match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect(),
        hound::SampleFormat::Float => r.samples::<f32>().collect(),
    };
    match samples {
        Ok(s) => WavRead::Ok(s),
        Err(e) => WavRead::Unreadable(e.to_string()),
    }
}

/// The instrument's constants, recorded in every report and manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentInfo {
    pub block_samples: usize,
    pub window_blocks: usize,
    pub knee: f64,
    pub silent_block_power: f64,
    pub sample_rate: u32,
}

impl InstrumentInfo {
    pub fn current() -> Self {
        Self {
            block_samples: IMPULSE_BLOCK_SAMPLES,
            window_blocks: IMPULSE_WINDOW_BLOCKS,
            knee: IMPULSE_KNEE,
            silent_block_power: SILENT_BLOCK_POWER,
            sample_rate: IMPULSE_SAMPLE_RATE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WavImpulsiveness {
    pub path: String,
    pub day: Option<String>,
    pub sample_rate: u32,
    pub slot: SlotImpulsiveness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayImpulsiveness {
    pub day: String,
    /// Non-silent slots aggregated into this day.
    pub slots: usize,
    pub silent_slots: usize,
    pub mean_fraction: f64,
    pub p90_fraction: f64,
    /// Share of slots with at least one impulsive block.
    pub impulsive_slot_share: f64,
    pub max_fraction: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurveyReport {
    pub instrument: InstrumentInfo,
    /// Ranked: mean_fraction desc, impulsive_slot_share desc, day asc.
    pub days: Vec<DayImpulsiveness>,
    pub undated_slots: usize,
    pub skipped_wrong_rate: usize,
    pub unreadable: usize,
    pub silent_slots: usize,
    pub wavs: Vec<WavImpulsiveness>,
}

/// The day ranking rule.
pub fn rank_order(a: &DayImpulsiveness, b: &DayImpulsiveness) -> std::cmp::Ordering {
    b.mean_fraction
        .total_cmp(&a.mean_fraction)
        .then(b.impulsive_slot_share.total_cmp(&a.impulsive_slot_share))
        .then(a.day.cmp(&b.day))
}

/// Measures every path and aggregates by capture day. Unreadable, wrong-rate,
/// undated and silent files are counted, never fatal.
pub fn survey(paths: &[PathBuf]) -> SurveyReport {
    let mut report = SurveyReport {
        instrument: InstrumentInfo::current(),
        days: Vec::new(),
        undated_slots: 0,
        skipped_wrong_rate: 0,
        unreadable: 0,
        silent_slots: 0,
        wavs: Vec::new(),
    };
    let mut by_day: BTreeMap<String, (Vec<f64>, usize)> = BTreeMap::new();
    for path in paths {
        let samples = match read_wav_12k(path) {
            WavRead::Ok(s) => s,
            WavRead::WrongRate(_) => {
                report.skipped_wrong_rate += 1;
                continue;
            }
            WavRead::Unreadable(_) => {
                report.unreadable += 1;
                continue;
            }
        };
        let slot = slot_impulsiveness(&samples);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let day = day_key(name);
        if slot.silent {
            report.silent_slots += 1;
        }
        match &day {
            None => report.undated_slots += 1,
            Some(d) => {
                let entry = by_day.entry(d.clone()).or_default();
                if slot.silent {
                    entry.1 += 1;
                } else {
                    entry.0.push(slot.impulsive_block_fraction);
                }
            }
        }
        report.wavs.push(WavImpulsiveness {
            path: path.to_string_lossy().into_owned(),
            day,
            sample_rate: IMPULSE_SAMPLE_RATE,
            slot,
        });
    }
    for (day, (mut fractions, silent)) in by_day {
        if fractions.is_empty() {
            continue;
        }
        let n = fractions.len();
        let mean = fractions.iter().sum::<f64>() / n as f64;
        let share = fractions.iter().filter(|&&f| f > 0.0).count() as f64 / n as f64;
        fractions.sort_by(|a, b| a.total_cmp(b));
        let p90 = fractions[((n - 1) as f64 * 0.9).round() as usize];
        report.days.push(DayImpulsiveness {
            day,
            slots: n,
            silent_slots: silent,
            mean_fraction: mean,
            p90_fraction: p90,
            impulsive_slot_share: share,
            max_fraction: fractions[n - 1],
        });
    }
    report.days.sort_by(rank_order);
    report
}

impl SurveyReport {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("write {}", path.display()))
    }

    /// True when `days` follows the ranking rule.
    pub fn is_ranked(&self) -> bool {
        self.days
            .windows(2)
            .all(|w| rank_order(&w[0], &w[1]) != std::cmp::Ordering::Greater)
    }

    /// Markdown table, one row per ranked day.
    pub fn markdown_table(&self) -> String {
        let mut out = String::from(
            "| rank | day | slots | silent | mean IBF | p90 IBF | impulsive-slot share | max IBF |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        for (i, d) in self.days.iter().enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {:.4} | {:.4} | {:.3} | {:.4} |\n",
                i + 1,
                d.day,
                d.slots,
                d.silent_slots,
                d.mean_fraction,
                d.p90_fraction,
                d.impulsive_slot_share,
                d.max_fraction
            ));
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StormEntry {
    pub wav_path: String,
    pub wav_sha256: String,
    pub impulsive_block_fraction: f64,
}

/// Manifest of the stormiest days' slots, in the curated-manifest shape
/// (`entries[].wav_path` / `wav_sha256`) batch101 Part B and batch104 read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StormManifest {
    pub schema_version: u32,
    pub label: String,
    pub source: String,
    pub instrument: InstrumentInfo,
    pub entries: Vec<StormEntry>,
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// The slots of the `top_days` highest-ranked days whose fraction is at
/// least `min_fraction`, in chronological (path) order.
pub fn select_storm_entries(
    report: &SurveyReport,
    top_days: usize,
    min_fraction: f64,
) -> Result<Vec<StormEntry>> {
    let days: Vec<&str> = report
        .days
        .iter()
        .take(top_days)
        .map(|d| d.day.as_str())
        .collect();
    let mut picked: Vec<&WavImpulsiveness> = report
        .wavs
        .iter()
        .filter(|w| {
            !w.slot.silent
                && w.slot.impulsive_block_fraction >= min_fraction
                && w.day.as_deref().is_some_and(|d| days.contains(&d))
        })
        .collect();
    picked.sort_by(|a, b| {
        let name = |w: &WavImpulsiveness| {
            Path::new(&w.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        (a.day.clone(), name(a), &a.path).cmp(&(b.day.clone(), name(b), &b.path))
    });
    picked
        .into_iter()
        .map(|w| {
            Ok(StormEntry {
                wav_path: w.path.clone(),
                wav_sha256: sha256_file(Path::new(&w.path))?,
                impulsive_block_fraction: w.slot.impulsive_block_fraction,
            })
        })
        .collect()
}

/// Builds the storm manifest with its provenance line.
pub fn storm_manifest(
    report: &SurveyReport,
    label: &str,
    top_days: usize,
    min_fraction: f64,
) -> Result<StormManifest> {
    let i = InstrumentInfo::current();
    Ok(StormManifest {
        schema_version: 1,
        label: label.to_string(),
        source: format!(
            "pancetta-research impulse-survey (PAN-162): block={} samples, window={} blocks, \
             knee={}x local median; top {top_days} day(s) by mean impulsive-block fraction, \
             slots with fraction >= {min_fraction}",
            i.block_samples, i.window_blocks, i.knee
        ),
        instrument: i,
        entries: select_storm_entries(report, top_days, min_fraction)?,
    })
}

/// Recursive `*.wav` listing, sorted.
pub fn wavs_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("read_dir {}", d.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// `entries[].wav_path` of a curated manifest.
pub fn wavs_in_manifest(path: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let Some(entries) = v["entries"].as_array() else {
        bail!("{}: no entries array", path.display());
    };
    entries
        .iter()
        .map(|e| {
            e["wav_path"]
                .as_str()
                .map(PathBuf::from)
                .with_context(|| format!("{}: entry without wav_path", path.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::{add_impulse_bursts, gaussian_noise, ImpulseBurstPlant};
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    const WINDOW: usize = 151_680;

    fn awgn(seed: u64, n: usize) -> Vec<f32> {
        gaussian_noise(&mut StdRng::seed_from_u64(seed), n, 0.03)
    }

    #[test]
    fn pure_awgn_has_no_impulsive_blocks() {
        for seed in 0..8 {
            for n in [WINDOW, 180_000] {
                let m = slot_impulsiveness(&awgn(seed, n));
                assert_eq!(m.impulsive_blocks, 0, "seed {seed} n {n}");
                assert!(!m.silent);
            }
        }
    }

    #[test]
    fn steady_tone_with_silent_edges_is_not_impulsive() {
        let mut x = awgn(1, WINDOW);
        // +10 dB over σ=0.03 noise: amplitude √(2·10)·σ.
        let amp = (20.0f32).sqrt() * 0.03;
        for (i, s) in x.iter_mut().enumerate() {
            let t = i as f32 / 12_000.0;
            if (0.5..13.14).contains(&t) {
                *s += amp * (2.0 * std::f32::consts::PI * 1500.0 * t).sin();
            }
        }
        assert_eq!(slot_impulsiveness(&x).impulsive_blocks, 0);
    }

    #[test]
    fn slow_floor_ramp_is_not_impulsive() {
        let mut x = awgn(2, WINDOW);
        let n = x.len() as f32;
        for (i, s) in x.iter_mut().enumerate() {
            *s *= 10f32.powf(6.0 * (i as f32 / n) / 20.0);
        }
        assert_eq!(slot_impulsiveness(&x).impulsive_blocks, 0);
    }

    /// The pilot's truncated-window failure on real audio: the slot's own
    /// signals end at 13.14 s, the band is quiet, then the next slot's
    /// transmissions start in the last blocks of a 15 s capture. A window
    /// truncated at the edge sees mostly gap and flags the onset; the
    /// in-slot 25-block window still sees the slot's signal level.
    #[test]
    fn next_slot_onset_is_not_impulsive() {
        let mut x = awgn(3, 180_000);
        let nb = x.len() / IMPULSE_BLOCK_SAMPLES;
        let amp = (20.0f32).sqrt() * 0.03;
        let onset = (nb - 3) * IMPULSE_BLOCK_SAMPLES;
        for (i, s) in x.iter_mut().enumerate() {
            let t = i as f32 / 12_000.0;
            if (0.5..13.14).contains(&t) || i >= onset {
                *s += amp * (2.0 * std::f32::consts::PI * 1500.0 * t).sin();
            }
        }
        assert_eq!(slot_impulsiveness(&x).impulsive_blocks, 0);
    }

    fn hand_bursts() -> Vec<f32> {
        let mut x = awgn(4, WINDOW);
        let mut rng = StdRng::seed_from_u64(99);
        let sigma = 0.03 * 10f32.powf(15.0 / 20.0);
        for b in [10usize, 40, 41] {
            let burst = gaussian_noise(&mut rng, IMPULSE_BLOCK_SAMPLES, sigma);
            for (s, v) in x[b * IMPULSE_BLOCK_SAMPLES..(b + 1) * IMPULSE_BLOCK_SAMPLES]
                .iter_mut()
                .zip(burst)
            {
                *s += v;
            }
        }
        x
    }

    #[test]
    fn hand_placed_bursts_are_counted() {
        let m = slot_impulsiveness(&hand_bursts());
        assert_eq!(m.blocks, 79);
        assert_eq!(m.impulsive_blocks, 3);
        assert!((m.impulsive_block_fraction - 3.0 / 79.0).abs() < 1e-12);
        let expected = 10.0 * (1.0 + 10f64.powf(1.5)).log10();
        assert!(
            (m.peak_excess_db - expected).abs() < 0.5,
            "peak {} vs {expected}",
            m.peak_excess_db
        );
    }

    #[test]
    fn instrument_is_gain_invariant() {
        let x = hand_bursts();
        let base = slot_impulsiveness(&x).impulsive_blocks;
        for g in [0.01f32, 10.0] {
            let y: Vec<f32> = x.iter().map(|s| s * g).collect();
            assert_eq!(slot_impulsiveness(&y).impulsive_blocks, base, "gain {g}");
        }
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        let empty = slot_impulsiveness(&[]);
        assert_eq!((empty.blocks, empty.impulsive_blocks), (0, 0));
        assert_eq!(empty.impulsive_block_fraction, 0.0);
        let short = slot_impulsiveness(&awgn(5, IMPULSE_BLOCK_SAMPLES + 10));
        assert_eq!((short.blocks, short.impulsive_blocks), (1, 0));
        assert_eq!(short.impulsive_block_fraction, 0.0);
        let zeros = slot_impulsiveness(&vec![0.0; WINDOW]);
        assert!(zeros.silent);
        assert_eq!(zeros.impulsive_blocks, 0);
        for m in [empty, short, zeros] {
            assert!(!m.impulsive_block_fraction.is_nan() && !m.peak_excess_db.is_nan());
        }
    }

    #[test]
    fn day_keys() {
        assert_eq!(
            day_key("ft8_20260613_201700.wav").as_deref(),
            Some("20260613")
        );
        assert_eq!(
            day_key("ft8_20260613_201700_20m.wav").as_deref(),
            Some("20260613")
        );
        assert_eq!(day_key("191111_110115.wav").as_deref(), Some("20191111"));
        assert_eq!(day_key("191111_110115.WAV").as_deref(), Some("20191111"));
        assert_eq!(day_key("test_01.wav"), None);
        assert_eq!(day_key("ft8_2026061_201700.wav"), None);
        assert_eq!(day_key("000000_000001.wav"), None);
    }

    fn write_wav(path: &Path, samples: &[f32], rate: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
    }

    fn plant_slot(seed: u64, plant: Option<&ImpulseBurstPlant>) -> Vec<f32> {
        let mut rng = StdRng::seed_from_u64(seed);
        let n = 79 * IMPULSE_BLOCK_SAMPLES;
        let mut x = gaussian_noise(&mut rng, n, 0.01);
        if let Some(p) = plant {
            add_impulse_bursts(&mut rng, &mut x, 0.01, p);
        }
        x
    }

    fn three_days(dir: &Path) -> Vec<PathBuf> {
        let plants = [
            ("20260701", None),
            ("20260702", Some(&ImpulseBurstPlant::BATCH101)),
            ("20260703", Some(&ImpulseBurstPlant::STORM)),
        ];
        let mut paths = Vec::new();
        for (di, (day, plant)) in plants.iter().enumerate() {
            for s in 0..4u64 {
                let p = dir.join(format!("ft8_{day}_0000{:02}.wav", s * 15));
                write_wav(&p, &plant_slot(100 * di as u64 + s, *plant), 12_000);
                paths.push(p);
            }
        }
        paths
    }

    #[test]
    fn survey_ranks_days_by_mean_fraction() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = three_days(tmp.path());
        let report = survey(&paths);
        let order: Vec<&str> = report.days.iter().map(|d| d.day.as_str()).collect();
        assert_eq!(order, ["20260703", "20260702", "20260701"]);
        assert!(report.is_ranked());
        for d in &report.days {
            let fr: Vec<f64> = report
                .wavs
                .iter()
                .filter(|w| w.day.as_deref() == Some(d.day.as_str()))
                .map(|w| w.slot.impulsive_block_fraction)
                .collect();
            assert_eq!(fr.len(), 4);
            let mean = fr.iter().sum::<f64>() / 4.0;
            assert!((d.mean_fraction - mean).abs() < 1e-12);
        }
    }

    #[test]
    fn survey_breaks_ties_by_share_then_day() {
        let tmp = tempfile::TempDir::new().unwrap();
        let x = plant_slot(7, Some(&ImpulseBurstPlant::STORM));
        let mut paths = Vec::new();
        for day in ["20260802", "20260801"] {
            let p = tmp.path().join(format!("ft8_{day}_000000.wav"));
            write_wav(&p, &x, 12_000);
            paths.push(p);
        }
        let report = survey(&paths);
        let order: Vec<&str> = report.days.iter().map(|d| d.day.as_str()).collect();
        assert_eq!(order, ["20260801", "20260802"]);
        // Share breaks a mean tie before the day does.
        let mk = |day: &str, share: f64| DayImpulsiveness {
            day: day.into(),
            slots: 1,
            silent_slots: 0,
            mean_fraction: 0.05,
            p90_fraction: 0.05,
            impulsive_slot_share: share,
            max_fraction: 0.05,
        };
        let mut days = [mk("20260801", 0.5), mk("20260802", 1.0)];
        days.sort_by(rank_order);
        assert_eq!(days[0].day, "20260802");
    }

    #[test]
    fn survey_counts_exclusions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let noise = plant_slot(1, None);
        let undated = dir.join("test_01.wav");
        write_wav(&undated, &noise, 12_000);
        let wrong_rate = dir.join("ft8_20260701_000000.wav");
        write_wav(&wrong_rate, &noise[..96_000], 6_400);
        let corrupt = dir.join("ft8_20260701_000015.wav");
        std::fs::write(&corrupt, b"RIFF\x10\x00\x00\x00WAVEjunk").unwrap();
        let silent = dir.join("ft8_20260701_000030.wav");
        write_wav(&silent, &vec![0.0; noise.len()], 12_000);
        let good = dir.join("ft8_20260701_000045.wav");
        write_wav(&good, &noise, 12_000);
        let report = survey(&[undated, wrong_rate, corrupt, silent, good]);
        assert_eq!(report.undated_slots, 1);
        assert_eq!(report.skipped_wrong_rate, 1);
        assert_eq!(report.unreadable, 1);
        assert_eq!(report.silent_slots, 1);
        assert_eq!(report.days.len(), 1);
        assert_eq!(report.days[0].slots, 1);
        assert_eq!(report.days[0].silent_slots, 1);
    }

    #[test]
    fn storm_manifest_selects_top_day_slots() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = three_days(tmp.path());
        let report = survey(&paths);
        let manifest = storm_manifest(&report, "storm_top1", 1, 0.02).unwrap();
        let expected: Vec<&WavImpulsiveness> = report
            .wavs
            .iter()
            .filter(|w| {
                w.day.as_deref() == Some("20260703") && w.slot.impulsive_block_fraction >= 0.02
            })
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(manifest.entries.len(), expected.len());
        for (e, w) in manifest.entries.iter().zip(&expected) {
            assert_eq!(e.wav_path, w.path);
            assert_eq!(e.wav_sha256, sha256_file(Path::new(&w.path)).unwrap());
        }
        let mut sorted = manifest.entries.clone();
        sorted.sort_by(|a, b| a.wav_path.cmp(&b.wav_path));
        assert_eq!(sorted, manifest.entries);
        let v: serde_json::Value = serde_json::to_value(&manifest).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert!(v["source"].as_str().unwrap().contains("knee=3x"));
        assert!(v["instrument"]["block_samples"] == 1920);
        assert!(v["entries"][0]["wav_path"].as_str().is_some());
        assert_eq!(v["entries"][0]["wav_sha256"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn survey_report_round_trips_and_renders() {
        let tmp = tempfile::TempDir::new().unwrap();
        let report = survey(&three_days(tmp.path()));
        let path = tmp.path().join("report.json");
        report.save(&path).unwrap();
        // serde_json float parsing is not guaranteed bit-exact; compare
        // structure exactly and floats to 1e-12.
        let loaded = SurveyReport::load(&path).unwrap();
        assert_eq!(loaded.instrument, report.instrument);
        assert_eq!(loaded.wavs.len(), report.wavs.len());
        assert_eq!(loaded.days.len(), report.days.len());
        for (a, b) in loaded.days.iter().zip(&report.days) {
            assert_eq!(
                (&a.day, a.slots, a.silent_slots),
                (&b.day, b.slots, b.silent_slots)
            );
            assert!((a.mean_fraction - b.mean_fraction).abs() < 1e-12);
            assert!((a.impulsive_slot_share - b.impulsive_slot_share).abs() < 1e-12);
        }
        assert!(loaded.is_ranked());
        let table = report.markdown_table();
        assert_eq!(table.lines().count(), 2 + report.days.len());
        assert_eq!(wavs_under(tmp.path()).unwrap().len(), 12);
    }
}
