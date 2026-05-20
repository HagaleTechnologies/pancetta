//! Hand-labeled expected decodes per fixture, loaded from
//! `research/corpus/fixtures/ft8/truth.json`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureCategory {
    /// Decoder must produce the exact messages in `expect`.
    Exact,
    /// Decoder must produce ≥ 1 message, content unspecified.
    AnyDecode,
    /// Fixture is known-undecodable today; tracked but doesn't penalize.
    Skip,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FixtureEntry {
    pub category: FixtureCategory,
    /// Expected message texts. For Exact: every text must appear in decoder output.
    /// For AnyDecode: contains the single sentinel "any-decode".
    /// For Skip: empty list.
    pub expect: Vec<String>,
    pub notes: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FixtureTruth {
    pub schema_version: u32,
    pub fixtures: BTreeMap<String, FixtureEntry>,
}

impl FixtureTruth {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let truth: FixtureTruth = serde_json::from_str(&s)?;
        anyhow::ensure!(
            truth.schema_version == Self::CURRENT_SCHEMA_VERSION,
            "FixtureTruth schema_version {} not supported (expected {})",
            truth.schema_version,
            Self::CURRENT_SCHEMA_VERSION,
        );
        Ok(truth)
    }

    /// Look up a fixture by its relative path (e.g. `"generated/ft8_cq.wav"`).
    pub fn get(&self, rel_path: &str) -> Option<&FixtureEntry> {
        self.fixtures.get(rel_path)
    }
}
