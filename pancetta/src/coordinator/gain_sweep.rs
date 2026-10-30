//! PAN-244 receive-gain experiment measurements and conservative inference.
//!
//! All time coordinates are originating UTC slot starts in seconds, never decode
//! receipt times. PSD input is settled mono 12 kHz audio, with capture lead removed.

use chrono::{DateTime, Utc};
use rustfft::{num_complex::Complex, FftPlanner};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const MIN_WINDOWS: usize = 6;
const MIN_SENDERS: usize = 3;
const MIN_PAIRS: usize = 6;
const MAX_DISTANCE: i64 = 120;
const PLATEAU_DB: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GainSetting {
    pub preamp_db: i32,
    pub attenuator_db: i32,
}

pub fn candidates(preamp: &[i32], att: &[i32]) -> Result<Vec<GainSetting>, String> {
    if !preamp.contains(&0) || !att.contains(&0) || preamp.iter().chain(att).any(|x| *x < 0) {
        return Err("invalid receive-gain capabilities".into());
    }
    let mut result: Vec<_> = preamp
        .iter()
        .map(|p| GainSetting {
            preamp_db: *p,
            attenuator_db: 0,
        })
        .chain(att.iter().filter(|a| **a > 0).map(|a| GainSetting {
            preamp_db: 0,
            attenuator_db: *a,
        }))
        .collect();
    result.sort_by_key(|g| (g.preamp_db - g.attenuator_db, g.preamp_db, g.attenuator_db));
    result.dedup();
    if result.len() < 2 {
        return Err("fewer than two distinct receive-gain settings".into());
    }
    Ok(result)
}

/// Balanced cycles. Call only for admitted receive slots; skipped TX does not
/// advance the experiment. The explicit two-setting gap changes source parity.
#[derive(Clone)]
pub struct Schedule {
    settings: Vec<GainSetting>,
    counts: Vec<[usize; 2]>,
    cycle: Vec<usize>,
    cursor: usize,
    last: Option<usize>,
    quiet: bool,
    start: usize,
}

impl Schedule {
    pub fn new(settings: Vec<GainSetting>) -> Self {
        Self {
            counts: vec![[0; 2]; settings.len()],
            settings,
            cycle: Vec::new(),
            cursor: 0,
            last: None,
            quiet: false,
            start: 0,
        }
    }
    pub fn next(&mut self, slot: i64) -> Option<GainSetting> {
        if self.settings.len() < 2 {
            return None;
        }
        if self.quiet {
            self.quiet = false;
            return None;
        }
        let parity = (slot.div_euclid(15).rem_euclid(2)) as usize;
        if self.cursor == self.cycle.len() {
            let n = self.settings.len();
            // Prefer the least observed setting/parity, then cyclic rotation.
            // Ordered index is the final deterministic tie-break.
            let start = (0..n)
                .filter(|i| Some(*i) != self.last)
                .min_by_key(|i| (self.counts[*i][parity], (*i + n - self.start) % n, *i))?;
            self.cycle = (0..n).map(|i| (start + i) % n).collect();
            self.cursor = 0;
            self.start = (start + 1) % n;
        }
        let i = self.cycle[self.cursor];
        self.cursor += 1;
        self.counts[i][parity] += 1;
        self.last = Some(i);
        if self.settings.len() == 2 && self.cursor == 2 {
            self.quiet = true;
        }
        Some(self.settings[i])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationSnr {
    pub callsign: String,
    pub snr_db: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub slot: i64,
    pub generation: u64,
    pub gain_epoch: u64,
    pub gain: GainSetting,
    pub noise_dbfs_per_hz: f64,
    pub stations: Vec<StationSnr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedWindow {
    pub slot: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingSummary {
    pub gain: GainSetting,
    pub valid_windows: usize,
    pub noise_dbfs_per_hz: Option<f64>,
    pub matched_snr_db: Option<f64>,
    pub unique_stations: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comparison {
    pub lower: GainSetting,
    pub higher: GainSetting,
    pub matched_stations: usize,
    pub matched_pairs: usize,
    pub snr_delta_db: Option<f64>,
    pub noise_delta_db: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameters {
    pub min_windows: usize,
    pub min_matched_senders: usize,
    pub min_matched_pairs: usize,
    pub max_pair_distance_seconds: i64,
    pub plateau_db: f64,
    pub normalization: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u8,
    pub band: String,
    pub dial_hz: u64,
    pub mode: String,
    pub started_utc: DateTime<Utc>,
    pub ended_utc: DateTime<Utc>,
    pub requested_duration_seconds: u64,
    pub original_gain: GainSetting,
    pub settings: Vec<SettingSummary>,
    pub observations: Vec<Observation>,
    pub skipped: Vec<SkippedWindow>,
    pub comparisons: Vec<Comparison>,
    pub status: String,
    pub reasons: Vec<String>,
    pub restoration: String,
    pub recommendation: Option<GainSetting>,
    pub parameters: Parameters,
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|x| x.is_finite());
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some(if n.is_multiple_of(2) {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    } else {
        values[n / 2]
    })
}

fn resolved_call(call: &str) -> Option<String> {
    let call = call.trim().to_ascii_uppercase();
    if call.len() < 3
        || call.contains(['<', '>'])
        || !call.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'/')
        || !call.bytes().any(|b| b.is_ascii_alphabetic())
        || !call.bytes().any(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(call)
}

/// One SNR per sender/source window. Hashes and non-finite data never match.
fn stations(o: &Observation) -> BTreeMap<String, f64> {
    let mut grouped: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for s in &o.stations {
        if let Some(call) = resolved_call(&s.callsign) {
            if s.snr_db.is_finite() {
                grouped.entry(call).or_default().push(s.snr_db);
            }
        }
    }
    grouped
        .into_iter()
        .filter_map(|(c, s)| median(s).map(|v| (c, v)))
        .collect()
}

impl Report {
    pub fn new(
        band: String,
        dial_hz: u64,
        duration: u64,
        original_gain: GainSetting,
        candidates: Vec<GainSetting>,
    ) -> Self {
        let now = Utc::now();
        Self { schema_version: 1, band, dial_hz, mode: "FT8".into(), started_utc: now, ended_utc: now,
            requested_duration_seconds: duration, original_gain,
            settings: candidates.into_iter().map(|gain| SettingSummary { gain, valid_windows: 0, noise_dbfs_per_hz: None, matched_snr_db: None, unique_stations: 0 }).collect(),
            observations: Vec::new(), skipped: Vec::new(), comparisons: Vec::new(), status: "inconclusive".into(), reasons: Vec::new(), restoration: "pending".into(), recommendation: None,
            parameters: Parameters { min_windows: MIN_WINDOWS, min_matched_senders: MIN_SENDERS, min_matched_pairs: MIN_PAIRS, max_pair_distance_seconds: MAX_DISTANCE, plateau_db: PLATEAU_DB,
                normalization: "12 kHz mono; 2048-point periodic Hann; 50% overlap; one-sided PSD = 2*|FFT|^2/(12000*sum(Hann^2)); full-scale sine has integrated power 0.5 (-3.0103 dBFS); median positive cells in 100-300 and 2500-2900 Hz, excluding decoded frequencies +/-50 Hz; >=20 bins; clipping >=0.1% at abs(sample)>=0.999 is invalid".into() }
        }
    }

    pub fn finish(&mut self, restored: bool, failure: Option<String>) {
        self.ended_utc = Utc::now();
        self.recommendation = None;
        self.reasons.clear();
        self.comparisons.clear();
        self.restoration = if restored { "confirmed" } else { "failed" }.into();
        // Exclude malformed windows and duplicate source slots even if a caller
        // accidentally retries completion delivery. Never count pipeline loss as silence.
        let mut seen = BTreeSet::new();
        let valid: Vec<_> = self
            .observations
            .iter()
            .filter(|o| o.noise_dbfs_per_hz.is_finite() && seen.insert((o.generation, o.slot)))
            .collect();
        let station_maps: Vec<_> = valid.iter().map(|o| stations(o)).collect();
        let mut heard: BTreeMap<String, BTreeSet<GainSetting>> = BTreeMap::new();
        for (o, map) in valid.iter().zip(&station_maps) {
            for call in map.keys() {
                heard.entry(call.clone()).or_default().insert(o.gain);
            }
        }
        for s in &mut self.settings {
            let mut snrs: BTreeMap<String, Vec<f64>> = BTreeMap::new();
            let mut floors = Vec::new();
            for (o, map) in valid
                .iter()
                .zip(&station_maps)
                .filter(|(o, _)| o.gain == s.gain)
            {
                floors.push(o.noise_dbfs_per_hz);
                for (c, v) in map {
                    snrs.entry(c.clone()).or_default().push(*v);
                }
            }
            s.valid_windows = floors.len();
            s.noise_dbfs_per_hz = median(floors);
            s.unique_stations = snrs.len();
            s.matched_snr_db = median(
                snrs.into_iter()
                    .filter(|(c, _)| heard[c].len() > 1)
                    .filter_map(|(_, v)| median(v))
                    .collect(),
            );
        }
        for low in 0..self.settings.len() {
            for high in low + 1..self.settings.len() {
                let lower = self.settings[low].gain;
                let higher = self.settings[high].gain;
                let mut deltas: BTreeMap<String, Vec<f64>> = BTreeMap::new();
                let mut paired_windows = BTreeSet::new();
                for call in heard.keys() {
                    let mut used = BTreeSet::new();
                    let mut lowers: Vec<_> = valid
                        .iter()
                        .enumerate()
                        .filter(|(i, o)| o.gain == lower && station_maps[*i].contains_key(call))
                        .collect();
                    lowers.sort_by_key(|(_, o)| o.slot);
                    for (li, l) in lowers {
                        let h = valid
                            .iter()
                            .enumerate()
                            .filter(|(i, o)| {
                                o.gain == higher
                                    && !used.contains(i)
                                    && station_maps[*i].contains_key(call)
                                    && l.slot.div_euclid(15).rem_euclid(2)
                                        == o.slot.div_euclid(15).rem_euclid(2)
                                    && (l.slot - o.slot).abs() <= MAX_DISTANCE
                            })
                            .min_by_key(|(_, o)| ((l.slot - o.slot).abs(), o.slot));
                        if let Some((hi, _)) = h {
                            used.insert(hi);
                            paired_windows.insert((li, hi));
                            deltas
                                .entry(call.clone())
                                .or_default()
                                .push(station_maps[hi][call] - station_maps[li][call]);
                        }
                    }
                }
                let lower_floor = median(
                    paired_windows
                        .iter()
                        .map(|(l, _)| valid[*l].noise_dbfs_per_hz)
                        .collect(),
                );
                let higher_floor = median(
                    paired_windows
                        .iter()
                        .map(|(_, h)| valid[*h].noise_dbfs_per_hz)
                        .collect(),
                );
                self.comparisons.push(Comparison {
                    lower,
                    higher,
                    matched_stations: deltas.len(),
                    matched_pairs: paired_windows.len(),
                    snr_delta_db: median(deltas.into_values().filter_map(median).collect()),
                    noise_delta_db: lower_floor.zip(higher_floor).map(|(l, h)| h - l),
                });
            }
        }
        self.status = "inconclusive".into();
        if !restored {
            self.status = "failed".into();
            self.reasons
                .push("original gain settings could not be restored".into());
        } else if let Some(reason) = failure {
            self.status = if reason == "interrupted" {
                "interrupted"
            } else {
                "failed"
            }
            .into();
            self.reasons.push(reason);
        } else if self.settings.len() < 2
            || self.settings.iter().any(|s| s.valid_windows < MIN_WINDOWS)
        {
            self.reasons
                .push("fewer than 6 valid windows for at least one setting".into());
        } else {
            // The highest candidate cannot demonstrate a plateau by itself.
            let mut missing = false;
            for s in self.settings.iter().take(self.settings.len() - 1) {
                let comparisons: Vec<_> = self
                    .comparisons
                    .iter()
                    .filter(|c| c.lower == s.gain)
                    .collect();
                if comparisons.iter().any(|c| {
                    c.matched_stations < MIN_SENDERS
                        || c.matched_pairs < MIN_PAIRS
                        || c.snr_delta_db.is_none()
                }) {
                    missing = true;
                    continue;
                }
                if comparisons
                    .iter()
                    .all(|c| c.snr_delta_db.is_some_and(|d| d <= PLATEAU_DB))
                {
                    self.recommendation = Some(s.gain);
                    self.status = "recommended".into();
                    self.reasons
                        .push("higher gain improved matched SNR by at most 1.0 dB".into());
                    return;
                }
            }
            self.reasons.push(
                if missing {
                    "insufficient matched-station comparisons"
                } else {
                    "no SNR plateau was measured"
                }
                .into(),
            );
        }
    }
}

/// Raw one-sided periodogram, with no waterfall normalization or audio AGC.
/// Samples are full-scale floats. A unit-amplitude sine integrates to 0.5.
pub fn noise_floor(samples: &[f32], decoded_hz: &[f32]) -> Result<f64, String> {
    const N: usize = 2048;
    const FS: f64 = 12000.0;
    if samples.len() < N || samples.iter().any(|s| !s.is_finite()) {
        return Err("missing or non-finite audio".into());
    }
    if samples.iter().filter(|s| s.abs() >= 0.999).count() * 1000 >= samples.len() {
        return Err("clipped audio".into());
    }
    let bins: Vec<_> = (1..N / 2)
        .filter(|k| {
            let hz = *k as f64 * FS / N as f64;
            ((100.0..=300.0).contains(&hz) || (2500.0..=2900.0).contains(&hz))
                && decoded_hz
                    .iter()
                    .all(|d| !d.is_finite() || (hz - *d as f64).abs() > 50.0)
        })
        .collect();
    if bins.len() < 20 {
        return Err("fewer than 20 unmasked noise bins".into());
    }
    let hann: Vec<_> = (0..N)
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / N as f64).cos())
        .collect();
    let norm = FS * hann.iter().map(|w| w * w).sum::<f64>();
    let fft = FftPlanner::new().plan_fft_forward(N);
    let mut powers = Vec::new();
    for start in (0..=samples.len() - N).step_by(N / 2) {
        let psd = periodogram(&samples[start..start + N], &hann, norm, fft.as_ref());
        for k in &bins {
            let p = psd[*k];
            if p.is_finite() && p > 0.0 {
                powers.push(p);
            }
        }
    }
    median(powers)
        .filter(|p| *p > 0.0)
        .map(|p| 10.0 * p.log10())
        .ok_or_else(|| "no positive finite noise power".into())
}

fn periodogram(samples: &[f32], hann: &[f64], norm: f64, fft: &dyn rustfft::Fft<f64>) -> Vec<f64> {
    let mut frame: Vec<_> = samples
        .iter()
        .zip(hann)
        .map(|(s, w)| Complex::new(*s as f64 * w, 0.0))
        .collect();
    fft.process(&mut frame);
    let n = frame.len();
    (0..=n / 2)
        .map(|k| frame[k].norm_sqr() / norm * if k == 0 || k == n / 2 { 1.0 } else { 2.0 })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gs(p: i32, a: i32) -> GainSetting {
        GainSetting {
            preamp_db: p,
            attenuator_db: a,
        }
    }
    fn fixture(gains: &[GainSetting], snrs: &[f64], windows: usize) -> Report {
        let mut r = Report::new("20m".into(), 14_074_000, 600, gs(0, 0), gains.to_vec());
        for window in 0..windows {
            for (i, g) in gains.iter().enumerate() {
                r.observations.push(Observation {
                    slot: window as i64 * 120 + i as i64 * 30,
                    generation: 1,
                    gain_epoch: i as u64,
                    gain: *g,
                    noise_dbfs_per_hz: -100.0 + i as f64 * 10.0,
                    stations: ["K1ABC", "W2XYZ", "JA1AAA"]
                        .into_iter()
                        .map(|c| StationSnr {
                            callsign: c.into(),
                            snr_db: snrs[i],
                        })
                        .collect(),
                });
            }
        }
        r
    }
    #[test]
    fn gain_sweep_candidates_are_sorted_deduplicated_non_cartesian() {
        assert_eq!(
            candidates(&[20, 0, 10, 10], &[6, 0, 12]).unwrap(),
            vec![gs(0, 12), gs(0, 6), gs(0, 0), gs(10, 0), gs(20, 0)]
        );
        assert!(candidates(&[10], &[0]).is_err());
        assert!(candidates(&[0], &[0]).is_err());
        assert!(candidates(&[0, -10], &[0]).is_err());
    }
    #[test]
    fn gain_sweep_40_windows_balanced_and_both_source_parities() {
        let settings = candidates(&[0, 10, 20], &[0, 6]).unwrap();
        let mut schedule = Schedule::new(settings.clone());
        let mut counts = BTreeMap::new();
        let mut last = None;
        for slot in 0..40 {
            let g = schedule.next(slot * 15).unwrap();
            assert_ne!(last, Some(g));
            last = Some(g);
            *counts.entry((g, slot % 2)).or_insert(0) += 1;
        }
        for g in settings {
            assert_eq!(counts[&(g, 0)] + counts[&(g, 1)], 10);
            assert!(counts[&(g, 0)] > 0 && counts[&(g, 1)] > 0);
        }
    }
    #[test]
    fn gain_sweep_two_settings_have_explicit_parity_gap() {
        let settings = vec![gs(0, 0), gs(10, 0)];
        let mut schedule = Schedule::new(settings.clone());
        let mut counts = BTreeMap::new();
        for slot in 0..12 {
            match schedule.next(slot * 15) {
                Some(g) => *counts.entry((g, slot % 2)).or_insert(0) += 1,
                None => assert_eq!(slot % 3, 2),
            }
        }
        for g in settings {
            assert!(counts[&(g, 0)] > 0 && counts[&(g, 1)] > 0);
        }
    }
    #[test]
    fn gain_sweep_tx_heavy_does_not_extend_elapsed_experiment() {
        let gains = vec![gs(0, 0), gs(10, 0)];
        let mut r = fixture(&gains, &[-10.0, -10.0], 6);
        r.observations.retain(|o| o.slot < 200);
        r.finish(true, None);
        assert_eq!(r.status, "inconclusive");
        assert!(r.recommendation.is_none());
    }
    #[test]
    fn gain_sweep_five_vs_six_and_no_vacuous_highest() {
        let gains = vec![gs(0, 0), gs(10, 0)];
        let mut r = fixture(&gains, &[-10.0, -9.8], 5);
        r.finish(true, None);
        assert!(r.recommendation.is_none());
        let mut r = fixture(&gains, &[-10.0, -9.8], 6);
        r.finish(true, None);
        assert_eq!(r.recommendation, Some(gains[0]));
        assert_eq!(r.comparisons[0].matched_stations, 3);
        assert_eq!(r.comparisons[0].matched_pairs, 6);
        assert!((r.comparisons[0].snr_delta_db.unwrap() - 0.2).abs() < 1e-10);
        assert_eq!(r.comparisons[0].noise_delta_db, Some(10.0));
        let mut r = fixture(&gains, &[-10.0, -6.0], 6);
        r.finish(true, None);
        assert!(r.recommendation.is_none());
        assert_eq!(r.reasons, ["no SNR plateau was measured"]);
    }
    #[test]
    fn gain_sweep_compares_every_higher_and_negative_benefit() {
        let gains = vec![gs(0, 0), gs(10, 0), gs(20, 0)];
        let mut r = fixture(&gains, &[-10.0, -9.8, -5.0], 6);
        r.finish(true, None);
        assert!(r.recommendation.is_none());
        assert_eq!(r.comparisons.len(), 3);
        let mut r = fixture(&gains, &[-10.0, -10.2, -11.0], 6);
        r.finish(true, None);
        assert_eq!(r.recommendation, Some(gains[0]));
    }
    #[test]
    fn gain_sweep_no_signal_missing_senders_and_wrong_parity_never_recommend() {
        let gains = vec![gs(0, 0), gs(10, 0)];
        let mut r = fixture(&gains, &[-10.0, -10.0], 6);
        for o in &mut r.observations {
            o.stations.clear();
        }
        r.finish(true, None);
        assert!(r.recommendation.is_none());
        assert_eq!(r.settings[0].valid_windows, 6);
        assert_eq!(r.settings[0].unique_stations, 0);
        assert!(r.settings[0].matched_snr_db.is_none());
        let mut r = fixture(&gains, &[-10.0, -10.0], 6);
        for o in &mut r.observations {
            o.stations.truncate(2);
        }
        r.finish(true, None);
        assert!(r.recommendation.is_none());
        let mut r = fixture(&gains, &[-10.0, -10.0], 6);
        for o in &mut r.observations {
            if o.gain == gains[1] {
                o.slot += 15;
            }
        }
        r.finish(true, None);
        assert!(r.recommendation.is_none());
    }
    #[test]
    fn gain_sweep_three_senders_in_two_windows_are_not_six_window_pairs() {
        let gains = vec![gs(0, 0), gs(10, 0)];
        let mut r = fixture(&gains, &[-10.0, -10.0], 6);
        for o in &mut r.observations {
            if o.slot >= 240 {
                o.stations.clear();
            }
        }
        r.finish(true, None);
        assert_eq!(r.settings[0].valid_windows, 6);
        assert_eq!(r.comparisons[0].matched_stations, 3);
        assert_eq!(r.comparisons[0].matched_pairs, 2);
        assert!(r.recommendation.is_none());
    }
    #[test]
    fn gain_sweep_interruption_or_restoration_failure_removes_recommendation() {
        let gains = vec![gs(0, 0), gs(10, 0)];
        let mut r = fixture(&gains, &[-10.0, -10.0], 6);
        r.finish(true, None);
        assert!(r.recommendation.is_some());
        r.finish(true, Some("interrupted".into()));
        assert!(r.recommendation.is_none());
        assert_eq!(r.status, "interrupted");
        r.finish(false, None);
        assert!(r.recommendation.is_none());
        assert_eq!(r.status, "failed");
    }
    #[test]
    fn gain_sweep_duplicates_sender_median_and_hashes_excluded() {
        let gains = vec![gs(0, 0), gs(10, 0)];
        let mut r = fixture(&gains, &[-10.0, -10.0], 6);
        for o in &mut r.observations {
            o.stations.push(StationSnr {
                callsign: "<12345>".into(),
                snr_db: 20.0,
            });
            o.stations.push(StationSnr {
                callsign: "k1abc".into(),
                snr_db: -10.0,
            });
        }
        let duplicate = r.observations[0].clone();
        r.observations.push(duplicate);
        r.finish(true, None);
        assert_eq!(r.settings[0].unique_stations, 3);
        assert_eq!(r.settings[0].valid_windows, 6);
        assert_eq!(r.comparisons[0].matched_pairs, 6);
    }
    #[test]
    fn gain_sweep_complete_report_json_round_trip() {
        let mut r = fixture(&[gs(0, 0), gs(10, 0)], &[-10.0, -10.0], 6);
        r.finish(true, None);
        let bytes = serde_json::to_vec_pretty(&r).unwrap();
        let saved: Report = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved.schema_version, 1);
        assert_eq!(saved.observations.len(), 12);
        assert_eq!(saved.recommendation, r.recommendation);
        assert_eq!(saved.restoration, "confirmed");
    }
    #[test]
    fn gain_sweep_nearest_time_tie_prefers_earlier_slot() {
        let gains = vec![gs(0, 0), gs(10, 0)];
        let mut r = fixture(&gains, &[-10.0, -10.0], 1);
        r.observations[1].slot = -30;
        for s in &mut r.observations[1].stations {
            s.snr_db = -11.0;
        }
        let mut later = r.observations[1].clone();
        later.slot = 30;
        for s in &mut later.stations {
            s.snr_db = -5.0;
        }
        r.observations.push(later);
        r.finish(true, None);
        assert_eq!(r.comparisons[0].snr_delta_db, Some(-1.0));
    }
    #[test]
    fn gain_sweep_full_scale_sine_integrates_to_half_power() {
        let n = 2048;
        let samples: Vec<_> = (0..n)
            .map(|i| (std::f64::consts::TAU * 32.0 * i as f64 / n as f64).sin() as f32)
            .collect();
        let hann: Vec<_> = (0..n)
            .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos())
            .collect();
        let fft = FftPlanner::new().plan_fft_forward(n);
        let psd = periodogram(
            &samples,
            &hann,
            12000.0 * hann.iter().map(|w| w * w).sum::<f64>(),
            fft.as_ref(),
        );
        let power = psd.iter().sum::<f64>() * 12000.0 / n as f64;
        assert!((power - 0.5).abs() < 1e-6, "{power}");
    }
    fn white_noise() -> Vec<f32> {
        let mut state = 12345_u32;
        (0..12000)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32 * 0.01
            })
            .collect()
    }
    #[test]
    fn gain_sweep_noise_doubling_is_six_db() {
        let samples = white_noise();
        let doubled: Vec<_> = samples.iter().map(|s| s * 2.0).collect();
        let delta = noise_floor(&doubled, &[]).unwrap() - noise_floor(&samples, &[]).unwrap();
        assert!((delta - 6.0206).abs() < 0.5, "{delta}");
        let expected =
            10.0_f64 * (2.0 * 0.01 * 0.01 / 3.0 / 12000.0 * std::f64::consts::LN_2).log10();
        assert!((noise_floor(&samples, &[]).unwrap() - expected).abs() < 1.0);
    }
    #[test]
    fn gain_sweep_noise_invalid_and_masked_inputs() {
        assert!(noise_floor(&[0.0; 4096], &[]).is_err());
        assert!(noise_floor(&[f32::NAN; 4096], &[]).is_err());
        assert!(noise_floor(&[1.0; 4096], &[]).is_err());
        let mut noise = white_noise();
        for v in noise.iter_mut().take(12) {
            *v = 0.999;
        }
        assert!(noise_floor(&noise, &[]).is_err());
        let masks: Vec<_> = (100..=3000).step_by(50).map(|x| x as f32).collect();
        assert!(noise_floor(&white_noise(), &masks).is_err());
    }
    #[test]
    fn gain_sweep_noise_tone_mask_and_outside_bands() {
        let noise = white_noise();
        let baseline = noise_floor(&noise, &[]).unwrap();
        for hz in [187.5, 1500.0] {
            let samples: Vec<_> = noise
                .iter()
                .enumerate()
                .map(|(i, s)| s + 0.1 * (std::f32::consts::TAU * hz * i as f32 / 12000.0).sin())
                .collect();
            assert!((noise_floor(&samples, &[hz]).unwrap() - baseline).abs() < 0.5);
        }
    }
}
