//! Corpus loaders. Plan 1 covers the fixtures tier only; curated + synth
//! land in plan 2.

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
/// regression test suite). Plan 1 returns just the paths with empty
/// `expected_messages`; plan 2 will read a `research/corpus/fixtures/ft8/truth.json`
/// to populate expected messages, but for plan 1 the fixtures tier is a
/// build-and-decode smoke test only — "did decode return at least one
/// message and not error."
pub fn load_ft8_fixtures(workspace_root: &Path) -> anyhow::Result<Vec<FixtureEntry>> {
    let mut out = Vec::new();
    for sub in ["generated", "wsjt"] {
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
                let display = format!(
                    "{}/{}",
                    sub,
                    path.file_name().unwrap().to_string_lossy()
                );
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
