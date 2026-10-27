//! PAN-161 pure multi-truth scoring and pre-registered verdict rules.
use crate::synth::CrowdedEdgeSignal;
use pancetta_ft8::decoder::SyncCandidateRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmCounts {
    pub edge_tp: u32,
    pub distractor_tp: u32,
    pub control_tp: u32,
    pub crowded_fp: u32,
    pub noise_fp: u32,
}
impl ArmCounts {
    pub fn total_tp(&self) -> u32 {
        self.edge_tp + self.distractor_tp + self.control_tp
    }
    pub fn total_fp(&self) -> u32 {
        self.crowded_fp + self.noise_fp
    }
    pub fn add(&mut self, other: &Self) {
        self.edge_tp += other.edge_tp;
        self.distractor_tp += other.distractor_tp;
        self.control_tp += other.control_tp;
        self.crowded_fp += other.crowded_fp;
        self.noise_fp += other.noise_fp;
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

/// A truth can score once; each extra output, including a duplicate, is FP.
pub fn score_messages<'a>(
    kind: &str,
    truths: &[CrowdedEdgeSignal],
    texts: impl IntoIterator<Item = &'a str>,
) -> ArmCounts {
    let mut found = HashSet::new();
    let mut out = ArmCounts::default();
    for text in texts {
        if let Some((index, truth)) = truths.iter().enumerate().find(|(index, truth)| {
            !found.contains(index) && normalize(&truth.message) == normalize(text)
        }) {
            found.insert(index);
            if truth.role == "edge-target" {
                out.edge_tp += 1;
            } else if truth.role == "control-target" {
                out.control_tp += 1;
            } else {
                out.distractor_tp += 1;
            }
        } else if kind == "noise" {
            out.noise_fp += 1;
        } else {
            out.crowded_fp += 1;
        }
    }
    out
}

/// Predeclared gate: two adjacent 6.25-Hz tone bins (12.5 Hz) and one
/// 0.16-s FT8 symbol around the exact synthesis coordinates.
pub const FREQ_GATE_HZ: f64 = 12.5;
pub const TIME_GATE_S: f64 = 0.16;

pub fn near_target(record: &SyncCandidateRecord, target: &CrowdedEdgeSignal) -> bool {
    record.pass == 0
        && (record.freq_hz - target.freq_hz).abs() <= FREQ_GATE_HZ
        && (record.dt_s - target.capture_dt_s).abs() <= TIME_GATE_S
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateLocation {
    pub freq_hz: f64,
    pub dt_s: f64,
    pub sync_score: f64,
    pub decoded: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Premise {
    pub cap_bound: bool,
    pub absent_at_200_present_at_600: bool,
    pub crc_valid_raised_cap: bool,
    pub candidates_200_near_target: usize,
    pub candidates_600_near_target: usize,
    pub raw_candidates_200: usize,
    pub raw_candidates_600: usize,
    pub target_candidates_200: Vec<CandidateLocation>,
    pub target_candidates_600: Vec<CandidateLocation>,
}

pub fn diagnose(
    target: &CrowdedEdgeSignal,
    raw_200: usize,
    raw_600: usize,
    base: &[SyncCandidateRecord],
    raised: &[SyncCandidateRecord],
    raised_target_output: bool,
) -> Premise {
    let a: Vec<_> = base.iter().filter(|r| near_target(r, target)).collect();
    let b: Vec<_> = raised.iter().filter(|r| near_target(r, target)).collect();
    let displaced = a.is_empty() && !b.is_empty();
    Premise {
        cap_bound: raw_200 >= 200,
        absent_at_200_present_at_600: displaced,
        crc_valid_raised_cap: displaced && raised_target_output && b.iter().any(|r| r.decoded),
        candidates_200_near_target: a.len(),
        candidates_600_near_target: b.len(),
        raw_candidates_200: raw_200,
        raw_candidates_600: raw_600,
        target_candidates_200: a
            .iter()
            .map(|r| CandidateLocation {
                freq_hz: r.freq_hz,
                dt_s: r.dt_s,
                sync_score: r.sync_score,
                decoded: r.decoded,
            })
            .collect(),
        target_candidates_600: b
            .iter()
            .map(|r| CandidateLocation {
                freq_hz: r.freq_hz,
                dt_s: r.dt_s,
                sync_score: r.sync_score,
                decoded: r.decoded,
            })
            .collect(),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub conclusive: bool,
    pub batch75: bool,
    pub bootstrap_total: bool,
    pub bootstrap_edge: bool,
    pub noise: bool,
    pub novel: bool,
    pub default_on: bool,
    pub delta_tp: i32,
    pub delta_fp: i32,
}

pub fn verdict(
    qualified_crc: usize,
    baseline: &ArmCounts,
    technique: &ArmCounts,
    total_ci_low: f64,
    edge_ci_low: f64,
) -> Verdict {
    let delta_tp = technique.total_tp() as i32 - baseline.total_tp() as i32;
    let delta_fp = technique.total_fp() as i32 - baseline.total_fp() as i32;
    let batch75 = (delta_tp >= 20 && delta_fp <= 50) || (delta_tp > 0 && delta_fp <= 0);
    let conclusive = qualified_crc >= 20;
    let bootstrap_total = total_ci_low > 0.0;
    let bootstrap_edge = edge_ci_low > 0.0;
    let noise = technique.noise_fp <= baseline.noise_fp;
    let novel = technique.crowded_fp as i32 - baseline.crowded_fp as i32 <= 2 * delta_tp;
    Verdict {
        conclusive,
        batch75,
        bootstrap_total,
        bootstrap_edge,
        noise,
        novel,
        default_on: conclusive && batch75 && bootstrap_total && bootstrap_edge && noise && novel,
        delta_tp,
        delta_fp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signal(role: &str, text: &str) -> CrowdedEdgeSignal {
        CrowdedEdgeSignal {
            message: text.into(),
            freq_hz: 1500.0,
            start_s: 9.0,
            capture_dt_s: -0.5,
            snr_db: -16.0,
            role: role.into(),
        }
    }
    fn candidate(pass: usize, freq: f64, dt: f64, decoded: bool) -> SyncCandidateRecord {
        SyncCandidateRecord {
            pass,
            freq_hz: freq,
            dt_s: dt,
            start_sample: (dt * 12_000.0) as isize,
            sync_score: 1.0,
            decoded,
            via_ft8lib_seed: false,
        }
    }
    #[test]
    fn crowded_edge_multi_truth_and_fp() {
        let truths = [
            signal("edge-target", "CQ K5ARH EM12"),
            signal("distractor", "CQ K1AAA FN42"),
        ];
        let c = score_messages(
            "edge",
            &truths,
            ["cq k5arh em12", "CQ K1AAA FN42", "CQ K5ARH EM12", "garbage"],
        );
        assert_eq!(
            c,
            ArmCounts {
                edge_tp: 1,
                distractor_tp: 1,
                control_tp: 0,
                crowded_fp: 2,
                noise_fp: 0
            }
        );
        assert_eq!(score_messages("noise", &[], ["CQ K5ARH EM12"]).noise_fp, 1);
    }
    #[test]
    fn crowded_edge_control_truth_is_not_an_edge_tp() {
        let truths = [signal("control-target", "CQ K5ARH EM12")];
        let counts = score_messages("control", &truths, ["CQ K5ARH EM12"]);
        assert_eq!(counts.edge_tp, 0);
        assert_eq!(counts.control_tp, 1);
        assert_eq!(counts.total_tp(), 1);
    }
    #[test]
    fn crowded_edge_cap_diagnosis_ignores_later_passes_and_order() {
        let target = signal("edge-target", "CQ K5ARH EM12");
        let base = [
            candidate(1, 1500.0, -0.5, true),
            candidate(0, 1700.0, -0.5, false),
        ];
        let raised = [
            candidate(0, 1500.0, -0.5, true),
            candidate(0, 1800.0, -0.5, false),
        ];
        let p = diagnose(&target, 400, 700, &base, &raised, true);
        assert!(p.cap_bound && p.absent_at_200_present_at_600 && p.crc_valid_raised_cap);
        let mut reversed = raised.to_vec();
        reversed.reverse();
        assert_eq!(p, diagnose(&target, 400, 700, &base, &reversed, true));
        assert!(!diagnose(&target, 400, 700, &base, &raised, false).crc_valid_raised_cap);
    }
    #[test]
    fn crowded_edge_verdict_boundaries() {
        let a = ArmCounts::default();
        let mut b = ArmCounts {
            edge_tp: 20,
            crowded_fp: 50,
            ..ArmCounts::default()
        };
        assert!(verdict(20, &a, &b, 1.0, 1.0).batch75);
        assert!(!verdict(20, &a, &b, 1.0, 1.0).novel);
        b.edge_tp = 19;
        assert!(!verdict(20, &a, &b, 1.0, 1.0).batch75);
        b.edge_tp = 1;
        b.crowded_fp = 0;
        assert!(verdict(20, &a, &b, 1.0, 1.0).default_on);
        b.edge_tp = 20;
        b.crowded_fp = 51;
        assert!(!verdict(20, &a, &b, 1.0, 1.0).batch75);
        b.crowded_fp = 0;
        assert!(!verdict(20, &a, &b, 0.0, 1.0).default_on);
        b.noise_fp = 1;
        assert!(!verdict(20, &a, &b, 1.0, 1.0).noise);
        b.noise_fp = 0;
        b.crowded_fp = 41;
        assert!(!verdict(20, &a, &b, 1.0, 1.0).novel);
        assert!(
            !verdict(
                19,
                &a,
                &ArmCounts {
                    edge_tp: 20,
                    ..ArmCounts::default()
                },
                1.0,
                1.0
            )
            .default_on
        );
    }
}
