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
