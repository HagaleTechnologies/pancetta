//! Curated corpus manifest: a JSON list of operator-recording WAVs ranked
//! by "interesting-ness" (busy band, marginal decodes, high noise floor).
//! The manifest references WAVs by absolute path + SHA-256; the actual
//! WAVs live in `~/.pancetta/recordings/` and are never committed.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CuratedEntry {
    /// Absolute path to the WAV file on the operator's machine.
    pub wav_path: PathBuf,
    /// SHA-256 hex of the WAV file content (for cache lookup against baselines).
    pub wav_sha256: String,
    /// Interesting-ness score (higher = more interesting; see curate binary docs).
    pub interest_score: f64,
    /// Per-criterion scores that summed to interest_score.
    pub score_breakdown: ScoreBreakdown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    /// Number of messages pancetta decodes from this WAV.
    pub pancetta_decode_count: u32,
    /// Estimated noise floor in dB.
    pub noise_floor_db: f64,
    /// Mean SNR (dB) of pancetta's decodes from this WAV; None if no decodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_decoded_snr_db: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CuratedManifest {
    pub schema_version: u32,
    /// Human-readable label: "hard_200", "hard_1000", "wild_50", etc.
    pub label: String,
    /// When this manifest was produced (ISO 8601 UTC).
    pub generated_at: String,
    /// The decoder identity used during curation scoring.
    pub scoring_decoder: String,
    pub entries: Vec<CuratedEntry>,
}

impl CuratedManifest {
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
        let m: CuratedManifest = serde_json::from_str(&s)?;
        anyhow::ensure!(
            m.schema_version == Self::CURRENT_SCHEMA_VERSION,
            "CuratedManifest schema_version {} not supported (expected {})",
            m.schema_version,
            Self::CURRENT_SCHEMA_VERSION,
        );
        Ok(m)
    }
}

/// Load a curated manifest from disk. The manifest's wav_path entries are
/// expected to be absolute (curate writes them that way); no rewriting needed.
pub fn load_curated_corpus(manifest_path: &Path) -> anyhow::Result<Vec<CuratedEntry>> {
    let manifest = CuratedManifest::load(manifest_path)?;
    Ok(manifest.entries)
}

/// Refuse to score unless every WAV and corresponding jt9 cache exists.
pub fn preflight_curated_corpus(
    entries: &[CuratedEntry],
    baseline_dir: &Path,
) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    for entry in entries {
        if !entry.wav_path.is_file() {
            missing.push(format!("missing WAV: {}", entry.wav_path.display()));
        }
        let baseline = baseline_dir.join(format!("{}.json", entry.wav_sha256));
        if !baseline.is_file() {
            missing.push(format!("missing baseline cache: {}", baseline.display()));
        }
    }
    anyhow::ensure!(
        missing.is_empty(),
        "curated corpus preflight failed:\n{}",
        missing.join("\n")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(wav_path: PathBuf, sha: &str) -> CuratedEntry {
        CuratedEntry {
            wav_path,
            wav_sha256: sha.into(),
            interest_score: 0.0,
            score_breakdown: ScoreBreakdown::default(),
        }
    }

    #[test]
    fn preflight_names_missing_wav_and_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let error = preflight_curated_corpus(
            &[entry(temp.path().join("absent.wav"), "deadbeef")],
            temp.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("absent.wav"));
        assert!(error.contains("deadbeef.json"));
    }

    #[test]
    fn preflight_accepts_complete_pair() {
        let temp = tempfile::tempdir().unwrap();
        let wav = temp.path().join("sample.wav");
        std::fs::write(&wav, b"wav").unwrap();
        std::fs::write(temp.path().join("abc.json"), b"{}").unwrap();
        preflight_curated_corpus(&[entry(wav, "abc")], temp.path()).unwrap();
    }
}
