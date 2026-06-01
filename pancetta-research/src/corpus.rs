//! Corpus loaders. Plan 1 covers the fixtures tier; plan 2 adds synth-clean.
//! Curated tiers land in plan 3.

use std::path::{Path, PathBuf};

/// A fixture WAV plus the messages we expect a healthy decoder to produce.
#[derive(Clone, Debug)]
pub struct FixtureEntry {
    pub wav_path: PathBuf,
    pub display_name: String,
    /// Messages we expect to be present in the decode output. If any expected
    /// message is missing, the fixture fails.
    pub expected_messages: Vec<String>,
}

/// Discover all fixture WAVs that ship with pancetta-ft8 (used by the
/// regression test suite). Returns entries with empty `expected_messages` —
/// truth.json is read separately by the eval binary (`FixtureTruth::load`),
/// which controls pass/fail categorization. This function returns paths only.
pub fn load_ft8_fixtures(workspace_root: &Path) -> anyhow::Result<Vec<FixtureEntry>> {
    let mut out = Vec::new();
    // All four fixture subdirs: generated/ (our encoded test signals),
    // wsjt/ (WSJT-X golden), basicft8/ (ft8_lib reference), jtdx/
    // (JTDX-recorded off-air). Truth.json holds per-fixture expectations.
    for sub in ["generated", "wsjt", "basicft8", "jtdx"] {
        let dir = workspace_root
            .join("pancetta-ft8/tests/fixtures/wav")
            .join(sub);
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "wav") {
                let display = format!("{}/{}", sub, path.file_name().unwrap().to_string_lossy());
                out.push(FixtureEntry {
                    wav_path: path,
                    display_name: display,
                    expected_messages: Vec::new(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Ok(out)
}

use crate::synth::{SynthManifest, SynthPairManifest};

/// One synth corpus entry, denormalized for the eval binary's convenience.
#[derive(Clone, Debug)]
pub struct SynthCorpusEntry {
    pub wav_path: PathBuf,
    pub encoded_message: String,
    pub snr_db: f64,
}

/// Load a synth manifest from disk and resolve all wav paths relative to
/// the workspace root.
pub fn load_synth_corpus(
    workspace_root: &Path,
    manifest_path: &Path,
) -> anyhow::Result<Vec<SynthCorpusEntry>> {
    let manifest = SynthManifest::load(manifest_path)?;
    let entries = manifest
        .entries
        .iter()
        .map(|e| SynthCorpusEntry {
            wav_path: workspace_root.join(&e.wav_path),
            encoded_message: e.encoded_message.clone(),
            snr_db: e.snr_db,
        })
        .collect();
    Ok(entries)
}

/// hb-146 — one pair-synth corpus entry. Each WAV contains two FT8
/// signals at controlled (ΔSNR, Δf, Δt). Two truths per WAV; recall is
/// per-message.
#[derive(Clone, Debug)]
pub struct SynthPairCorpusEntry {
    pub wav_path: PathBuf,
    pub message_strong: String,
    pub message_weak: String,
    pub strong_snr_db: f64,
    pub delta_snr_db: f64,
    pub delta_freq_hz: f64,
    pub delta_time_s: f64,
}

/// hb-146 — load a pair-synth manifest from disk and resolve wav paths.
pub fn load_synth_pair_corpus(
    workspace_root: &Path,
    manifest_path: &Path,
) -> anyhow::Result<Vec<SynthPairCorpusEntry>> {
    let manifest = SynthPairManifest::load(manifest_path)?;
    let entries = manifest
        .entries
        .iter()
        .map(|e| SynthPairCorpusEntry {
            wav_path: workspace_root.join(&e.wav_path),
            message_strong: e.message_strong.clone(),
            message_weak: e.message_weak.clone(),
            strong_snr_db: e.strong_snr_db,
            delta_snr_db: e.delta_snr_db,
            delta_freq_hz: e.delta_freq_hz,
            delta_time_s: e.delta_time_s,
        })
        .collect();
    Ok(entries)
}
