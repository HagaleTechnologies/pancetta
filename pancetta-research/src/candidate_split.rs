//! FT4 candidate-admission diagnosis; no decoder configuration is changed here.

use crate::{
    baseline_cache::BaselineCache,
    scorecard::CorpusProvenance,
    synth::{SynthEntry, SynthManifest},
};

use anyhow::{ensure, Context, Result};
use pancetta_ft8::{Ft8Config, SyncCandidateRecord};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Mirrors bin/gen_synth.rs::LEAD_IN_S_FT4; FT4 slots place the signal 0.5 s after buffer start.
pub const FT4_SYNTH_LEAD_IN_S: f64 = 0.5;
/// FT4 search grid: 20.8333 Hz / FREQ_OSR 2.
pub const FT4_GRID_FREQ_HZ: f64 = 20.833333333333332 / 2.0;
/// FT4 search grid: 0.048 s / TIME_OSR 2.
pub const FT4_GRID_TIME_S: f64 = 0.024;
/// One FT4 frequency grid step (20.8333 / 2 Hz), four time steps (0.048 / 2 s), with frequency slack.
pub const PRIMARY_GATE: Gate = Gate {
    freq_hz: 10.5,
    time_s: 0.096,
};
/// Sensitivity check: about one FT4 tone spacing (20.8333 Hz), and 0.16 s.
pub const SENSITIVITY_GATE: Gate = Gate {
    freq_hz: 20.9,
    time_s: 0.16,
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Gate {
    pub freq_hz: f64,
    pub time_s: f64,
}
#[derive(Clone, Copy, Debug)]
pub struct TruthPosition {
    pub freq_hz: f64,
    pub time_s: f64,
}
impl TruthPosition {
    pub fn from_ft4_synth_entry(e: &SynthEntry) -> Self {
        Self {
            freq_hz: e.base_freq_hz,
            time_s: FT4_SYNTH_LEAD_IN_S + e.dt_s,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissLabel {
    #[serde(rename = "no candidate produced")]
    NoCandidateProduced,
    #[serde(rename = "candidate produced, decode failed")]
    CandidateProducedDecodeFailed,
}
fn offsets(c: &SyncCandidateRecord, t: &TruthPosition) -> (f64, f64) {
    (c.freq_hz - t.freq_hz, c.dt_s - t.time_s)
}
pub fn within_gate(c: &SyncCandidateRecord, t: &TruthPosition, g: Gate) -> bool {
    let (df, dt) = offsets(c, t);
    df.abs() <= g.freq_hz + 1e-12 && dt.abs() <= g.time_s + 1e-12
}
pub fn classify_miss(t: &TruthPosition, records: &[SyncCandidateRecord], g: Gate) -> MissLabel {
    if records.iter().any(|c| within_gate(c, t, g)) {
        MissLabel::CandidateProducedDecodeFailed
    } else {
        MissLabel::NoCandidateProduced
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NearestCandidate {
    pub df_hz: f64,
    pub dt_s: f64,
    pub sync_score: f64,
    pub pass: usize,
    pub decoded: bool,
}
pub fn nearest_candidate(
    t: &TruthPosition,
    records: &[SyncCandidateRecord],
) -> Option<NearestCandidate> {
    let distance = |c: &SyncCandidateRecord| {
        let (df, dt) = offsets(c, t);
        (df / FT4_GRID_FREQ_HZ).powi(2) + (dt / FT4_GRID_TIME_S).powi(2)
    };
    records
        .iter()
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
        .map(|c| {
            let (df_hz, dt_s) = offsets(c, t);
            NearestCandidate {
                df_hz,
                dt_s,
                sync_score: c.sync_score,
                pass: c.pass,
                decoded: c.decoded,
            }
        })
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapLiftedRecheck {
    pub max_sync_candidates: usize,
    pub pass0_admitted: usize,
    pub gated_candidate_found: bool,
    pub best_gated_sync_score: Option<f64>,
    pub truth_recovered: bool,
    pub label: String,
}
impl CapLiftedRecheck {
    pub fn from_records(
        cap: usize,
        t: &TruthPosition,
        records: &[SyncCandidateRecord],
        truth_recovered: bool,
    ) -> Self {
        let pass0: Vec<_> = records.iter().filter(|c| c.pass == 0).collect();
        let best = pass0
            .iter()
            .filter(|c| within_gate(c, t, PRIMARY_GATE))
            .map(|c| c.sync_score)
            .max_by(f64::total_cmp);
        Self {
            max_sync_candidates: cap,
            pass0_admitted: pass0.len(),
            gated_candidate_found: best.is_some(),
            best_gated_sync_score: best,
            truth_recovered,
            label: cap_lifted_label(best.is_some(), pass0.len(), cap).into(),
        }
    }
}
fn cap_lifted_label(found: bool, admitted: usize, cap: usize) -> &'static str {
    if found {
        "present above min_sync_score, cut by the max_sync_candidates cap"
    } else if admitted >= cap {
        "inconclusive: max_sync_candidates cap still binds"
    } else {
        "absent above min_sync_score"
    }
}
fn inconclusive_recheck(c: &CapLiftedRecheck) -> bool {
    !c.gated_candidate_found && c.pass0_admitted >= c.max_sync_candidates
}
fn is_zero(n: &usize) -> bool {
    *n == 0
}
pub struct WavObservation {
    pub wav_path: String,
    pub wav_sha256: String,
    pub snr_db: f64,
    pub message: String,
    pub truth: TruthPosition,
    pub jt9_recovered: bool,
    pub pancetta_recovered: bool,
    pub records: Vec<SyncCandidateRecord>,
    pub max_sync_candidates: usize,
    pub cap_lifted: Option<CapLiftedRecheck>,
}
pub fn jt9_recovered_from_cache(dir: &Path, sha: &str, message: &str) -> Result<(bool, String)> {
    let p = dir.join(format!("{sha}.json"));
    let cache: BaselineCache = serde_json::from_slice(&std::fs::read(&p).with_context(|| {
        format!(
            "jt9 baseline missing for {sha}: {}; run baseline --tier synth --mode ft4",
            p.display()
        )
    })?)
    .with_context(|| format!("invalid jt9 baseline {}", p.display()))?;
    ensure!(
        cache.schema_version == BaselineCache::CURRENT_SCHEMA_VERSION && cache.wav_sha256 == sha,
        "jt9 baseline schema/hash mismatch: {}",
        p.display()
    );
    ensure!(
        !cache.decoder_identity.trim().is_empty(),
        "jt9 baseline has no decoder identity: {}",
        p.display()
    );
    Ok((
        cache.decodes.iter().any(|d| d.message.contains(message)),
        cache.decoder_identity,
    ))
}
pub fn synth_corpus_provenance(
    tier: &str,
    workspace: &Path,
    manifest: &Path,
) -> Result<CorpusProvenance> {
    let manifest = workspace.join(manifest);
    let m = SynthManifest::load(&manifest)?;
    let hash = |p: &Path| -> Result<String> {
        Ok(format!(
            "{:x}",
            Sha256::digest(std::fs::read(p).with_context(|| format!("hashing {}", p.display()))?)
        ))
    };
    let mut set = Sha256::new();
    for e in &m.entries {
        set.update(hash(&workspace.join(&e.wav_path))?.as_bytes());
    }
    let config = manifest
        .to_str()
        .and_then(|p| p.strip_suffix(".manifest.json"))
        .map(|s| std::path::PathBuf::from(format!("{s}.config.json")))
        .filter(|p| p.exists());
    Ok(CorpusProvenance {
        tier: tier.into(),
        manifest: manifest
            .strip_prefix(workspace)
            .unwrap_or(&manifest)
            .display()
            .to_string(),
        manifest_sha256: hash(&manifest)?,
        config_sha256: config.as_deref().map(hash).transpose()?,
        wav_set_sha256: format!("{:x}", set.finalize()),
        wav_count: m.entries.len(),
    })
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecoderProvenance {
    pub path: String,
    pub entry_point: String,
    pub config: String,
    pub budget: String,
    pub max_sync_candidates: usize,
    pub min_sync_score: f64,
}
impl DecoderProvenance {
    pub fn native(c: &Ft8Config) -> Self {
        Self {
            path: "native".into(),
            entry_point: "Ft8Decoder::decode_window_with_candidate_dump".into(),
            config: "Ft8Config::default() with protocol = Ft4".into(),
            budget: "unlimited".into(),
            max_sync_candidates: c.max_sync_candidates,
            min_sync_score: c.min_sync_score,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TruthProvenance {
    pub source: String,
    pub predicate: String,
    pub baseline_dir: String,
    pub decoder_identities: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gates {
    pub primary: Gate,
    pub sensitivity: Gate,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GateCalibration {
    pub recovered_truths: usize,
    pub max_abs_df_hz: f64,
    pub max_abs_dt_s: f64,
    pub all_within_primary_gate: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    pub wavs: usize,
    pub jt9_truths: usize,
    pub pancetta_recovered: usize,
    pub pancetta_recovered_jt9_truths: usize,
    pub pancetta_only_recoveries: usize,
    pub jt9_truth_misses: usize,
    pub no_candidate_produced: usize,
    pub candidate_produced_decode_failed: usize,
    pub no_candidate_produced_pct: Option<f64>,
    pub candidate_produced_decode_failed_pct: Option<f64>,
    pub sensitivity_no_candidate_produced: usize,
    pub sensitivity_candidate_produced_decode_failed: usize,
    pub labels_changed_by_sensitivity_gate: usize,
}
impl Counts {
    fn observe(&mut self, o: &WavObservation) {
        self.wavs += 1;
        self.jt9_truths += usize::from(o.jt9_recovered);
        self.pancetta_recovered += usize::from(o.pancetta_recovered);
        self.pancetta_recovered_jt9_truths += usize::from(o.jt9_recovered && o.pancetta_recovered);
        self.pancetta_only_recoveries += usize::from(!o.jt9_recovered && o.pancetta_recovered);
        if o.jt9_recovered && !o.pancetta_recovered {
            self.jt9_truth_misses += 1;
            let a = classify_miss(&o.truth, &o.records, PRIMARY_GATE);
            let b = classify_miss(&o.truth, &o.records, SENSITIVITY_GATE);
            self.no_candidate_produced += usize::from(a == MissLabel::NoCandidateProduced);
            self.candidate_produced_decode_failed +=
                usize::from(a == MissLabel::CandidateProducedDecodeFailed);
            self.sensitivity_no_candidate_produced +=
                usize::from(b == MissLabel::NoCandidateProduced);
            self.sensitivity_candidate_produced_decode_failed +=
                usize::from(b == MissLabel::CandidateProducedDecodeFailed);
            self.labels_changed_by_sensitivity_gate += usize::from(a != b);
        }
        self.percentages();
    }
    fn percentages(&mut self) {
        self.no_candidate_produced_pct = (self.jt9_truth_misses > 0)
            .then(|| 100.0 * self.no_candidate_produced as f64 / self.jt9_truth_misses as f64);
        self.candidate_produced_decode_failed_pct = (self.jt9_truth_misses > 0).then(|| {
            100.0 * self.candidate_produced_decode_failed as f64 / self.jt9_truth_misses as f64
        });
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.no_candidate_produced + self.candidate_produced_decode_failed
                == self.jt9_truth_misses,
            "primary split count mismatch"
        );
        ensure!(
            self.sensitivity_no_candidate_produced
                + self.sensitivity_candidate_produced_decode_failed
                == self.jt9_truth_misses,
            "sensitivity count mismatch"
        );
        ensure!(
            self.jt9_truth_misses + self.pancetta_recovered_jt9_truths == self.jt9_truths,
            "truth count mismatch"
        );
        ensure!(
            self.pancetta_recovered_jt9_truths + self.pancetta_only_recoveries
                == self.pancetta_recovered,
            "recovery count mismatch"
        );
        ensure!(
            self.jt9_truths <= self.wavs && self.pancetta_recovered <= self.wavs,
            "population exceeds WAVs"
        );
        ensure!(
            self.labels_changed_by_sensitivity_gate + self.sensitivity_no_candidate_produced
                == self.no_candidate_produced,
            "sensitivity flips mismatch"
        );
        let mut expected = self.clone();
        expected.percentages();
        // serde_json's default parser can move a percentage by one ULP.
        // Validate against the counts within the metric's 1e-9 tolerance.
        let same_percentage = |actual: Option<f64>, expected: Option<f64>| match (actual, expected)
        {
            (None, None) => true,
            (Some(a), Some(b)) => (a - b).abs() < 1e-9,
            _ => false,
        };
        ensure!(
            same_percentage(
                self.no_candidate_produced_pct,
                expected.no_candidate_produced_pct
            ) && same_percentage(
                self.candidate_produced_decode_failed_pct,
                expected.candidate_produced_decode_failed_pct
            ),
            "percentage mismatch"
        );
        if let (Some(a), Some(b)) = (
            self.no_candidate_produced_pct,
            self.candidate_produced_decode_failed_pct,
        ) {
            ensure!(
                (a + b - 100.0).abs() < 1e-9,
                "percentages do not sum to 100"
            );
        }
        Ok(())
    }
    fn row(&self, name: &str) -> String {
        format!(
            "| {name} | {} | {} | {} | {} ({}) | {} ({}) |",
            self.jt9_truths,
            self.pancetta_recovered,
            self.jt9_truth_misses,
            self.no_candidate_produced,
            pct(self.no_candidate_produced_pct),
            self.candidate_produced_decode_failed,
            pct(self.candidate_produced_decode_failed_pct)
        )
    }
}
fn pct(n: Option<f64>) -> String {
    n.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "-".into())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnrBin {
    pub snr_db: f64,
    #[serde(flatten)]
    pub counts: Counts,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NonTruthMisses {
    pub total: usize,
    pub no_candidate_produced: usize,
    pub candidate_produced_decode_failed: usize,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CapSummary {
    pub max_sync_candidates: usize,
    pub rechecked: usize,
    pub present_cut_by_cap: usize,
    pub absent: usize,
    /// A saturated raised cap cannot prove absence above the sync threshold.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub inconclusive: usize,
    pub truth_recovered: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MissRecord {
    pub wav_path: String,
    pub wav_sha256: String,
    pub snr_db: f64,
    pub message: String,
    pub truth_freq_hz: f64,
    pub truth_time_s: f64,
    pub label: MissLabel,
    pub sensitivity_label: MissLabel,
    pub gated_candidates: usize,
    pub gated_candidate_decoded_other_text: bool,
    pub nearest: Option<NearestCandidate>,
    pub admitted_candidates: usize,
    pub pass0_admitted: usize,
    pub min_pass0_sync_score: Option<f64>,
    pub candidate_list_at_cap: bool,
    pub cap_lifted: Option<CapLiftedRecheck>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateSplitScorecard {
    pub schema_version: u32,
    pub ticket: String,
    pub generated_at: String,
    pub decoder: DecoderProvenance,
    pub truth: TruthProvenance,
    pub corpus: CorpusProvenance,
    pub gates: Gates,
    pub gate_calibration: GateCalibration,
    pub totals: Counts,
    pub by_snr_db: Vec<SnrBin>,
    pub non_jt9_truth_misses: NonTruthMisses,
    pub cap_lifted_recheck: Option<CapSummary>,
    pub misses: Vec<MissRecord>,
}
impl CandidateSplitScorecard {
    pub fn build(
        observations: &[WavObservation],
        decoder: DecoderProvenance,
        truth: TruthProvenance,
        corpus: CorpusProvenance,
    ) -> Result<Self> {
        let mut s = Self {
            schema_version: 1,
            ticket: "PAN-154".into(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            decoder,
            truth,
            corpus,
            gates: Gates {
                primary: PRIMARY_GATE,
                sensitivity: SENSITIVITY_GATE,
            },
            gate_calibration: GateCalibration {
                all_within_primary_gate: true,
                ..Default::default()
            },
            totals: Counts::default(),
            by_snr_db: vec![],
            non_jt9_truth_misses: NonTruthMisses::default(),
            cap_lifted_recheck: None,
            misses: vec![],
        };
        for o in observations {
            ensure!(
                o.snr_db.is_finite() && o.truth.freq_hz.is_finite() && o.truth.time_s.is_finite(),
                "non-finite truth coordinates"
            );
            s.totals.observe(o);
            if !s.by_snr_db.iter().any(|b| b.snr_db == o.snr_db) {
                s.by_snr_db.push(SnrBin {
                    snr_db: o.snr_db,
                    counts: Counts::default(),
                });
            }
            s.by_snr_db
                .iter_mut()
                .find(|b| b.snr_db == o.snr_db)
                .unwrap()
                .counts
                .observe(o);
            if o.pancetta_recovered {
                ensure!(
                    o.records
                        .iter()
                        .any(|c| within_gate(c, &o.truth, PRIMARY_GATE)),
                    "gate calibration failed for {}",
                    o.wav_path
                );
                let n = nearest_candidate(&o.truth, &o.records)
                    .context("gate calibration: no candidate")?;
                s.gate_calibration.recovered_truths += 1;
                s.gate_calibration.max_abs_df_hz =
                    s.gate_calibration.max_abs_df_hz.max(n.df_hz.abs());
                s.gate_calibration.max_abs_dt_s = s.gate_calibration.max_abs_dt_s.max(n.dt_s.abs());
                continue;
            }
            let label = classify_miss(&o.truth, &o.records, PRIMARY_GATE);
            if !o.jt9_recovered {
                s.non_jt9_truth_misses.total += 1;
                if label == MissLabel::NoCandidateProduced {
                    s.non_jt9_truth_misses.no_candidate_produced += 1;
                } else {
                    s.non_jt9_truth_misses.candidate_produced_decode_failed += 1;
                }
                continue;
            }
            let pass0: Vec<_> = o.records.iter().filter(|c| c.pass == 0).collect();
            s.misses.push(MissRecord {
                wav_path: o.wav_path.clone(),
                wav_sha256: o.wav_sha256.clone(),
                snr_db: o.snr_db,
                message: o.message.clone(),
                truth_freq_hz: o.truth.freq_hz,
                truth_time_s: o.truth.time_s,
                label,
                sensitivity_label: classify_miss(&o.truth, &o.records, SENSITIVITY_GATE),
                gated_candidates: o
                    .records
                    .iter()
                    .filter(|c| within_gate(c, &o.truth, PRIMARY_GATE))
                    .count(),
                gated_candidate_decoded_other_text: o
                    .records
                    .iter()
                    .any(|c| c.decoded && within_gate(c, &o.truth, PRIMARY_GATE)),
                nearest: nearest_candidate(&o.truth, &o.records),
                admitted_candidates: o.records.len(),
                pass0_admitted: pass0.len(),
                min_pass0_sync_score: pass0.iter().map(|c| c.sync_score).min_by(f64::total_cmp),
                candidate_list_at_cap: pass0.len() == o.max_sync_candidates,
                cap_lifted: o.cap_lifted.clone(),
            });
            if let Some(c) = &o.cap_lifted {
                let a = s.cap_lifted_recheck.get_or_insert_with(|| CapSummary {
                    max_sync_candidates: c.max_sync_candidates,
                    ..Default::default()
                });
                ensure!(
                    a.max_sync_candidates == c.max_sync_candidates,
                    "mixed recheck caps"
                );
                a.rechecked += 1;
                a.present_cut_by_cap += usize::from(c.gated_candidate_found);
                a.absent += usize::from(!c.gated_candidate_found && !inconclusive_recheck(c));
                a.inconclusive += usize::from(inconclusive_recheck(c));
                a.truth_recovered += usize::from(c.truth_recovered);
            }
        }
        s.by_snr_db.sort_by(|a, b| a.snr_db.total_cmp(&b.snr_db));
        s.check_invariants()?;
        Ok(s)
    }
    pub fn check_invariants(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported scorecard schema");
        self.totals.validate()?;
        ensure!(
            self.corpus.wav_count == self.totals.wavs,
            "corpus count mismatch"
        );
        ensure!(
            self.misses.len() == self.totals.jt9_truth_misses,
            "miss rows mismatch"
        );
        for (i, b) in self.by_snr_db.iter().enumerate() {
            b.counts.validate()?;
            ensure!(
                b.snr_db.is_finite() && !self.by_snr_db[..i].iter().any(|x| x.snr_db == b.snr_db),
                "invalid/duplicate SNR bin"
            );
            self.check_rows(
                &b.counts,
                self.misses
                    .iter()
                    .filter(|m| m.snr_db == b.snr_db)
                    .collect(),
            )?;
        }
        self.check_rows(&self.totals, self.misses.iter().collect())?;
        // Compare every integer count, rather than a hand-picked subset of fields.
        let total = serde_json::to_value(&self.totals)?;
        for (key, value) in total.as_object().context("counts object")? {
            if let Some(n) = value.as_u64() {
                let sum: u64 = self
                    .by_snr_db
                    .iter()
                    .map(|b| {
                        serde_json::to_value(&b.counts).unwrap()[key]
                            .as_u64()
                            .unwrap()
                    })
                    .sum();
                ensure!(n == sum, "bins do not sum to totals: {key}");
            }
        }
        for m in &self.misses {
            ensure!(
                self.by_snr_db.iter().any(|b| b.snr_db == m.snr_db),
                "miss outside SNR bins"
            );
            ensure!(
                (m.gated_candidates == 0) == (m.label == MissLabel::NoCandidateProduced),
                "label/gate mismatch"
            );
            ensure!(
                !m.gated_candidate_decoded_other_text || m.gated_candidates > 0,
                "decoded-other without gate"
            );
            ensure!(
                m.cap_lifted.is_some()
                    == (self.cap_lifted_recheck.is_some()
                        && m.label == MissLabel::NoCandidateProduced),
                "recheck scope mismatch"
            );
        }
        if let Some(a) = &self.cap_lifted_recheck {
            let rows: Vec<_> = self
                .misses
                .iter()
                .filter_map(|m| m.cap_lifted.as_ref())
                .collect();
            ensure!(
                a.rechecked == self.totals.no_candidate_produced && a.rechecked == rows.len(),
                "recheck count mismatch"
            );
            ensure!(
                a.present_cut_by_cap == rows.iter().filter(|r| r.gated_candidate_found).count()
                    && a.absent
                        == rows
                            .iter()
                            .filter(|r| !r.gated_candidate_found && !inconclusive_recheck(r))
                            .count()
                    && a.inconclusive == rows.iter().filter(|r| inconclusive_recheck(r)).count()
                    && a.truth_recovered == rows.iter().filter(|r| r.truth_recovered).count(),
                "recheck summary mismatch"
            );
            ensure!(
                rows.iter()
                    .all(|r| r.max_sync_candidates == a.max_sync_candidates),
                "recheck cap mismatch"
            );
            ensure!(
                rows.iter().all(|r| r.label
                    == cap_lifted_label(
                        r.gated_candidate_found,
                        r.pass0_admitted,
                        r.max_sync_candidates
                    )),
                "recheck label mismatch"
            );
        }
        ensure!(
            self.non_jt9_truth_misses.total
                == self.non_jt9_truth_misses.no_candidate_produced
                    + self.non_jt9_truth_misses.candidate_produced_decode_failed,
            "non-truth split mismatch"
        );
        ensure!(
            self.non_jt9_truth_misses.total
                + self.totals.jt9_truths
                + self.totals.pancetta_only_recoveries
                == self.totals.wavs,
            "non-truth population mismatch"
        );
        ensure!(
            self.gate_calibration.all_within_primary_gate
                && self.gate_calibration.recovered_truths == self.totals.pancetta_recovered,
            "gate calibration mismatch"
        );
        Ok(())
    }
    fn check_rows(&self, c: &Counts, rows: Vec<&MissRecord>) -> Result<()> {
        ensure!(rows.len() == c.jt9_truth_misses, "bin miss rows mismatch");
        ensure!(
            rows.iter()
                .filter(|m| m.label == MissLabel::NoCandidateProduced)
                .count()
                == c.no_candidate_produced,
            "row label mismatch"
        );
        ensure!(
            rows.iter()
                .filter(|m| m.sensitivity_label == MissLabel::NoCandidateProduced)
                .count()
                == c.sensitivity_no_candidate_produced,
            "row sensitivity mismatch"
        );
        ensure!(
            rows.iter()
                .filter(|m| m.label != m.sensitivity_label)
                .count()
                == c.labels_changed_by_sensitivity_gate,
            "row flip mismatch"
        );
        Ok(())
    }
    pub fn summary_table(&self) -> String {
        let mut lines=vec!["| SNR (dB) | jt9 truths | Pancetta recovered | jt9-truth misses | no candidate produced | candidate produced, decode failed |".into(),"|---:|---:|---:|---:|---:|---:|".into()];
        lines.extend(
            self.by_snr_db
                .iter()
                .map(|b| b.counts.row(&b.snr_db.to_string())),
        );
        lines.push(self.totals.row("Total"));
        lines.join("\n")
    }
    pub fn total_line(&self) -> String {
        let t = &self.totals;
        format!("TOTAL jt9-truth misses: {} | no candidate produced: {} ({}) | candidate produced, decode failed: {} ({})",
        t.jt9_truth_misses,t.no_candidate_produced,pct(t.no_candidate_produced_pct),t.candidate_produced_decode_failed,pct(t.candidate_produced_decode_failed_pct))
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.check_invariants()?;
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        let s: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        s.check_invariants()?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{SynthChannel, SynthEntry};
    fn truth() -> TruthPosition {
        TruthPosition {
            freq_hz: 1000.0,
            time_s: 0.5,
        }
    }
    fn record(df: f64, dt: f64) -> SyncCandidateRecord {
        SyncCandidateRecord {
            pass: 0,
            freq_hz: 1000.0 + df,
            dt_s: 0.5 + dt,
            start_sample: ((0.5 + dt) * 12000.0) as isize,
            sync_score: 6.0,
            decoded: false,
            via_ft8lib_seed: false,
        }
    }
    fn obs(jt9: bool, pan: bool, records: Vec<SyncCandidateRecord>) -> WavObservation {
        WavObservation {
            wav_path: "test.wav".into(),
            wav_sha256: "abc".into(),
            snr_db: -14.0,
            message: "CQ K1AAA FN42".into(),
            truth: truth(),
            jt9_recovered: jt9,
            pancetta_recovered: pan,
            records,
            max_sync_candidates: 200,
            cap_lifted: None,
        }
    }
    fn build(o: &[WavObservation]) -> anyhow::Result<CandidateSplitScorecard> {
        CandidateSplitScorecard::build(
            o,
            DecoderProvenance::native(&pancetta_ft8::Ft8Config::default()),
            TruthProvenance {
                source: "jt9".into(),
                predicate: "contains".into(),
                baseline_dir: "tmp".into(),
                decoder_identities: vec!["fixture".into()],
            },
            CorpusProvenance {
                tier: "synth-ft4".into(),
                manifest: "test".into(),
                manifest_sha256: "abc".into(),
                config_sha256: None,
                wav_set_sha256: "def".into(),
                wav_count: o.len(),
            },
        )
    }
    #[test]
    fn miss_label_serializes_to_ticket_wording() {
        assert_eq!(
            serde_json::to_string(&MissLabel::NoCandidateProduced).unwrap(),
            "\"no candidate produced\""
        );
        assert_eq!(
            serde_json::to_string(&MissLabel::CandidateProducedDecodeFailed).unwrap(),
            "\"candidate produced, decode failed\""
        );
    }
    #[test]
    fn truth_position_adds_ft4_lead_in() {
        let e = SynthEntry {
            wav_path: "x".into(),
            encoded_message: "x".into(),
            snr_db: 0.0,
            channel: SynthChannel::Awgn,
            drift_hz_per_sec: 0.0,
            seed_for_this_wav: 1,
            base_freq_hz: 1537.73,
            dt_s: 0.116,
        };
        let t = TruthPosition::from_ft4_synth_entry(&e);
        assert_eq!(t.freq_hz, 1537.73);
        assert!((t.time_s - 0.616).abs() < 1e-12);
    }
    #[test]
    fn no_candidate_when_nothing_within_primary_gate() {
        assert_eq!(
            classify_miss(
                &truth(),
                &[record(11.0, 0.0), record(0.0, 0.2)],
                PRIMARY_GATE
            ),
            MissLabel::NoCandidateProduced
        );
    }
    #[test]
    fn candidate_failed_when_a_record_is_within_primary_gate() {
        assert_eq!(
            classify_miss(&truth(), &[record(-3.59, -0.003)], PRIMARY_GATE),
            MissLabel::CandidateProducedDecodeFailed
        );
    }
    #[test]
    fn primary_gate_is_inclusive() {
        for r in [
            record(10.5, 0.0),
            record(-10.5, 0.0),
            record(0.0, 0.096),
            record(0.0, -0.096),
        ] {
            assert!(within_gate(&r, &truth(), PRIMARY_GATE));
        }
        for r in [record(10.6, 0.0), record(0.0, 0.1)] {
            assert!(!within_gate(&r, &truth(), PRIMARY_GATE));
        }
    }
    #[test]
    fn later_pass_records_count() {
        let mut r = record(0.0, 0.0);
        r.pass = 1;
        assert_eq!(
            classify_miss(&truth(), &[r], PRIMARY_GATE),
            MissLabel::CandidateProducedDecodeFailed
        );
    }
    #[test]
    fn decoded_flag_does_not_decide_the_label() {
        let mut r = record(0.0, 0.0);
        r.decoded = true;
        let s = build(&[obs(true, false, vec![r])]).unwrap();
        assert_eq!(s.misses[0].label, MissLabel::CandidateProducedDecodeFailed);
        assert!(s.misses[0].gated_candidate_decoded_other_text);
    }
    #[test]
    fn sensitivity_gate_flip_is_counted() {
        let s = build(&[obs(true, false, vec![record(15.0, 0.0)])]).unwrap();
        assert_eq!(s.totals.labels_changed_by_sensitivity_gate, 1);
        assert_eq!(
            s.misses[0].sensitivity_label,
            MissLabel::CandidateProducedDecodeFailed
        );
    }
    #[test]
    fn nearest_candidate_uses_ft4_grid_units() {
        assert_eq!(
            nearest_candidate(&truth(), &[record(10.0, 0.0), record(0.0, 0.03)])
                .unwrap()
                .df_hz,
            10.0
        );
    }
    #[test]
    fn build_labels_only_jt9_truth_misses() {
        let s = build(&[
            obs(true, true, vec![record(0.0, 0.0)]),
            obs(true, false, vec![]),
            obs(true, false, vec![record(0.0, 0.0)]),
            obs(false, false, vec![]),
            obs(false, true, vec![record(0.0, 0.0)]),
        ])
        .unwrap();
        let t = &s.totals;
        assert_eq!(
            (
                t.jt9_truths,
                t.pancetta_recovered,
                t.pancetta_recovered_jt9_truths,
                t.pancetta_only_recoveries
            ),
            (3, 2, 1, 1)
        );
        assert_eq!(
            (
                t.jt9_truth_misses,
                s.misses.len(),
                t.no_candidate_produced,
                t.candidate_produced_decode_failed
            ),
            (2, 2, 1, 1)
        );
        assert_eq!(s.non_jt9_truth_misses.total, 1);
    }
    #[test]
    fn percentages_sum_to_100_or_are_null_without_misses() {
        let s = build(&[obs(true, false, vec![])]).unwrap();
        assert_eq!(s.totals.no_candidate_produced_pct, Some(100.0));
        assert_eq!(s.totals.candidate_produced_decode_failed_pct, Some(0.0));
        let s = build(&[obs(false, false, vec![])]).unwrap();
        assert_eq!(s.totals.no_candidate_produced_pct, None);
        assert_eq!(s.totals.candidate_produced_decode_failed_pct, None);
    }
    #[test]
    fn bins_sum_to_totals_and_invariants_hold() {
        let mut a = obs(true, false, vec![]);
        a.snr_db = -20.0;
        let s = build(&[a, obs(true, false, vec![record(0.0, 0.0)])]).unwrap();
        assert_eq!(s.by_snr_db.len(), 2);
        s.check_invariants().unwrap();
        assert_eq!(
            s.by_snr_db.iter().map(|b| b.counts.wavs).sum::<usize>(),
            s.totals.wavs
        );
    }
    #[test]
    fn check_invariants_rejects_inconsistent_counts() {
        let mut s = build(&[obs(true, false, vec![])]).unwrap();
        s.totals.no_candidate_produced += 1;
        assert!(s.check_invariants().is_err());
    }
    #[test]
    fn build_fails_when_a_recovered_truth_has_no_gated_candidate() {
        assert!(build(&[obs(true, true, vec![record(0.0, 0.5)])])
            .unwrap_err()
            .to_string()
            .contains("gate calibration"));
    }
    #[test]
    fn jt9_cache_missing_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let e = jt9_recovered_from_cache(d.path(), "abc", "CQ")
            .unwrap_err()
            .to_string();
        assert!(e.contains("abc"));
        assert!(e.contains("baseline --tier synth --mode ft4"));
    }
    #[test]
    fn jt9_cache_uses_contains_predicate_and_reports_identity() {
        let d = tempfile::tempdir().unwrap();
        let cache = serde_json::json!({"schema_version":1,"wav_path":"a","wav_sha256":"abc","decoder_identity":"fixture",
            "elapsed_seconds":0.0,"decodes":[{"message":"CQ K1AAA FN42","freq_hz":1000.0,"dt_s":0.5,"snr_db":-10.0}]});
        std::fs::write(d.path().join("abc.json"), cache.to_string()).unwrap();
        assert_eq!(
            jt9_recovered_from_cache(d.path(), "abc", "K1AAA").unwrap(),
            (true, "fixture".into())
        );
        assert!(
            !jt9_recovered_from_cache(d.path(), "abc", "K1AAB")
                .unwrap()
                .0
        );
    }
    #[test]
    fn cap_lifted_label_strings() {
        assert_eq!(
            CapLiftedRecheck::from_records(400, &truth(), &[record(0.0, 0.0)], false).label,
            "present above min_sync_score, cut by the max_sync_candidates cap"
        );
        assert_eq!(
            CapLiftedRecheck::from_records(400, &truth(), &[], false).label,
            "absent above min_sync_score"
        );
    }
    #[test]
    fn cap_lifted_saturated_absence_is_inconclusive() {
        let records = vec![record(50.0, 0.0); 400];
        let mut o = obs(true, false, vec![]);
        o.cap_lifted = Some(CapLiftedRecheck::from_records(
            400,
            &truth(),
            &records,
            false,
        ));
        let mut s = build(&[o]).unwrap();
        let summary = s.cap_lifted_recheck.as_ref().unwrap();
        assert_eq!(
            (summary.rechecked, summary.absent, summary.inconclusive),
            (1, 0, 1)
        );
        assert_eq!(
            s.misses[0].cap_lifted.as_ref().unwrap().label,
            "inconclusive: max_sync_candidates cap still binds"
        );
        s.misses[0].cap_lifted.as_mut().unwrap().label = "absent above min_sync_score".into();
        assert!(s.check_invariants().is_err());
    }
    #[test]
    fn total_line_format() {
        let o: Vec<_> = (0..193)
            .map(|i| {
                obs(
                    true,
                    false,
                    if i < 26 {
                        vec![]
                    } else {
                        vec![record(0.0, 0.0)]
                    },
                )
            })
            .collect();
        assert_eq!(build(&o).unwrap().total_line(),"TOTAL jt9-truth misses: 193 | no candidate produced: 26 (13.5%) | candidate produced, decode failed: 167 (86.5%)");
    }
    #[test]
    fn scorecard_json_roundtrip() {
        // This fraction changes by one ULP in serde_json's default float parser.
        let observations: Vec<_> = (0..193)
            .map(|i| {
                obs(
                    true,
                    false,
                    if i < 26 {
                        vec![]
                    } else {
                        vec![record(0.0, 0.0)]
                    },
                )
            })
            .collect();
        let s = build(&observations).unwrap();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        s.save(&p).unwrap();
        let loaded = CandidateSplitScorecard::load(&p).unwrap();
        assert_eq!(loaded.total_line(), s.total_line());
        assert_eq!(loaded.totals.wavs, s.totals.wavs);
        assert_eq!(loaded.totals.jt9_truths, s.totals.jt9_truths);
        assert_eq!(
            serde_json::to_value(&loaded.misses).unwrap(),
            serde_json::to_value(&s.misses).unwrap()
        );
    }
    #[test]
    fn corpus_wav_set_hash_matches_eval_definition() {
        let d = tempfile::tempdir().unwrap();
        let mut m: crate::synth::SynthManifest = serde_json::from_str(include_str!(
            "../../research/corpus/synth/manifests/ft4_clean.manifest.json"
        ))
        .unwrap();
        m.entries.truncate(2);
        for (i, e) in m.entries.iter_mut().enumerate() {
            e.wav_path = format!("{i}.wav").into();
            std::fs::write(d.path().join(&e.wav_path), [i as u8]).unwrap();
        }
        let p = d.path().join("tiny.manifest.json");
        m.save(&p).unwrap();
        let a = synth_corpus_provenance("synth-ft4", d.path(), &p).unwrap();
        let expected = format!(
            "{:x}",
            Sha256::digest(
                format!("{:x}{:x}", Sha256::digest([0]), Sha256::digest([1])).as_bytes()
            )
        );
        assert_eq!(a.wav_set_sha256, expected);
        assert_eq!(a.manifest, "tiny.manifest.json");
        m.entries.reverse();
        m.save(&p).unwrap();
        assert_ne!(
            a.wav_set_sha256,
            synth_corpus_provenance("synth-ft4", d.path(), &p)
                .unwrap()
                .wav_set_sha256
        );
    }
}
