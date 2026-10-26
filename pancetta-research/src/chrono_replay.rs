//! Chronological-replay corpus manifest.
//!
//! A [`ChronoReplayManifest`] is a list of slot-aligned WAVs **in timestamp
//! order**, drawn from a single contiguous operator session. Unlike the
//! shuffled / interest-scored `curated` manifests (hard-200, hard-1000,
//! wild-N), entries here are required to be sequential — the eval harness
//! processes them in order and carries decoder state across consecutive
//! WAVs so cross-slot mechanisms (hb-048 a7, hb-057 median-DT, hb-173
//! within-QSO) can be exercised against a realistic session trace.
//!
//! See [`crate::bin::eval`] for stateful tier dispatch and the
//! `curate_chrono_replay` binary for manifest construction.
//!
//! ## Why a separate manifest type
//!
//! `CuratedManifest` carries `interest_score` + `score_breakdown`. Chrono
//! replay rejects scoring as a selection criterion — it requires temporal
//! continuity, not interestingness — so its entries carry only the bits
//! needed for ordered replay: path, SHA, and an explicit `slot_index`.
//!
//! ## Schema
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "label": "k5arh_20260530",
//!   "generated_at": "2026-06-01T22:00:00Z",
//!   "source_session_label": "ft8_20260530_*",
//!   "first_wav_timestamp": "2026-05-30T15:23:56Z",
//!   "last_wav_timestamp": "2026-05-30T16:38:43Z",
//!   "span_seconds": 4527.0,
//!   "entries": [
//!     { "wav_path": "/abs/path/ft8_20260530_152356.wav",
//!       "wav_sha256": "…", "slot_index": 0,
//!       "wav_timestamp": "2026-05-30T15:23:56Z" },
//!     …
//!   ]
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One slot in a chronological-replay tier — a WAV at a specific point in
/// a continuous capture session. Entries in a [`ChronoReplayManifest`] are
/// ordered by `slot_index` ascending, which mirrors the temporal order of
/// the underlying audio.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChronoReplayEntry {
    /// Absolute path to the WAV on the operator's machine.
    pub wav_path: PathBuf,
    /// SHA-256 hex of the WAV file content (matches jt9 baseline cache key).
    pub wav_sha256: String,
    /// Zero-based slot index within the manifest. Strict monotonic order
    /// is invariant; gaps in the operator's recording stream are preserved
    /// (the harness DOES NOT inject silence — only consecutive WAVs are
    /// included by `curate_chrono_replay`).
    pub slot_index: u32,
    /// ISO 8601 UTC timestamp parsed from the recording filename (e.g.
    /// "2026-05-30T15:23:56Z" from `ft8_20260530_152356.wav`). The
    /// timestamp is metadata only — replay order is determined by
    /// `slot_index`, not by this field.
    pub wav_timestamp: String,
}

/// A chronological-replay corpus: an ordered sequence of slot WAVs from a
/// single capture session. The harness asserts on load that
/// `slot_index` values are `0..N` strictly increasing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChronoReplayManifest {
    /// Schema version (current = 1).
    pub schema_version: u32,
    /// Human-readable label for this tier (e.g. "k5arh_20260530").
    pub label: String,
    /// ISO 8601 UTC timestamp of manifest construction.
    pub generated_at: String,
    /// Filename pattern of the source session (e.g. "ft8_20260530_*").
    /// Provenance for re-curation.
    pub source_session_label: String,
    /// ISO 8601 UTC of the first WAV (slot_index 0).
    pub first_wav_timestamp: String,
    /// ISO 8601 UTC of the last WAV (slot_index N-1).
    pub last_wav_timestamp: String,
    /// Span in seconds between first and last WAVs (informational).
    pub span_seconds: f64,
    /// Entries in slot_index ascending order.
    pub entries: Vec<ChronoReplayEntry>,
}

impl ChronoReplayManifest {
    /// Manifest schema version supported by this crate.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Persist to disk as pretty-printed JSON.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<()> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Load from disk and validate slot ordering.
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(&path)?;
        let m: ChronoReplayManifest = serde_json::from_str(&s)?;
        anyhow::ensure!(
            m.schema_version == Self::CURRENT_SCHEMA_VERSION,
            "ChronoReplayManifest schema_version {} not supported (expected {})",
            m.schema_version,
            Self::CURRENT_SCHEMA_VERSION,
        );
        // Ordering invariant — strict monotonic 0..N.
        for (expected, e) in m.entries.iter().enumerate() {
            anyhow::ensure!(
                e.slot_index as usize == expected,
                "ChronoReplayManifest entries out of order: expected slot_index {} at position {}, found {}",
                expected,
                expected,
                e.slot_index,
            );
        }
        Ok(m)
    }
}

/// Load a chrono-replay manifest from disk, returning entries in their
/// natural (slot_index) order. The loader has already validated the
/// 0..N monotonicity invariant.
pub fn load_chrono_replay_corpus(manifest_path: &Path) -> anyhow::Result<Vec<ChronoReplayEntry>> {
    let manifest = ChronoReplayManifest::load(manifest_path)?;
    Ok(manifest.entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(idx: u32) -> ChronoReplayEntry {
        ChronoReplayEntry {
            wav_path: PathBuf::from(format!("/tmp/wav_{idx}.wav")),
            wav_sha256: format!("{:064x}", idx),
            slot_index: idx,
            wav_timestamp: format!("2026-05-30T00:00:{:02}Z", idx),
        }
    }

    #[test]
    fn roundtrip_preserves_ordering() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let m = ChronoReplayManifest {
            schema_version: ChronoReplayManifest::CURRENT_SCHEMA_VERSION,
            label: "test".into(),
            generated_at: "2026-06-01T00:00:00Z".into(),
            source_session_label: "ft8_test_*".into(),
            first_wav_timestamp: "2026-05-30T00:00:00Z".into(),
            last_wav_timestamp: "2026-05-30T00:00:02Z".into(),
            span_seconds: 30.0,
            entries: (0..3).map(entry).collect(),
        };
        m.save(tmp.path()).unwrap();
        let loaded = ChronoReplayManifest::load(tmp.path()).unwrap();
        assert_eq!(loaded.entries.len(), 3);
        for (i, e) in loaded.entries.iter().enumerate() {
            assert_eq!(e.slot_index as usize, i);
        }
    }

    #[test]
    fn load_rejects_out_of_order_entries() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut entries: Vec<_> = (0..3).map(entry).collect();
        entries.swap(0, 2);
        let m = ChronoReplayManifest {
            schema_version: ChronoReplayManifest::CURRENT_SCHEMA_VERSION,
            label: "test".into(),
            generated_at: "2026-06-01T00:00:00Z".into(),
            source_session_label: "ft8_test_*".into(),
            first_wav_timestamp: "2026-05-30T00:00:00Z".into(),
            last_wav_timestamp: "2026-05-30T00:00:02Z".into(),
            span_seconds: 30.0,
            entries,
        };
        m.save(tmp.path()).unwrap();
        let err = ChronoReplayManifest::load(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("out of order"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_rejects_unknown_schema() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let json = serde_json::json!({
            "schema_version": 999,
            "label": "test",
            "generated_at": "2026-06-01T00:00:00Z",
            "source_session_label": "ft8_test_*",
            "first_wav_timestamp": "2026-05-30T00:00:00Z",
            "last_wav_timestamp": "2026-05-30T00:00:02Z",
            "span_seconds": 30.0,
            "entries": []
        });
        std::fs::write(tmp.path(), json.to_string()).unwrap();
        let err = ChronoReplayManifest::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("schema_version"));
    }
}

#[cfg(test)]
mod pan160_tests {
    use super::*;
    use crate::{BaselineCache, BaselineDecode};
    use sha2::{Digest, Sha256};

    #[test]
    fn strict_preflight_validates_all_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("sample.wav");
        std::fs::write(&wav, b"wav").unwrap();
        let sha = format!("{:x}", Sha256::digest(b"wav"));
        let cache_path = dir.path().join(format!("{sha}.json"));
        let cache = BaselineCache {
            schema_version: 1,
            wav_path: "sample.wav".into(),
            wav_sha256: sha.clone(),
            decoder_identity: "jt9 test".into(),
            decodes: vec![BaselineDecode {
                message: "CQ W1AW FN31".into(),
                freq_hz: 1000.,
                dt_s: 0.3,
                snr_db: -10.,
            }],
            elapsed_seconds: 1.,
        };
        let entries = vec![ChronoReplayEntry {
            wav_path: wav.clone(),
            wav_sha256: sha.clone(),
            slot_index: 0,
            wav_timestamp: "2026-01-01T00:00:00Z".into(),
        }];
        let save = |c: &BaselineCache| {
            std::fs::write(&cache_path, serde_json::to_vec(c).unwrap()).unwrap()
        };
        save(&cache);
        assert!(preflight_timing_history(&entries, None, dir.path()).is_empty());
        assert!(!preflight_timing_history(&[], None, dir.path()).is_empty());
        let mut bad = entries.clone();
        bad.push(entries[0].clone());
        assert!(!preflight_timing_history(&bad, None, dir.path()).is_empty());
        bad[1].slot_index = 1;
        assert!(!preflight_timing_history(&bad, None, dir.path()).is_empty());
        bad[1].wav_timestamp = "invalid".into();
        assert!(!preflight_timing_history(&bad, None, dir.path()).is_empty());
        for field in ["version", "sha", "json", "missing"] {
            let mut c = cache.clone();
            match field {
                "version" => {
                    c.schema_version = 99;
                    save(&c);
                }
                "sha" => {
                    c.wav_sha256 = "wrong".into();
                    save(&c);
                }
                "json" => std::fs::write(&cache_path, b"{}").unwrap(),
                _ => std::fs::remove_file(&cache_path).unwrap(),
            }
            assert!(
                !preflight_timing_history(&entries, None, dir.path()).is_empty(),
                "{field}"
            );
        }
        save(&cache);
        std::fs::write(&wav, b"wrong").unwrap();
        assert!(!preflight_timing_history(&entries, None, dir.path()).is_empty());
        std::fs::remove_file(&wav).unwrap();
        std::fs::remove_file(&cache_path).unwrap();
        assert_eq!(
            preflight_timing_history(&entries, None, dir.path()).len(),
            2
        );
        std::fs::write(&wav, b"wav").unwrap();
        save(&cache);
        let mut moved = entries;
        moved[0].wav_path = PathBuf::from("/unavailable/sample.wav");
        assert!(preflight_timing_history(&moved, Some(dir.path()), dir.path()).is_empty());
    }
}

/// A rejected input in the strict PAN-160 experiment preflight.
#[derive(Debug, Serialize)]
pub struct TimingInputProblem {
    pub slot_index: Option<u32>,
    pub path: PathBuf,
    pub expected_sha256: String,
    pub reason: String,
}

/// Validate every recording and typed truth cache before any decoding begins.
/// This is deliberately separate from the legacy tier loader.
pub fn preflight_timing_history(
    entries: &[ChronoReplayEntry],
    media_dir: Option<&Path>,
    baseline_dir: &Path,
) -> Vec<TimingInputProblem> {
    use sha2::{Digest, Sha256};
    let mut problems = Vec::new();
    if entries.is_empty() {
        problems.push(TimingInputProblem {
            slot_index: None,
            path: PathBuf::new(),
            expected_sha256: String::new(),
            reason: "empty slot list".into(),
        });
    }
    let mut previous_time = None;
    let mut digests = std::collections::HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let wav = media_dir
            .and_then(|dir| entry.wav_path.file_name().map(|name| dir.join(name)))
            .unwrap_or_else(|| entry.wav_path.clone());
        let mut reject = |path: PathBuf, reason: String| {
            problems.push(TimingInputProblem {
                slot_index: Some(entry.slot_index),
                path,
                expected_sha256: entry.wav_sha256.clone(),
                reason,
            })
        };
        if entry.slot_index as usize != index || !digests.insert(&entry.wav_sha256) {
            reject(wav.clone(), "duplicate or unordered slot".into());
        }
        match chrono::DateTime::parse_from_rfc3339(&entry.wav_timestamp) {
            Ok(at) if previous_time.is_none_or(|previous| at > previous) => {
                previous_time = Some(at)
            }
            _ => reject(wav.clone(), "invalid or non-increasing timestamp".into()),
        }
        match std::fs::read(&wav) {
            Ok(bytes) if format!("{:x}", Sha256::digest(&bytes)) == entry.wav_sha256 => {}
            Ok(_) => reject(wav.clone(), "WAV SHA mismatch".into()),
            Err(e) => reject(wav.clone(), e.to_string()),
        }
        let cache_path = baseline_dir.join(format!("{}.json", entry.wav_sha256));
        let cache = std::fs::read(&cache_path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<crate::BaselineCache>(&bytes).map_err(|e| e.to_string())
            });
        match cache {
            Ok(c)
                if c.schema_version == crate::BaselineCache::CURRENT_SCHEMA_VERSION
                    && c.wav_sha256 == entry.wav_sha256 => {}
            Ok(_) => reject(cache_path, "baseline version or SHA mismatch".into()),
            Err(e) => reject(cache_path, e),
        }
    }
    problems
}

#[cfg(test)]
mod timing_report_tests {
    use super::*;
    #[test]
    fn paired_identities_keep_losses_novelty_and_incomplete_status() {
        let control = TimingSlotResult {
            slot: 0,
            truth: vec!["A".into(), "B".into()],
            decoded: vec!["A".into(), "NOVEL".into()],
            eligible: vec!["W1AW".into()],
            ..Default::default()
        };
        let trial = TimingSlotResult {
            decoded: vec!["B".into()],
            eligible: vec![],
            ..control.clone()
        };
        let pair = compare_timing_slots(&[control.clone()], &[trial.clone()], 1);
        assert_eq!(pair["new"], serde_json::json!([[0, "B"]]));
        assert_eq!(pair["lost"], serde_json::json!([[0, "A"]]));
        assert_eq!(pair["recall_delta"], 0.);
        assert_eq!(pair["control_novel"], serde_json::json!([[0, "NOVEL"]]));
        assert_eq!(pair["eligible_control_slots"], serde_json::json!([0]));
        assert_eq!(compare_timing_slots(&[control], &[], 1)["status"], "failed");
        assert!(compare_timing_slots(&[], &[], 33)["recall_delta"].is_null());
        let loss = compare_timing_slots(
            &[trial.clone()],
            &[TimingSlotResult {
                decoded: vec![],
                ..trial
            }],
            1,
        );
        assert_eq!(loss["recall_delta"], -0.5);
    }
}

/// Per-slot identities, sufficient to recompute paired recall and novelty.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimingSlotResult {
    pub slot: usize,
    pub audio_sha256: String,
    pub truth: Vec<String>,
    pub decoded: Vec<String>,
    pub eligible: Vec<String>,
    pub elapsed_seconds: f64,
    pub diagnostics: serde_json::Value,
}

/// Paired comparison. Eligibility is frozen from the control arm, with each
/// arm's actual history retained in its slot records. Novel does not mean FP.
pub fn compare_timing_slots(
    control: &[TimingSlotResult],
    trial: &[TimingSlotResult],
    expected: usize,
) -> serde_json::Value {
    use std::collections::BTreeSet;
    let complete = expected > 0
        && control.len() == expected
        && trial.len() == expected
        && control.iter().zip(trial).enumerate().all(|(i, (a, b))| {
            a.slot == i && b.slot == i && a.audio_sha256 == b.audio_sha256 && a.truth == b.truth
        });
    if !complete {
        return serde_json::json!({"status":"failed", "recall_delta":null, "reason":"incomplete or unpaired session"});
    }
    let recovered = |slots: &[TimingSlotResult]| -> BTreeSet<(usize, String)> {
        slots
            .iter()
            .flat_map(|s| {
                s.truth
                    .iter()
                    .filter(|m| s.decoded.contains(m))
                    .map(|m| (s.slot, m.clone()))
            })
            .collect()
    };
    let novel = |slots: &[TimingSlotResult]| -> BTreeSet<(usize, String)> {
        slots
            .iter()
            .flat_map(|s| {
                s.decoded
                    .iter()
                    .filter(|m| !s.truth.contains(m))
                    .map(|m| (s.slot, m.clone()))
            })
            .collect()
    };
    let a = recovered(control);
    let b = recovered(trial);
    let truth: usize = control
        .iter()
        .map(|s| s.truth.iter().collect::<BTreeSet<_>>().len())
        .sum();
    let eligible: Vec<_> = control
        .iter()
        .filter(|s| !s.eligible.is_empty())
        .map(|s| s.slot)
        .collect();
    let new: Vec<_> = b.difference(&a).cloned().collect();
    let lost: Vec<_> = a.difference(&b).cloned().collect();
    let cohort = |select: &dyn Fn(usize) -> bool| {
        serde_json::json!({
        "control_recovered":a.iter().filter(|(slot,_)|select(*slot)).count(),
        "trial_recovered":b.iter().filter(|(slot,_)|select(*slot)).count(),
        "truth":control.iter().filter(|s|select(s.slot)).map(|s|s.truth.iter().collect::<BTreeSet<_>>().len()).sum::<usize>()})
    };
    serde_json::json!({"status":if trial.iter().any(|s| s.diagnostics["ap"]["attempts"].as_u64().unwrap_or(0)>0 || s.diagnostics["v2_hits"].as_u64().unwrap_or(0)>0) {"measured"} else {"inconclusive_no_eligible_priors"},
        "completed_slots":expected, "truth":truth, "control_recovered":a.len(), "trial_recovered":b.len(),
        "recall_delta":if truth>0 {Some((b.len() as f64-a.len() as f64)/truth as f64)} else {None},
        "new":new, "lost":lost, "control_novel":novel(control), "trial_novel":novel(trial),
        "eligible_control_slots":eligible,
        "cold":cohort(&|slot| !eligible.contains(&slot)),
        "eligible":cohort(&|slot| eligible.contains(&slot)),
        "historical_slots_1_5":cohort(&|slot| slot<5),
        "historical_slots_6_33":cohort(&|slot| (5..33).contains(&slot)),
        "control_elapsed_seconds":control.iter().map(|s|s.elapsed_seconds).sum::<f64>(),
        "trial_elapsed_seconds":trial.iter().map(|s|s.elapsed_seconds).sum::<f64>()})
}
