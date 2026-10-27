//! PAN-161 paired four-arm evaluator. An inconclusive corpus exits nonzero
//! after writing the complete scorecard, so it cannot be mistaken for a pass.
use anyhow::{Context, Result};
use pancetta_ft8::decoder::SyncCandidateRecord;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_research::bootstrap_recall_delta;
use pancetta_research::crowded_edge::{
    diagnose, score_messages, verdict, ArmCounts, Premise, Verdict,
};
use pancetta_research::synth::{CrowdedEdgeEntry, CrowdedEdgeManifest};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Serialize)]
struct DecodedEvidence {
    text: String,
    freq_hz: f64,
    dt_s: f64,
}
#[derive(Serialize)]
struct ArmRecord {
    cap: usize,
    partial: bool,
    raw_candidates: usize,
    scores: ArmCounts,
    messages: Vec<String>,
    decoded: Vec<DecodedEvidence>,
    #[serde(skip)]
    candidates: Vec<SyncCandidateRecord>,
}
#[derive(Serialize)]
struct WavRecord {
    wav_path: PathBuf,
    kind: String,
    arms: Vec<ArmRecord>,
    premise_off: Option<Premise>,
    premise_on: Option<Premise>,
}
#[derive(Serialize)]
struct Totals {
    arms: Vec<ArmCounts>,
    qualified_windows: usize,
    cap_bound_windows: usize,
    displaced_windows: usize,
    crc_valid_displaced_windows: usize,
}
#[derive(Serialize)]
struct Scorecard {
    schema_version: u32,
    manifest_sha256: String,
    manifest_path: PathBuf,
    freq_gate_hz: f64,
    time_gate_s: f64,
    /// Seeds actually used for `[total_tp_ci, edge_tp_ci]`.
    bootstrap_seeds: [u64; 2],
    bootstrap_repetitions: usize,
    elapsed_seconds: f64,
    full: Totals,
    qualified: Totals,
    total_tp_ci: pancetta_research::DeltaCi,
    edge_tp_ci: pancetta_research::DeltaCi,
    verdict: Verdict,
    per_wav: Vec<WavRecord>,
}
fn read_wav(path: &Path, expected: &CrowdedEdgeEntry) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path).with_context(|| format!("missing WAV {}", path.display()))?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    anyhow::ensure!(
        hash == expected.sha256,
        "WAV hash mismatch: {}",
        path.display()
    );
    let reader = hound::WavReader::new(std::io::Cursor::new(bytes))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1
            && spec.sample_rate == expected.sample_rate
            && spec.bits_per_sample == 16,
        "unsupported WAV format: {}",
        path.display()
    );
    let samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|x| x.map(|v| v as f32 / 32767.0))
        .collect::<std::result::Result<_, _>>()?;
    anyhow::ensure!(
        samples.len() == expected.sample_count,
        "WAV sample count mismatch: {}",
        path.display()
    );
    Ok(samples)
}
fn run_arm(
    samples: &[f32],
    entry: &CrowdedEdgeEntry,
    cap: usize,
    partial: bool,
) -> Result<ArmRecord> {
    let mut decoder = Ft8Decoder::new(Ft8Config {
        max_sync_candidates: cap,
        costas_partial_metric_enabled: partial,
        ..Ft8Config::default()
    })?;
    let (messages, candidates) = decoder.decode_window_with_candidate_dump(samples)?;
    let decoded: Vec<DecodedEvidence> = messages
        .into_iter()
        .map(|m| DecodedEvidence {
            text: m.text,
            freq_hz: m.frequency_offset,
            dt_s: m.time_offset,
        })
        .collect();
    let texts: Vec<String> = decoded.iter().map(|m| m.text.clone()).collect();
    let scores = score_messages(
        &entry.kind,
        &entry.signals,
        texts.iter().map(String::as_str),
    );
    Ok(ArmRecord {
        cap,
        partial,
        raw_candidates: decoder.candidate_stage_raw_max(),
        scores,
        messages: texts,
        decoded,
        candidates,
    })
}
fn has_target_output(
    arm: &ArmRecord,
    target: &pancetta_research::synth::CrowdedEdgeSignal,
) -> bool {
    arm.decoded.iter().any(|m| {
        m.text.eq_ignore_ascii_case(&target.message)
            && (m.freq_hz - target.freq_hz).abs() <= pancetta_research::crowded_edge::FREQ_GATE_HZ
            && (m.dt_s - target.capture_dt_s).abs() <= pancetta_research::crowded_edge::TIME_GATE_S
    })
}
fn totals(records: &[WavRecord], qualified_only: bool) -> Totals {
    let mut arms = vec![ArmCounts::default(); 4];
    let mut result = Totals {
        arms: Vec::new(),
        qualified_windows: 0,
        cap_bound_windows: 0,
        displaced_windows: 0,
        crc_valid_displaced_windows: 0,
    };
    for record in records {
        let Some(p) = &record.premise_off else {
            if !qualified_only {
                for (i, arm) in record.arms.iter().enumerate() {
                    arms[i].add(&arm.scores);
                }
            }
            continue;
        };
        let qualified = p.crc_valid_raised_cap;
        if qualified_only && !qualified {
            continue;
        }
        if qualified {
            result.qualified_windows += 1;
        }
        if p.cap_bound {
            result.cap_bound_windows += 1;
        }
        if p.absent_at_200_present_at_600 {
            result.displaced_windows += 1;
        }
        if p.crc_valid_raised_cap {
            result.crc_valid_displaced_windows += 1;
        }
        for (i, arm) in record.arms.iter().enumerate() {
            arms[i].add(&arm.scores);
        }
    }
    result.arms = arms;
    result
}
fn evaluate(manifest_path: &Path, output: &Path, wav_dir: Option<&Path>) -> Result<Scorecard> {
    let start = Instant::now();
    let manifest_bytes = std::fs::read(manifest_path)?;
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
    let manifest: CrowdedEdgeManifest = serde_json::from_slice(&manifest_bytes)?;
    manifest.validate()?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut records = Vec::new();
    for (index, entry) in manifest.entries.iter().enumerate() {
        let path = match wav_dir {
            Some(dir) => dir.join(entry.wav_path.file_name().context("WAV filename absent")?),
            None => root.join(&entry.wav_path),
        };
        let samples = read_wav(&path, entry)?;
        let arms = vec![
            run_arm(&samples, entry, 200, false)?,
            run_arm(&samples, entry, 200, true)?,
            run_arm(&samples, entry, 600, false)?,
            run_arm(&samples, entry, 600, true)?,
        ];
        let target = entry.signals.iter().find(|s| s.role == "edge-target");
        let premise_off = target.map(|t| {
            diagnose(
                t,
                arms[0].raw_candidates,
                arms[2].raw_candidates,
                &arms[0].candidates,
                &arms[2].candidates,
                has_target_output(&arms[2], t),
            )
        });
        let premise_on = target.map(|t| {
            diagnose(
                t,
                arms[1].raw_candidates,
                arms[3].raw_candidates,
                &arms[1].candidates,
                &arms[3].candidates,
                has_target_output(&arms[3], t),
            )
        });
        records.push(WavRecord {
            wav_path: entry.wav_path.clone(),
            kind: entry.kind.clone(),
            arms,
            premise_off,
            premise_on,
        });
        eprintln!(
            "evaluated {}/{} {}",
            index + 1,
            manifest.entries.len(),
            entry.wav_path.display()
        );
    }
    let full = totals(&records, false);
    let qualified = totals(&records, true);
    let a_total: Vec<_> = records
        .iter()
        .map(|r| (r.arms[0].scores.total_tp(), 0))
        .collect();
    let b_total: Vec<_> = records
        .iter()
        .map(|r| (r.arms[1].scores.total_tp(), 0))
        .collect();
    let a_edge: Vec<_> = records
        .iter()
        .map(|r| (r.arms[0].scores.edge_tp, 0))
        .collect();
    let b_edge: Vec<_> = records
        .iter()
        .map(|r| (r.arms[1].scores.edge_tp, 0))
        .collect();
    let bootstrap_seeds = [manifest.config.seed ^ 0x161, manifest.config.seed ^ 0x162];
    let total_tp_ci = bootstrap_recall_delta(&a_total, &b_total, 1000, bootstrap_seeds[0]);
    let edge_tp_ci = bootstrap_recall_delta(&a_edge, &b_edge, 1000, bootstrap_seeds[1]);
    let decision = verdict(
        full.crc_valid_displaced_windows,
        &full.arms[0],
        &full.arms[1],
        total_tp_ci.ci_low,
        edge_tp_ci.ci_low,
    );
    let scorecard = Scorecard {
        schema_version: 1,
        manifest_sha256,
        manifest_path: manifest_path.into(),
        freq_gate_hz: pancetta_research::crowded_edge::FREQ_GATE_HZ,
        time_gate_s: pancetta_research::crowded_edge::TIME_GATE_S,
        bootstrap_seeds,
        bootstrap_repetitions: 1000,
        elapsed_seconds: start.elapsed().as_secs_f64(),
        full,
        qualified,
        total_tp_ci,
        edge_tp_ci,
        verdict: decision,
        per_wav: records,
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, serde_json::to_vec_pretty(&scorecard)?)?;
    Ok(scorecard)
}
fn main() -> Result<()> {
    let mut manifest = None;
    let mut output = None;
    let mut wav_dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--manifest" => {
                manifest = Some(PathBuf::from(
                    args.next().context("--manifest value missing")?,
                ))
            }
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().context("--output value missing")?,
                ))
            }
            "--wav-dir" => {
                wav_dir = Some(PathBuf::from(
                    args.next().context("--wav-dir value missing")?,
                ))
            }
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }
    let manifest = manifest.context("--manifest required")?;
    let output = output.context("--output required")?;
    let result = evaluate(&manifest, &output, wav_dir.as_deref())?;
    println!(
        "{} WAVs; {} CRC-valid displaced edge targets; default_on={}; scorecard {}",
        result.per_wav.len(),
        result.full.crc_valid_displaced_windows,
        result.verdict.default_on,
        output.display()
    );
    anyhow::ensure!(
        result.verdict.conclusive,
        "inconclusive corpus: fewer than 20 CRC-valid displaced edge targets"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_research::synth::{generate_crowded_edge, CrowdedEdgeConfig};
    fn fixture_config() -> CrowdedEdgeConfig {
        CrowdedEdgeConfig {
            schema_version: 1,
            generator_version: 1,
            seed: 161,
            output_dir: "unused".into(),
            repetitions: 1,
            target_snr_db: vec![-16.0],
            target_dt_s: vec![-0.6],
            distractor_count: 3,
            distractor_snr_db: -8.0,
            noise_snr_db: -16.0,
            noise_windows: 0,
            in_window_controls: 0,
        }
    }
    fn fixture() -> (CrowdedEdgeEntry, Vec<u8>) {
        generate_crowded_edge(&fixture_config()).unwrap().remove(0)
    }
    #[test]
    fn crowded_edge_scorecard_records_bootstrap_seeds_used() {
        let tmp = tempfile::tempdir().unwrap();
        let (entry, bytes) = fixture();
        std::fs::write(tmp.path().join(entry.wav_path.file_name().unwrap()), &bytes).unwrap();
        let manifest_path = tmp.path().join("m.json");
        let manifest = CrowdedEdgeManifest {
            schema_version: CrowdedEdgeManifest::VERSION,
            generator_version: 1,
            config: fixture_config(),
            entries: vec![entry],
        };
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let scorecard = evaluate(
            &manifest_path,
            &tmp.path().join("out.json"),
            Some(tmp.path()),
        )
        .unwrap();
        assert_eq!(scorecard.bootstrap_seeds, [161 ^ 0x161, 161 ^ 0x162]);
    }
    #[test]
    fn crowded_edge_unsupported_manifest_and_missing_wav_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("m.json");
        let (entry, bytes) = fixture();
        let config = CrowdedEdgeConfig {
            schema_version: 1,
            generator_version: 1,
            seed: 161,
            output_dir: "unused".into(),
            repetitions: 1,
            target_snr_db: vec![-16.0],
            target_dt_s: vec![-0.6],
            distractor_count: 3,
            distractor_snr_db: -8.0,
            noise_snr_db: -16.0,
            noise_windows: 0,
            in_window_controls: 0,
        };
        let bad = CrowdedEdgeManifest {
            schema_version: 2,
            generator_version: 1,
            config,
            entries: vec![entry.clone()],
        };
        std::fs::write(&manifest, serde_json::to_vec(&bad).unwrap()).unwrap();
        let err = evaluate(&manifest, &tmp.path().join("out.json"), None)
            .err()
            .unwrap();
        assert!(err.to_string().contains("unsupported"));
        let path = tmp.path().join("fixture.wav");
        assert!(read_wav(&path, &entry)
            .unwrap_err()
            .to_string()
            .contains("missing WAV"));
        std::fs::write(&path, &bytes).unwrap();
        let mut bad_hash = entry.clone();
        bad_hash.sha256 = "bad".into();
        assert!(read_wav(&path, &bad_hash)
            .unwrap_err()
            .to_string()
            .contains("hash mismatch"));
        assert_eq!(read_wav(&path, &entry).unwrap().len(), entry.sample_count);
    }
    #[test]
    fn crowded_edge_live_decoder_dump_preserves_outputs() {
        let (entry, bytes) = fixture();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fixture.wav");
        std::fs::write(&path, bytes).unwrap();
        let samples = read_wav(&path, &entry).unwrap();
        let dump = run_arm(&samples, &entry, 200, false).unwrap();
        let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
        let ordinary = decoder.decode_window(&samples).unwrap();
        let ordinary_texts: Vec<_> = ordinary.into_iter().map(|m| m.text).collect();
        assert_eq!(dump.messages, ordinary_texts);
        assert!(dump.raw_candidates >= dump.candidates.iter().filter(|r| r.pass == 0).count());
    }
    fn window(qualified: bool) -> WavRecord {
        let arm = |partial| ArmRecord {
            cap: 200,
            partial,
            raw_candidates: 400,
            scores: ArmCounts {
                edge_tp: 1,
                ..ArmCounts::default()
            },
            messages: Vec::new(),
            decoded: Vec::new(),
            candidates: Vec::new(),
        };
        let premise = Premise {
            cap_bound: true,
            absent_at_200_present_at_600: qualified,
            crc_valid_raised_cap: qualified,
            ..Premise::default()
        };
        WavRecord {
            wav_path: "w.wav".into(),
            kind: "edge".into(),
            arms: vec![arm(false), arm(true), arm(false), arm(true)],
            premise_off: Some(premise.clone()),
            premise_on: Some(premise),
        }
    }
    #[test]
    fn crowded_edge_qualified_totals_count_only_qualified_windows() {
        let records = [window(true), window(false)];
        let q = totals(&records, true);
        assert_eq!(
            (
                q.qualified_windows,
                q.cap_bound_windows,
                q.displaced_windows,
                q.crc_valid_displaced_windows,
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(q.arms[0].edge_tp, 1);
        let f = totals(&records, false);
        assert_eq!(
            (
                f.qualified_windows,
                f.cap_bound_windows,
                f.displaced_windows,
                f.crc_valid_displaced_windows,
            ),
            (1, 2, 1, 1)
        );
        assert_eq!(f.arms[0].edge_tp, 2);
    }
    #[test]
    fn crowded_edge_qualification_uses_production_arms_only() {
        let mut rescued = window(false);
        rescued.premise_off = Some(Premise {
            cap_bound: true,
            absent_at_200_present_at_600: true,
            crc_valid_raised_cap: true,
            ..Premise::default()
        });
        let full = totals(std::slice::from_ref(&rescued), false);
        assert_eq!(full.crc_valid_displaced_windows, 1);
        assert_eq!(
            totals(std::slice::from_ref(&rescued), true).qualified_windows,
            1
        );
    }
}
