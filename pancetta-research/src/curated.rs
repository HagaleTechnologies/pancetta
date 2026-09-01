//! Curated corpus manifest: a JSON list of operator-recording WAVs ranked
//! by "interesting-ness" (busy band, marginal decodes, high noise floor).
//! The manifest references WAVs by absolute path + SHA-256; the actual
//! WAVs live in `~/.pancetta/recordings/` and are never committed.

use crate::baseline_cache::BaselineCache;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
        anyhow::ensure!(
            entry.wav_sha256.len() == 64
                && entry
                    .wav_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "invalid WAV SHA-256: {}",
            entry.wav_sha256
        );
        if !entry.wav_path.is_file() {
            missing.push(format!("missing WAV: {}", entry.wav_path.display()));
        }
        let baseline = baseline_dir.join(format!("{}.json", entry.wav_sha256));
        if !baseline.is_file() {
            missing.push(format!("missing baseline cache: {}", baseline.display()));
        } else {
            // A stale cache renamed for another WAV, or a syntactically valid
            // `{}`, would previously pass this check on existence alone and
            // later get silently treated as an empty truth set by
            // `unwrap_or_default()` in the scoring path — plausible but
            // wrong recall/bootstrap results. Deserialize into the typed
            // schema and cross-check version + wav_sha256 against the
            // manifest entry it's supposed to back.
            match std::fs::read_to_string(&baseline) {
                Err(error) => missing.push(format!(
                    "unreadable baseline cache: {} ({error})",
                    baseline.display()
                )),
                Ok(contents) => match serde_json::from_str::<BaselineCache>(&contents) {
                    Err(error) => missing.push(format!(
                        "malformed baseline cache: {} ({error})",
                        baseline.display()
                    )),
                    Ok(cache) => {
                        if cache.schema_version != BaselineCache::CURRENT_SCHEMA_VERSION {
                            missing.push(format!(
                                "baseline cache schema_version {} != {} (expected): {}",
                                cache.schema_version,
                                BaselineCache::CURRENT_SCHEMA_VERSION,
                                baseline.display()
                            ));
                        }
                        if !cache.wav_sha256.eq_ignore_ascii_case(&entry.wav_sha256) {
                            missing.push(format!(
                                "baseline cache wav_sha256 mismatch: {} names {} but manifest expects {}",
                                baseline.display(),
                                cache.wav_sha256,
                                entry.wav_sha256
                            ));
                        }
                    }
                },
            }
        }
        if entry.wav_path.is_file() {
            let actual = format!("{:x}", Sha256::digest(std::fs::read(&entry.wav_path)?));
            if !actual.eq_ignore_ascii_case(&entry.wav_sha256) {
                missing.push(format!(
                    "WAV SHA-256 mismatch: {} expected {} got {}",
                    entry.wav_path.display(),
                    entry.wav_sha256,
                    actual
                ));
            }
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
        let sha = "deadbeef".repeat(8);
        let error =
            preflight_curated_corpus(&[entry(temp.path().join("absent.wav"), &sha)], temp.path())
                .unwrap_err()
                .to_string();
        assert!(error.contains("absent.wav"));
        assert!(error.contains(&format!("{sha}.json")));
    }

    fn valid_baseline_json(wav_sha256: &str) -> String {
        serde_json::to_string(&BaselineCache {
            schema_version: BaselineCache::CURRENT_SCHEMA_VERSION,
            wav_path: "sample.wav".into(),
            wav_sha256: wav_sha256.into(),
            decoder_identity: "jt9 (test)".into(),
            decodes: vec![],
            elapsed_seconds: 0.1,
        })
        .unwrap()
    }

    #[test]
    fn preflight_accepts_complete_pair() {
        let temp = tempfile::tempdir().unwrap();
        let wav = temp.path().join("sample.wav");
        std::fs::write(&wav, b"wav").unwrap();
        let sha = "61e25c2cdda1758ce167dbeb7e6d776c5cd6c2f12168f190ef0dca674ff60e6c";
        std::fs::write(
            temp.path().join(format!("{sha}.json")),
            valid_baseline_json(sha),
        )
        .unwrap();
        preflight_curated_corpus(&[entry(wav, sha)], temp.path()).unwrap();
    }

    #[test]
    fn preflight_rejects_syntactically_valid_but_schema_empty_cache() {
        let temp = tempfile::tempdir().unwrap();
        let wav = temp.path().join("sample.wav");
        std::fs::write(&wav, b"wav").unwrap();
        let sha = "61e25c2cdda1758ce167dbeb7e6d776c5cd6c2f12168f190ef0dca674ff60e6c";
        // A syntactically valid but empty `{}` cache must not silently pass —
        // it previously got treated as a zero-decode truth set downstream.
        std::fs::write(temp.path().join(format!("{sha}.json")), b"{}").unwrap();
        let error = preflight_curated_corpus(&[entry(wav, sha)], temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("malformed baseline cache"));
    }

    #[test]
    fn preflight_rejects_baseline_cache_naming_a_different_wav_sha() {
        let temp = tempfile::tempdir().unwrap();
        let wav = temp.path().join("sample.wav");
        std::fs::write(&wav, b"wav").unwrap();
        let sha = "61e25c2cdda1758ce167dbeb7e6d776c5cd6c2f12168f190ef0dca674ff60e6c";
        let other_sha = "0".repeat(64);
        std::fs::write(
            temp.path().join(format!("{sha}.json")),
            valid_baseline_json(&other_sha),
        )
        .unwrap();
        let error = preflight_curated_corpus(&[entry(wav, sha)], temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("wav_sha256 mismatch"));
    }
}
