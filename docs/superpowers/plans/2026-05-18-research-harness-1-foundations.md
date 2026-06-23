# Decoder Research Harness — Plan 1 of 3: Foundations

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lay the foundations for the decoder research harness — new `pancetta-research` crate excluded from CI, scorecard JSON schema, `DecoderUnderTest` trait with `Ft8Decoder` impl, the `scripts/research-env.sh` disk-hygiene script, the `research/` directory skeleton, and a first end-to-end smoke that runs `eval` against the 82 fixture WAVs and emits a real scorecard. Plans 2 (full eval pipeline + corpus tiers) and 3 (curation + leaderboard + experiment lifecycle) come after this one.

**Architecture:** New crate `pancetta-research` lives in the workspace but is **excluded from `default-members`** so neither `cargo build`/`cargo test` from the root nor CI ever touches it. It depends on `pancetta-ft8` for the decoder under test, on `serde`/`serde_json` for the scorecard, and on a few utility crates. The script `scripts/research-env.sh` is bash, handles preflight/audit/cleanup of on-disk research artifacts, and starts as the gatekeeper for any `eval` invocation. The `research/` directory is plain markdown + JSON.

**Tech Stack:** Rust 2021 (workspace edition), serde/serde_json, hound (WAV reader — already in workspace dev-deps), bash for the script. No new heavy dependencies. PyTorch / neural networks not in this plan.

**Spec:** `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`

---

## File Map

**Workspace (root):**
- Modify: `Cargo.toml` — add `pancetta-research` as a workspace member; explicitly omit from `default-members`.
- Modify: `.gitignore` — add research-related ignore rules (synth wavs, on-disk artifacts).

**New crate `pancetta-research/`:**
- Create: `pancetta-research/Cargo.toml`
- Create: `pancetta-research/README.md` — local-only banner.
- Create: `pancetta-research/src/lib.rs` — public re-exports.
- Create: `pancetta-research/src/scorecard.rs` — `Scorecard` struct + JSON schema (v1).
- Create: `pancetta-research/src/decoder.rs` — `DecoderUnderTest` trait + `Ft8Decoder` impl.
- Create: `pancetta-research/src/mode.rs` — `Mode` enum (only `Ft8` for now).
- Create: `pancetta-research/src/corpus.rs` — fixtures loader (other tiers in plan 2).
- Create: `pancetta-research/src/metrics.rs` — composite metric calculation.
- Create: `pancetta-research/src/bin/eval.rs` — first cut: fixtures tier only, emits scorecard.
- Create: `pancetta-research/tests/schema_roundtrip.rs` — scorecard JSON round-trip test.
- Create: `pancetta-research/tests/decoder_smoke.rs` — `Ft8Decoder` decodes a known fixture.
- Create: `pancetta-research/tests/eval_fixtures.rs` — full `eval` binary smoke test.
- Create: `pancetta-research/tests/ci_guard.rs` — fails build if workflow files reference research.

**Scripts:**
- Create: `scripts/research-env.sh` — preflight, audit, guard-ci subcommands; other subcommands in plan 3.
- Modify: `scripts/check.sh` — invoke `research-env.sh --guard-ci` as a step.

**`research/` directory skeleton:**
- Create: `research/README.md` — what this directory is.
- Create: `research/hypothesis_bank.md` — header only; entries come in plan 3.
- Create: `research/scorecards/.gitkeep`
- Create: `research/scorecards/history/.gitkeep`
- Create: `research/experiments/.gitkeep`
- Create: `research/baselines/ft8/.gitkeep`
- Create: `research/corpus/fixtures/ft8/.gitkeep`
- Create: `research/corpus/curated/ft8/.gitkeep`
- Create: `research/corpus/synth/manifests/.gitkeep`

**Documentation:**
- Modify: `CLAUDE.md` — note new crate + research/ dir under "Workspace Structure".

---

## Phase A — Workspace + crate scaffold

### Task 1: Add `pancetta-research` to the workspace, excluded from default-members

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Read current workspace Cargo.toml**

Run: `cat Cargo.toml | head -40`

Expected: find `[workspace]` block with a `members = [...]` list. Note whether `default-members` is currently set.

- [ ] **Step 2: Add `pancetta-research` to `members`**

In `Cargo.toml` `[workspace]` block, add `"pancetta-research"` to the `members` array (keep alphabetical order):

```toml
[workspace]
members = [
    "pancetta",
    "pancetta-audio",
    "pancetta-config",
    "pancetta-core",
    "pancetta-cqdx",
    "pancetta-dsp",
    "pancetta-dx",
    "pancetta-ft8",
    "pancetta-hamlib",
    "pancetta-qso",
    "pancetta-research",
    "pancetta-tui",
]
```

- [ ] **Step 3: Explicitly set `default-members` to exclude research**

Below `members`, add:

```toml
default-members = [
    "pancetta",
    "pancetta-audio",
    "pancetta-config",
    "pancetta-core",
    "pancetta-cqdx",
    "pancetta-dsp",
    "pancetta-dx",
    "pancetta-ft8",
    "pancetta-hamlib",
    "pancetta-qso",
    "pancetta-tui",
]
```

Note: `pancetta-research` is in `members` but not `default-members`. Root-level `cargo build` will skip it.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml
git commit -m "build: add pancetta-research to workspace, excluded from default-members"
```

---

### Task 2: Create the `pancetta-research` crate skeleton

**Files:**
- Create: `pancetta-research/Cargo.toml`
- Create: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Make the crate directory and Cargo.toml**

```bash
mkdir -p pancetta-research/src/bin
mkdir -p pancetta-research/tests
```

Create `pancetta-research/Cargo.toml`:

```toml
[package]
name = "pancetta-research"
version = "0.1.0"
edition = "2021"
publish = false                          # never published; local-only research crate

[lib]
name = "pancetta_research"
path = "src/lib.rs"

[features]
# All eval-time corpus-touching tests gate on this feature so default test passes
# do not require corpus files to exist.
research-eval = []

[dependencies]
pancetta-ft8 = { path = "../pancetta-ft8" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
chrono = { version = "0.4", features = ["serde"] }
hound = "3.5"                            # WAV reader (already used in pancetta-ft8)

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create the empty lib.rs**

Create `pancetta-research/src/lib.rs`:

```rust
//! Local-only research harness for the pancetta decoder.
//!
//! This crate is **excluded from the workspace `default-members`** and never
//! built in CI. See `pancetta-research/README.md` and
//! `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p pancetta-research`

Expected: compiles cleanly, no warnings, "Finished `dev`".

- [ ] **Step 4: Verify root build still skips it**

Run: `cargo build`

Expected: builds the workspace default-members; "Compiling pancetta-research" should NOT appear in the output.

Quick grep to confirm: `cargo build 2>&1 | grep pancetta-research` — should produce no output.

- [ ] **Step 5: Commit**

```bash
git add pancetta-research/Cargo.toml pancetta-research/src/lib.rs
git commit -m "feat(research): scaffold pancetta-research crate (local-only, no CI)"
```

---

### Task 3: Add the local-only README banner

**Files:**
- Create: `pancetta-research/README.md`

- [ ] **Step 1: Write the README**

Create `pancetta-research/README.md`:

```markdown
# pancetta-research

**Local-only crate. Builds and runs from your dev machine only. No GitHub
Actions, no CI, no cron — burns Actions minutes for no benefit. If you find
yourself wiring this into CI, stop.**

This crate is the iteration harness for improving the pancetta decoder. It is
deliberately excluded from the workspace `default-members`, so `cargo build`
and `cargo test` from the repo root skip it entirely.

## Quick start

```bash
# Build the harness
cargo build --release -p pancetta-research

# Run a fixtures-only eval (smoke test; ~10 s)
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures \
    --mode ft8 \
    --output research/scorecards/main.json

# Run the disk hygiene check
./scripts/research-env.sh --preflight
```

## Why this is local-only

The full corpus (~7.5 GB of operator recordings in `~/.pancetta/recordings/`)
lives on the operator's machine, not in git. The harness builds a curated
subset, runs the decoder against it, and produces scorecards. Running this in
CI would (a) burn Actions minutes on an iteration loop that is inherently
operator-driven and (b) not have access to the real-world WAV corpus anyway.

## Design

See `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.

## Implementation plans

- Plan 1 of 3 (foundations): `docs/superpowers/plans/2026-05-18-research-harness-1-foundations.md` — this one
- Plan 2 of 3 (eval pipeline + corpus): written after plan 1 lands
- Plan 3 of 3 (curation + leaderboard + lifecycle): written after plan 2 lands
```

- [ ] **Step 2: Commit**

```bash
git add pancetta-research/README.md
git commit -m "docs(research): local-only banner + quick start"
```

---

## Phase B — Scorecard schema

### Task 4: Define the `Mode` enum

**Files:**
- Create: `pancetta-research/src/mode.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Create the mode module**

Create `pancetta-research/src/mode.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Ft8,
    // Future: Ft4, Js8, Jt9, Jt65, Msk144. Add when their decoders exist.
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Ft8 => "ft8",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "ft8" => Ok(Mode::Ft8),
            other => Err(format!("unknown mode: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json() {
        let json = serde_json::to_string(&Mode::Ft8).unwrap();
        assert_eq!(json, "\"ft8\"");
        let back: Mode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Mode::Ft8);
    }

    #[test]
    fn parse_from_string() {
        assert_eq!("ft8".parse::<Mode>().unwrap(), Mode::Ft8);
        assert_eq!("FT8".parse::<Mode>().unwrap(), Mode::Ft8);
        assert!("ft4".parse::<Mode>().is_err());
    }
}
```

- [ ] **Step 2: Re-export from lib.rs**

In `pancetta-research/src/lib.rs`, add:

```rust
pub mod mode;
pub use mode::Mode;
```

- [ ] **Step 3: Run the unit tests**

Run: `cargo test -p pancetta-research mode::tests`

Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/mode.rs pancetta-research/src/lib.rs
git commit -m "feat(research): Mode enum (ft8-only for now)"
```

---

### Task 5: Define the `Scorecard` struct + JSON schema v1

**Files:**
- Create: `pancetta-research/src/scorecard.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Write the scorecard module**

Create `pancetta-research/src/scorecard.rs`:

```rust
use crate::Mode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Top-level scorecard JSON document. See spec section "Eval binary —
/// Scorecard JSON shape".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scorecard {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub mode: Mode,
    pub git: GitInfo,
    pub build: BuildInfo,
    pub harness: HarnessInfo,
    pub config: ConfigInfo,
    /// Keyed by tier name ("synth-clean", "fixtures", "curated-hard-200", …)
    pub tiers: BTreeMap<String, TierResult>,
    pub composite: CompositeInfo,
    pub regressions: RegressionFlags,
    pub notes: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitInfo {
    pub branch: String,
    pub head_sha: String,
    pub main_merge_base: String,
    pub dirty: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildInfo {
    pub rustc_version: String,
    pub release: bool,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub harness_version: String,
    pub host: String,
    pub cores_used: usize,
    pub elapsed_seconds: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigInfo {
    pub decoder: Value,          // opaque snapshot of the decoder config
    pub seed: u64,
    pub tiers_run: Vec<String>,
}

/// Per-tier results. Sparse: only fields relevant to the tier are populated.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TierResult {
    pub wavs_processed: u32,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub by_snr_db: Vec<SnrBin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_at_50pct_recovery_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_at_90pct_recovery_db: Option<f64>,
    #[serde(default)]
    pub false_positives_total: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixtures_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixtures_passed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixtures_failed: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub failures: Vec<FixtureFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truth_decodes_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truth_decodes_recovered: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_rate: Option<f64>,
    #[serde(default)]
    pub novel_decodes: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wsjtx_decoded: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jtdx_decoded: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vs_wsjtx_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vs_jtdx_pct: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub per_wav_top_failures: Vec<PerWavFailure>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnrBin {
    pub snr_db: f64,
    pub attempts: u32,
    pub decoded: u32,
    pub fp: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixtureFailure {
    pub wav: String,
    pub expected: Vec<String>,
    pub got: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerWavFailure {
    pub wav_hash: String,
    pub truth: u32,
    pub recovered: u32,
    pub wsjtx: u32,
    pub jtdx: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompositeInfo {
    pub weights: BTreeMap<String, f64>,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_baseline_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_vs_main: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegressionFlags {
    pub fixture_regression: bool,
    pub false_positive_introduced: bool,
    pub snr_curve_regression_db: f64,
}

impl Scorecard {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn save<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<()> {
        let pretty = serde_json::to_string_pretty(self)?;
        std::fs::write(path, pretty)?;
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let card: Scorecard = serde_json::from_str(&s)?;
        if card.schema_version != Self::CURRENT_SCHEMA_VERSION {
            anyhow::bail!(
                "scorecard schema_version {} not supported (expected {})",
                card.schema_version,
                Self::CURRENT_SCHEMA_VERSION,
            );
        }
        Ok(card)
    }
}
```

- [ ] **Step 2: Re-export from lib.rs**

In `pancetta-research/src/lib.rs`, add:

```rust
pub mod scorecard;
pub use scorecard::Scorecard;
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p pancetta-research`

Expected: compiles cleanly.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/scorecard.rs pancetta-research/src/lib.rs
git commit -m "feat(research): scorecard JSON schema v1"
```

---

### Task 6: Round-trip test for scorecard JSON

**Files:**
- Create: `pancetta-research/tests/schema_roundtrip.rs`

- [ ] **Step 1: Write the round-trip test**

Create `pancetta-research/tests/schema_roundtrip.rs`:

```rust
use chrono::Utc;
use pancetta_research::scorecard::{
    BuildInfo, CompositeInfo, ConfigInfo, GitInfo, HarnessInfo, RegressionFlags, Scorecard,
    TierResult,
};
use pancetta_research::Mode;
use serde_json::json;
use std::collections::BTreeMap;

fn sample_scorecard() -> Scorecard {
    let mut tiers = BTreeMap::new();
    tiers.insert(
        "fixtures".to_string(),
        TierResult {
            wavs_processed: 82,
            fixtures_total: Some(82),
            fixtures_passed: Some(80),
            fixtures_failed: Some(2),
            pass_rate: Some(0.9756),
            ..Default::default()
        },
    );
    let mut weights = BTreeMap::new();
    weights.insert("fixtures_pass_rate".to_string(), 1.0);
    Scorecard {
        schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
        generated_at: Utc::now(),
        mode: Mode::Ft8,
        git: GitInfo {
            branch: "main".to_string(),
            head_sha: "abc1234".to_string(),
            main_merge_base: "abc1234".to_string(),
            dirty: false,
        },
        build: BuildInfo {
            rustc_version: "1.85.0".to_string(),
            release: true,
            features: vec!["transmit".into(), "research-eval".into()],
        },
        harness: HarnessInfo {
            harness_version: env!("CARGO_PKG_VERSION").to_string(),
            host: "darwin/arm64".to_string(),
            cores_used: 10,
            elapsed_seconds: 12.5,
        },
        config: ConfigInfo {
            decoder: json!({"placeholder": "decoder config snapshot"}),
            seed: 42,
            tiers_run: vec!["fixtures".to_string()],
        },
        tiers,
        composite: CompositeInfo {
            weights,
            score: 0.9756,
            main_baseline_score: None,
            delta_vs_main: None,
        },
        regressions: RegressionFlags::default(),
        notes: "Smoke test scorecard.".to_string(),
    }
}

#[test]
fn scorecard_round_trips_to_disk() {
    let card = sample_scorecard();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    card.save(tmp.path()).unwrap();
    let back = Scorecard::load(tmp.path()).unwrap();
    assert_eq!(card.schema_version, back.schema_version);
    assert_eq!(card.mode, back.mode);
    assert_eq!(card.tiers.len(), back.tiers.len());
    assert!(back.tiers.contains_key("fixtures"));
    assert_eq!(card.composite.score, back.composite.score);
}

#[test]
fn scorecard_load_rejects_wrong_schema_version() {
    let mut card = sample_scorecard();
    card.schema_version = 999;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    card.save(tmp.path()).unwrap();
    let err = Scorecard::load(tmp.path()).unwrap_err();
    assert!(err.to_string().contains("schema_version"));
}

#[test]
fn scorecard_json_omits_empty_optional_fields() {
    let card = sample_scorecard();
    let json = serde_json::to_string(&card).unwrap();
    // Empty Vec fields should be skipped, not serialized as [].
    assert!(!json.contains("\"by_snr_db\":[]"));
    assert!(!json.contains("\"failures\":[]"));
    // Optional fields that are None should be skipped.
    assert!(!json.contains("\"truth_decodes_total\":null"));
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p pancetta-research --test schema_roundtrip`

Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add pancetta-research/tests/schema_roundtrip.rs
git commit -m "test(research): scorecard JSON round-trip + schema version check"
```

---

### Task 7: Composite metric calculation

**Files:**
- Create: `pancetta-research/src/metrics.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Write the metrics module**

Create `pancetta-research/src/metrics.rs`:

```rust
use crate::scorecard::{CompositeInfo, Scorecard, TierResult};
use std::collections::BTreeMap;

/// Default composite-metric weights (spec section "Composite Metric").
pub fn default_weights() -> BTreeMap<String, f64> {
    let mut w = BTreeMap::new();
    w.insert("real_decode_rate_hard_200".to_string(), 0.50);
    w.insert("snr_50pct_synth_clean".to_string(), 0.30);
    w.insert("fixtures_pass_rate".to_string(), 0.15);
    w.insert("snr_50pct_synth_doppler".to_string(), 0.05);
    w
}

/// Map an SNR-at-50%-recovery value (in dB; more negative is better) to a
/// [0, 1] score. clamp((-snr - 10) / 20, 0, 1) — so -30 dB → 1.0, -10 dB → 0.0.
pub fn normalize_snr_db(snr_db: f64) -> f64 {
    let raw = (-snr_db - 10.0) / 20.0;
    raw.clamp(0.0, 1.0)
}

/// Compute the composite score for a scorecard. Missing tiers contribute 0
/// for their term (i.e. the metric degrades gracefully when not all tiers
/// were run; the engineer sees the result but should treat it as partial).
pub fn compute_composite(
    weights: &BTreeMap<String, f64>,
    tiers: &BTreeMap<String, TierResult>,
) -> f64 {
    let real_rate = tiers
        .get("curated-hard-200")
        .and_then(|t| t.decode_rate)
        .unwrap_or(0.0);
    let snr_clean = tiers
        .get("synth-clean")
        .and_then(|t| t.snr_at_50pct_recovery_db)
        .map(normalize_snr_db)
        .unwrap_or(0.0);
    let fixtures = tiers
        .get("fixtures")
        .and_then(|t| t.pass_rate)
        .unwrap_or(0.0);
    let snr_doppler = tiers
        .get("synth-doppler")
        .and_then(|t| t.snr_at_50pct_recovery_db)
        .map(normalize_snr_db)
        .unwrap_or(0.0);

    weights.get("real_decode_rate_hard_200").copied().unwrap_or(0.0) * real_rate
        + weights.get("snr_50pct_synth_clean").copied().unwrap_or(0.0) * snr_clean
        + weights.get("fixtures_pass_rate").copied().unwrap_or(0.0) * fixtures
        + weights.get("snr_50pct_synth_doppler").copied().unwrap_or(0.0) * snr_doppler
}

/// Fill in the CompositeInfo on a scorecard from its tiers + the given weights.
pub fn populate_composite(card: &mut Scorecard, weights: BTreeMap<String, f64>) {
    let score = compute_composite(&weights, &card.tiers);
    card.composite = CompositeInfo {
        weights,
        score,
        main_baseline_score: None,
        delta_vs_main: None,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scorecard::TierResult;

    #[test]
    fn normalize_snr_boundary_conditions() {
        assert_eq!(normalize_snr_db(-30.0), 1.0);
        assert_eq!(normalize_snr_db(-10.0), 0.0);
        assert!((normalize_snr_db(-20.0) - 0.5).abs() < 1e-9);
        // Out of range clamps:
        assert_eq!(normalize_snr_db(-40.0), 1.0);
        assert_eq!(normalize_snr_db(0.0), 0.0);
    }

    #[test]
    fn composite_fixtures_only() {
        let weights = default_weights();
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "fixtures".to_string(),
            TierResult {
                pass_rate: Some(1.0),
                ..Default::default()
            },
        );
        let score = compute_composite(&weights, &tiers);
        // Only the fixtures weight (0.15) contributes.
        assert!((score - 0.15).abs() < 1e-9);
    }

    #[test]
    fn composite_all_tiers() {
        let weights = default_weights();
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "fixtures".to_string(),
            TierResult {
                pass_rate: Some(1.0),
                ..Default::default()
            },
        );
        tiers.insert(
            "curated-hard-200".to_string(),
            TierResult {
                decode_rate: Some(0.5),
                ..Default::default()
            },
        );
        tiers.insert(
            "synth-clean".to_string(),
            TierResult {
                snr_at_50pct_recovery_db: Some(-20.0), // → 0.5
                ..Default::default()
            },
        );
        tiers.insert(
            "synth-doppler".to_string(),
            TierResult {
                snr_at_50pct_recovery_db: Some(-15.0), // → 0.25
                ..Default::default()
            },
        );
        let score = compute_composite(&weights, &tiers);
        // 0.50*0.5 + 0.30*0.5 + 0.15*1.0 + 0.05*0.25 = 0.25 + 0.15 + 0.15 + 0.0125 = 0.5625
        assert!((score - 0.5625).abs() < 1e-9);
    }
}
```

- [ ] **Step 2: Re-export from lib.rs**

In `pancetta-research/src/lib.rs`, add:

```rust
pub mod metrics;
```

- [ ] **Step 3: Run the unit tests**

Run: `cargo test -p pancetta-research metrics::tests`

Expected: 3 passed.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/metrics.rs pancetta-research/src/lib.rs
git commit -m "feat(research): composite metric calculation + tests"
```

---

## Phase C — DecoderUnderTest trait

### Task 8: Define the `DecoderUnderTest` trait

**Files:**
- Create: `pancetta-research/src/decoder.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Explore the existing pancetta-ft8 decoder API**

Run: `grep -n "^pub fn\|^pub struct\|^pub enum" pancetta-ft8/src/decoder.rs | head -30`

Expected: find the public decode entry point. Note the exact function name and signature; we will reference it in `Ft8Decoder` next.

Run: `head -80 pancetta-ft8/src/decoder.rs`

Expected: see imports + the decoder struct or function. Confirm what the decode return type is (likely `Vec<DecodedMessage>` or similar).

Record the exact symbol names you saw — they will be referenced in Step 3 below.

- [ ] **Step 2: Write the trait**

Create `pancetta-research/src/decoder.rs`:

```rust
use crate::Mode;
use serde::Serialize;
use std::path::Path;

/// One decoded message from a single WAV. Mode-agnostic.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Decode {
    /// Message string as the decoder produced it (e.g. "CQ K1ABC FN42").
    pub message: String,
    /// Audio frequency offset in Hz.
    pub freq_hz: f64,
    /// Time offset relative to slot start in seconds (DT).
    pub dt_s: f64,
    /// Decoder-reported SNR in dB (sign convention varies by mode; use raw).
    pub snr_db: f64,
    /// True if the CRC checked out. Pancetta returns only CRC-valid decodes
    /// today, so this is `true` for our impl; the field exists for parity
    /// with baseline tools that may report uncertain decodes.
    pub crc_valid: bool,
}

/// Generic interface for any decoder we want to evaluate. Implementors wrap
/// the production decoder, a baseline (jt9/JTDX), or an experimental variant.
pub trait DecoderUnderTest: Send + Sync {
    /// Mode this decoder targets.
    fn mode(&self) -> Mode;
    /// Stable identifier for this decoder (e.g. "pancetta-ft8@HEAD", "jt9").
    fn identity(&self) -> String;
    /// Decode a single WAV file. Errors should be returned as `Err`, not
    /// silently turned into empty decodes — the harness logs them.
    fn decode_wav(&self, path: &Path) -> anyhow::Result<Vec<Decode>>;
    /// Opaque JSON snapshot of effective config — serialized into the
    /// scorecard for reproducibility.
    fn config_snapshot(&self) -> serde_json::Value;
}
```

- [ ] **Step 3: Re-export from lib.rs**

In `pancetta-research/src/lib.rs`, add:

```rust
pub mod decoder;
pub use decoder::{Decode, DecoderUnderTest};
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p pancetta-research`

Expected: compiles cleanly.

- [ ] **Step 5: Commit**

```bash
git add pancetta-research/src/decoder.rs pancetta-research/src/lib.rs
git commit -m "feat(research): DecoderUnderTest trait + Decode struct"
```

---

### Task 9: Implement `Ft8Decoder` wrapping `pancetta-ft8`

**Files:**
- Modify: `pancetta-research/src/decoder.rs`
- Modify: `pancetta-ft8/src/decoder.rs` (add `Serialize` derive to `Ft8Config` if missing)

**Verified pancetta-ft8 public API** (so the engineer doesn't have to guess):

- Decoder struct: `pancetta_ft8::Ft8Decoder` (re-exported from `pancetta_ft8::decoder`)
- Config struct: `pancetta_ft8::Ft8Config`
- Decode method: `Ft8Decoder::decode_window(&mut self, samples: &[f32]) -> Ft8Result<Vec<DecodedMessage>>`
  - takes `&mut self` — we wrap in interior mutability
- Constructor: `Ft8Decoder::new(config: Ft8Config) -> Ft8Result<Self>`
- `DecodedMessage` fields (in `pancetta_ft8::message`, re-exported at crate root):
  - `text: String` (plain-text message — use this for `Decode.message`)
  - `snr_db: f32`
  - `frequency_offset: f64`
  - `time_offset: f64`
- `Ft8Result<T> = Result<T, Ft8Error>`; the error type is re-exported.
- Samples must be `&[f32]` at 12 kHz mono. Fixtures + operator recordings already conform; we assert.

- [ ] **Step 1: Confirm the API shape on your checkout (fast verification, not discovery)**

Run: `grep -n "pub fn decode_window\|pub struct DecodedMessage\|pub struct Ft8Config\|pub fn new(config" pancetta-ft8/src/decoder.rs pancetta-ft8/src/message.rs | head -10`

Expected: see `pub fn decode_window(&mut self, samples: &[f32])`, `pub struct DecodedMessage`, `pub struct Ft8Config`. If these names have changed, adjust the impl in step 2 accordingly.

- [ ] **Step 2: Implement `Ft8Decoder`**

Append to `pancetta-research/src/decoder.rs`:

```rust
use anyhow::Context;
use std::sync::Mutex;

/// Wraps the production pancetta-ft8 decoder for use by the harness.
///
/// Holds an `Ft8Config` (the public config struct) and constructs a fresh
/// `pancetta_ft8::Ft8Decoder` per call to `decode_wav`. The production
/// decoder takes `&mut self` and we want this trait impl to be `Send + Sync`,
/// so we don't keep the decoder around between calls — construction is cheap.
pub struct Ft8Decoder {
    config: pancetta_ft8::Ft8Config,
    /// Used only so `config_snapshot` is stable across calls. Empty by
    /// default; future plans may stash per-experiment overrides here.
    _scratch: Mutex<()>,
}

impl Ft8Decoder {
    /// Build with default pancetta-ft8 config (matches what production uses
    /// on `main`).
    pub fn with_default_config() -> Self {
        Self {
            config: pancetta_ft8::Ft8Config::default(),
            _scratch: Mutex::new(()),
        }
    }
}

impl DecoderUnderTest for Ft8Decoder {
    fn mode(&self) -> Mode {
        Mode::Ft8
    }

    fn identity(&self) -> String {
        format!("pancetta-ft8@{}", env!("CARGO_PKG_VERSION"))
    }

    fn decode_wav(&self, path: &Path) -> anyhow::Result<Vec<Decode>> {
        // Load WAV via hound; pancetta-ft8 expects mono f32 samples at 12 kHz
        // (FT8's canonical decode rate). The fixture and recording WAVs are
        // already at 12 kHz mono; assert and bail if not.
        let mut reader = hound::WavReader::open(path)
            .with_context(|| format!("opening WAV {}", path.display()))?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.channels == 1 && spec.sample_rate == 12000,
            "WAV {} not 12kHz mono (got {} ch, {} Hz)",
            path.display(),
            spec.channels,
            spec.sample_rate,
        );
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / i32::MAX as f32))
                .collect::<Result<Vec<_>, _>>()?,
            hound::SampleFormat::Float => reader
                .samples::<f32>()
                .collect::<Result<Vec<_>, _>>()?,
        };

        // Construct a fresh decoder per WAV. decode_window takes &mut self,
        // and we want the outer trait impl to stay `&self`.
        let mut decoder = pancetta_ft8::Ft8Decoder::new(self.config.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let raw = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window failed for {}: {e}", path.display()))?;
        Ok(raw
            .into_iter()
            .map(|d| Decode {
                message: d.text.clone(),
                freq_hz: d.frequency_offset,
                dt_s: d.time_offset,
                snr_db: d.snr_db as f64,
                crc_valid: true, // pancetta returns CRC-valid only
            })
            .collect())
    }

    fn config_snapshot(&self) -> serde_json::Value {
        // Prefer JSON-serialize; fall back to Debug-print if Ft8Config doesn't
        // (yet) derive Serialize. Step 3 below adds the derive if it's missing.
        match serde_json::to_value(&self.config) {
            Ok(v) => v,
            Err(_) => serde_json::json!({
                "debug_repr": format!("{:?}", self.config),
            }),
        }
    }
}
```

- [ ] **Step 3: Add `Serialize` to `Ft8Config` if missing**

Run: `grep -n "^pub struct Ft8Config\|derive.*Serialize" pancetta-ft8/src/decoder.rs | head -5`

If `Ft8Config` does not derive `Serialize` (likely doesn't today), edit `pancetta-ft8/src/decoder.rs`:

Find the `#[derive(...)]` line above `pub struct Ft8Config {` and add `Serialize` to the derive list (and `Deserialize` for symmetry if not present). Example before:

```rust
#[derive(Clone, Debug)]
pub struct Ft8Config { ... }
```

After:

```rust
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Ft8Config { ... }
```

Confirm `serde = { version = "1", features = ["derive"] }` is in `pancetta-ft8/Cargo.toml` `[dependencies]`:

Run: `grep '^serde' pancetta-ft8/Cargo.toml`

If serde is missing, add it. Otherwise no change needed.

**Important:** if `Ft8Config` has fields whose types don't `Serialize` (rare — would be types from a third-party crate), the build will fail. In that case, add `#[serde(skip)]` on those fields and use Default for round-trip. Plan 1 doesn't need full round-trip fidelity, just JSON output.

Run: `cargo build -p pancetta-ft8` to confirm.

- [ ] **Step 4: Verify the harness builds**

Run: `cargo build -p pancetta-research`

Expected: compiles cleanly. If any `pancetta_ft8::*` symbol you used doesn't exist, the compiler error tells you the real name — fix and re-run.

- [ ] **Step 5: Commit**

```bash
git add pancetta-research/src/decoder.rs pancetta-ft8/src/decoder.rs pancetta-ft8/Cargo.toml
git commit -m "feat(research): Ft8Decoder impl wrapping production decoder"
```

---

### Task 10: Smoke test — Ft8Decoder decodes a known fixture

**Files:**
- Create: `pancetta-research/tests/decoder_smoke.rs`

- [ ] **Step 1: Identify a known-decodable fixture**

Run: `ls pancetta-ft8/tests/fixtures/wav/`

Expected: see `generated/` and `wsjt/` subdirs. The `generated/ft8_cq.wav` file is one we encoded ourselves; pick a `wsjt/*.wav` for which the current decoder is known to succeed.

Run: `grep -rn "\.wav" pancetta-ft8/tests/ | grep "expected\|asserts" | head -5`

Pick a fixture whose existing regression test passes today. Record its path and the expected message text.

- [ ] **Step 2: Write the smoke test**

Create `pancetta-research/tests/decoder_smoke.rs`:

```rust
//! Smoke test: Ft8Decoder over a known fixture decodes at least one message.

use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use std::path::PathBuf;

fn fixture(rel: &str) -> PathBuf {
    let workspace = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(workspace)
        .parent()
        .unwrap()
        .join("pancetta-ft8/tests/fixtures/wav")
        .join(rel)
}

#[test]
fn ft8_decoder_finds_at_least_one_decode_in_generated_cq() {
    let path = fixture("generated/ft8_cq.wav");
    assert!(path.exists(), "fixture missing: {}", path.display());

    let decoder = Ft8Decoder::with_default_config();
    let decodes = decoder.decode_wav(&path).expect("decode should not error");
    assert!(
        !decodes.is_empty(),
        "expected at least one decode in {}, got 0",
        path.display(),
    );
    // The generated CQ fixture should produce a CQ message.
    assert!(
        decodes.iter().any(|d| d.message.contains("CQ")),
        "expected a CQ decode, got: {:?}",
        decodes.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn ft8_decoder_config_snapshot_is_json_object() {
    let decoder = Ft8Decoder::with_default_config();
    let snap = decoder.config_snapshot();
    assert!(
        snap.is_object(),
        "config snapshot should be a JSON object, got: {snap:?}"
    );
}

#[test]
fn ft8_decoder_identity_includes_version() {
    let decoder = Ft8Decoder::with_default_config();
    let id = decoder.identity();
    assert!(id.starts_with("pancetta-ft8@"));
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p pancetta-research --test decoder_smoke -- --nocapture`

Expected: 3 passed. If the CQ-decode assertion fails, the fixture path or expected content is wrong — adjust based on what `pancetta-ft8/tests/` actually validates today. If `decode_samples` errors, the production API name in Task 9 doesn't match reality — fix Task 9, re-run.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/tests/decoder_smoke.rs
git commit -m "test(research): Ft8Decoder smoke test on a known fixture"
```

---

## Phase D — Disk hygiene script

### Task 11: Write `scripts/research-env.sh` with preflight/audit/guard-ci

**Files:**
- Create: `scripts/research-env.sh`

- [ ] **Step 1: Write the script**

Create `scripts/research-env.sh`:

```bash
#!/usr/bin/env bash
# scripts/research-env.sh — local-only disk hygiene + CI-guard for the
# decoder research harness. Spec:
# docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md
#
# Subcommands:
#   --preflight     Check disk caps; run scheduled purges if 80-95 GB; pause at 95+ GB.
#   --audit         Print usage report; no actions.
#   --guard-ci      Scan .github/workflows/ for forbidden references; fail if found.
#   --status        (plan 3) List experiments + artifact disk usage.
#   --cleanup       (plan 3) Interactive purge of expired artifacts.
#   --pin <slug>    (plan 3) Keep artifacts past default retention.
#   --finalize <slug>  (plan 3) Rename branch scorecard to history/.
#
# Per spec, caps are:
#   - research/ in git: 500 MB
#   - ~/.pancetta/research_artifacts/ + research/corpus/synth/wavs/ +
#     training/*/data/ together: 100 GB
#
# Warn at 80 GB. Hard pause at 95 GB.

set -euo pipefail

CMD="${1:-}"
shift || true

REPO_ROOT="$(git rev-parse --show-toplevel)"
ARTIFACTS_DIR="${HOME}/.pancetta/research_artifacts"
SYNTH_WAVS_DIR="${REPO_ROOT}/research/corpus/synth/wavs"
TRAINING_DATA_GLOB="${REPO_ROOT}/training"
RESEARCH_DIR="${REPO_ROOT}/research"

WARN_GB=80
PAUSE_GB=95
REPO_CAP_MB=500

# Bytes of a directory, in GB rounded to one decimal. Returns 0 if dir missing.
dir_gb() {
    local d="$1"
    [ -d "$d" ] || { echo "0.0"; return; }
    local bytes
    bytes=$(du -sb "$d" 2>/dev/null | awk '{print $1}')
    awk -v b="$bytes" 'BEGIN { printf "%.1f", b / (1024*1024*1024) }'
}

# Sum size of training/*/data/ subdirs (one per future training dataset).
training_data_gb() {
    local total=0
    if [ -d "$TRAINING_DATA_GLOB" ]; then
        for d in "$TRAINING_DATA_GLOB"/*/data; do
            [ -d "$d" ] || continue
            local b
            b=$(du -sb "$d" 2>/dev/null | awk '{print $1}')
            total=$((total + b))
        done
    fi
    awk -v b="$total" 'BEGIN { printf "%.1f", b / (1024*1024*1024) }'
}

usage_report() {
    local artifacts synth training total
    artifacts=$(dir_gb "$ARTIFACTS_DIR")
    synth=$(dir_gb "$SYNTH_WAVS_DIR")
    training=$(training_data_gb)
    total=$(awk -v a="$artifacts" -v s="$synth" -v t="$training" \
        'BEGIN { printf "%.1f", a + s + t }')

    echo "== Research disk usage =="
    printf "  artifacts (%s): %s GB\n" "$ARTIFACTS_DIR" "$artifacts"
    printf "  synth wavs (%s): %s GB\n" "$SYNTH_WAVS_DIR" "$synth"
    printf "  training data (%s/*/data): %s GB\n" "$TRAINING_DATA_GLOB" "$training"
    printf "  ---\n"
    printf "  total on-disk: %s GB  (warn at %d, pause at %d)\n" \
        "$total" "$WARN_GB" "$PAUSE_GB"

    # repo cap check
    local repo_mb
    repo_mb=$(du -sm "$RESEARCH_DIR" 2>/dev/null | awk '{print $1}')
    repo_mb=${repo_mb:-0}
    printf "  research/ in git: %s MB  (cap %d MB)\n" "$repo_mb" "$REPO_CAP_MB"

    # Return the total in GB via stdout for callers; readers use awk.
    echo "$total"
}

cmd_audit() {
    usage_report > /dev/null
    usage_report | sed -n '1,$p' | head -n -1   # drop the trailing total echo
    return 0
}

cmd_preflight() {
    local total
    total=$(usage_report | tail -n 1)
    # The function prints the report; the last line is the total we capture.
    # Re-print the report for the user (idempotent — usage_report only reads disk).
    usage_report | head -n -1
    echo

    if awk -v t="$total" -v p="$PAUSE_GB" 'BEGIN { exit !(t >= p) }'; then
        echo "STOP: ${total} GB >= ${PAUSE_GB} GB pause threshold."
        echo "Run 'scripts/research-env.sh --cleanup' (available in plan 3) and retry."
        exit 1
    elif awk -v t="$total" -v w="$WARN_GB" 'BEGIN { exit !(t >= w) }'; then
        echo "WARN: ${total} GB >= ${WARN_GB} GB. Scheduled purges would run here."
        echo "Plan 1 only reports; --cleanup arrives in plan 3."
    else
        echo "OK: ${total} GB on-disk research footprint."
    fi
}

cmd_guard_ci() {
    local hits=0
    if [ -d "${REPO_ROOT}/.github/workflows" ]; then
        local forbidden=(
            "pancetta-research"
            "research-env.sh"
            "research/scorecards"
            "research/experiments"
            "research/baselines"
            "research/corpus"
        )
        for term in "${forbidden[@]}"; do
            if grep -rIn "$term" "${REPO_ROOT}/.github/workflows" >/dev/null 2>&1; then
                echo "FORBIDDEN reference in .github/workflows:"
                grep -rIn "$term" "${REPO_ROOT}/.github/workflows" | head -5
                hits=$((hits + 1))
            fi
        done
    fi
    if [ "$hits" -gt 0 ]; then
        echo
        echo "Research harness is local-only. See pancetta-research/README.md."
        exit 1
    fi
    echo "OK: no research references in .github/workflows"
}

case "$CMD" in
    --preflight) cmd_preflight ;;
    --audit) cmd_audit ;;
    --guard-ci) cmd_guard_ci ;;
    --status|--cleanup|--pin|--finalize)
        echo "Subcommand $CMD lands in plan 3 (iteration loop)."
        exit 0
        ;;
    -h|--help|"")
        sed -n '2,18p' "$0"
        ;;
    *)
        echo "unknown subcommand: $CMD" >&2
        exit 2
        ;;
esac
```

- [ ] **Step 2: Make it executable**

Run: `chmod +x scripts/research-env.sh`

- [ ] **Step 3: Run --audit to verify**

Run: `./scripts/research-env.sh --audit`

Expected: prints a usage report. All directories may be 0 GB or missing at this point — that's fine.

- [ ] **Step 4: Run --preflight to verify**

Run: `./scripts/research-env.sh --preflight`

Expected: prints usage report + "OK: X GB on-disk research footprint."

- [ ] **Step 5: Run --guard-ci**

Run: `./scripts/research-env.sh --guard-ci`

Expected: "OK: no research references in .github/workflows".

- [ ] **Step 6: Commit**

```bash
git add scripts/research-env.sh
git commit -m "feat(research): disk hygiene script (preflight, audit, guard-ci)"
```

---

### Task 12: Wire the CI guard into `scripts/check.sh`

**Files:**
- Modify: `scripts/check.sh`

- [ ] **Step 1: Inspect current check.sh to find the right insertion point**

Run: `grep -n "^# Step\|^echo \"==\|^run_step" scripts/check.sh | head -20`

Expected: see how the script stages its steps. We want to add a "guard-ci" step after fmt and clippy but before tests, so it fails fast.

- [ ] **Step 2: Add the guard-ci step**

Locate the section that runs fmt and clippy. Immediately after them, add (matching the existing run-step style — example assuming the script uses `echo "== <step> =="` banners):

```bash
echo "== research guard-ci =="
./scripts/research-env.sh --guard-ci
```

If `check.sh` has a `--fast` lane that skips expensive steps, the guard-ci step should run in *both* lanes (it's microseconds).

- [ ] **Step 3: Run check.sh --fast to verify the new step**

Run: `./scripts/check.sh --fast`

Expected: see `== research guard-ci ==` followed by `OK: no research references in .github/workflows`, then fmt + clippy. Script exits 0.

- [ ] **Step 4: Commit**

```bash
git add scripts/check.sh
git commit -m "build: check.sh runs research guard-ci step"
```

---

## Phase E — `research/` skeleton + gitignore

### Task 13: Create the `research/` directory skeleton

**Files:**
- Create: `research/README.md`
- Create: `research/hypothesis_bank.md`
- Create: `research/scorecards/.gitkeep`
- Create: `research/scorecards/history/.gitkeep`
- Create: `research/experiments/.gitkeep`
- Create: `research/baselines/ft8/.gitkeep`
- Create: `research/corpus/fixtures/ft8/.gitkeep`
- Create: `research/corpus/curated/ft8/.gitkeep`
- Create: `research/corpus/synth/manifests/.gitkeep`

- [ ] **Step 1: Create directories**

Run:

```bash
mkdir -p research/scorecards/history \
         research/experiments \
         research/baselines/ft8 \
         research/corpus/fixtures/ft8 \
         research/corpus/curated/ft8 \
         research/corpus/synth/manifests
touch research/scorecards/.gitkeep \
      research/scorecards/history/.gitkeep \
      research/experiments/.gitkeep \
      research/baselines/ft8/.gitkeep \
      research/corpus/fixtures/ft8/.gitkeep \
      research/corpus/curated/ft8/.gitkeep \
      research/corpus/synth/manifests/.gitkeep
```

- [ ] **Step 2: Write research/README.md**

Create `research/README.md`:

```markdown
# research/

This directory is the journal + artifacts surface for the decoder research
harness. It is plain markdown + JSON + manifests — no databases, no daemons.

Layout (per
`docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`):

- `hypothesis_bank.md` — ranked list of ideas. Claude reads/writes each
  iteration.
- `experiments/` — one `.md` per experiment branch. Journal entries survive
  forever, even after the branch is deleted.
- `scorecards/main.json` — current main-branch scorecard (the bar to beat).
- `scorecards/history/` — all past scorecards (merged + shelved).
- `scorecards/<branch>.json` — in-progress, on the experiment branch.
- `baselines/<mode>/` — cached jt9/JTDX decodes per WAV (committed; tiny).
- `corpus/fixtures/<mode>/` — references into `pancetta-ft8/tests/fixtures/wav/`
  + ground-truth JSON.
- `corpus/curated/<mode>/` — manifest of hard real-world WAVs (paths +
  hashes pointing into `~/.pancetta/recordings/`).
- `corpus/synth/manifests/` — synth-corpus generator configs (committed).
- `corpus/synth/wavs/` — generated synth WAVs (gitignored).

WAV files live outside the repo. The manifests reference them by absolute
path + SHA-256.
```

- [ ] **Step 3: Write the hypothesis bank header**

Create `research/hypothesis_bank.md`:

```markdown
# Hypothesis Bank

last_updated: 2026-05-18T00:00:00Z
current_focus_mode: ft8
wild_card_ratio_target: 0.20
wild_cards_run: 0
exploitation_run: 0
current_ratio: 0.0

## Active (ranked by score)

(empty — plan 3 includes a `bootstrap-bank` session that fills this in)

## Shelved (kept for reference)

(empty)

## Graduated (merged to main)

(empty)
```

- [ ] **Step 4: Stage and commit**

```bash
git add research/
git commit -m "feat(research): skeleton research/ directory + hypothesis bank header"
```

---

### Task 14: Add gitignore rules

**Files:**
- Modify: `.gitignore`

- [ ] **Step 1: Look at the current gitignore**

Run: `cat .gitignore`

Expected: see existing rules (likely `target/`, `*.swp`, etc.).

- [ ] **Step 2: Append research-related rules**

Append to `.gitignore`:

```gitignore

# Decoder research harness — local-only artifacts
# Spec: docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md

# Synth corpus WAVs are generated; manifests + generator are committed
research/corpus/synth/wavs/

# Generated training data (large .npy etc.)
training/*/data/
training/*/data_test/
!training/*/data/.gitkeep

# Local research artifacts staging (if a script ever materializes weights
# inside the repo by mistake, ignore them)
research/corpus/curated/ft8/staged/
```

- [ ] **Step 3: Verify nothing currently in research/ is being ignored that we intend to track**

Run: `git check-ignore -v research/scorecards/.gitkeep research/hypothesis_bank.md research/README.md`

Expected: each prints "not ignored" (or exit code 1, meaning no ignore rule matches them). They're tracked.

Run: `git check-ignore -v research/corpus/synth/wavs/dummy.wav 2>/dev/null || echo "would be ignored"`

Expected: "would be ignored" (the path isn't real; we just want to confirm the rule pattern matches).

- [ ] **Step 4: Commit**

```bash
git add .gitignore
git commit -m "build: gitignore synth wavs + training data"
```

---

## Phase F — First end-to-end smoke: `eval` against fixtures

### Task 15: Write the first cut of the `eval` binary (fixtures-only)

**Files:**
- Create: `pancetta-research/src/corpus.rs`
- Create: `pancetta-research/src/bin/eval.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Write the fixtures corpus loader**

Create `pancetta-research/src/corpus.rs`:

```rust
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
```

- [ ] **Step 2: Re-export from lib.rs**

In `pancetta-research/src/lib.rs`, add:

```rust
pub mod corpus;
```

- [ ] **Step 3: Write the eval binary**

Create `pancetta-research/src/bin/eval.rs`:

```rust
//! eval — runs a DecoderUnderTest against requested corpus tiers and emits a
//! scorecard. Plan 1 supports the fixtures tier only.

use anyhow::Context;
use chrono::Utc;
use pancetta_research::corpus::load_ft8_fixtures;
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use pancetta_research::metrics::{default_weights, populate_composite};
use pancetta_research::scorecard::{
    BuildInfo, ConfigInfo, GitInfo, HarnessInfo, RegressionFlags, Scorecard, TierResult,
};
use pancetta_research::Mode;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug)]
struct Args {
    tiers: Vec<String>,
    mode: Mode,
    output: PathBuf,
    seed: u64,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut tiers: Option<Vec<String>> = None;
        let mut mode: Option<Mode> = None;
        let mut output: Option<PathBuf> = None;
        let mut seed: u64 = 42;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--tier" | "--tiers" => {
                    tiers = Some(
                        iter.next()
                            .context("--tier needs a value")?
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect(),
                    );
                }
                "--mode" => {
                    mode = Some(iter.next().context("--mode needs a value")?.parse()?);
                }
                "--output" => {
                    output = Some(iter.next().context("--output needs a value")?.into());
                }
                "--seed" => {
                    seed = iter.next().context("--seed needs a value")?.parse()?;
                }
                "-h" | "--help" => {
                    eprintln!("usage: eval --tier <tiers,...> --mode <mode> --output <path> [--seed N]");
                    eprintln!("  plan 1: only --tier fixtures is supported");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            tiers: tiers.context("--tier required")?,
            mode: mode.context("--mode required")?,
            output: output.context("--output required")?,
            seed,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn run_fixtures_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
) -> anyhow::Result<TierResult> {
    let fixtures = load_ft8_fixtures(workspace)?;
    let total = fixtures.len() as u32;
    let mut passed = 0u32;
    let mut failures = Vec::new();
    for f in &fixtures {
        match decoder.decode_wav(&f.wav_path) {
            Ok(decodes) if !decodes.is_empty() => {
                // Plan 1: "pass" means "decoded ≥ 1 message and did not error."
                // Plan 2 will compare against truth.json for exact-message match.
                passed += 1;
            }
            Ok(_) => failures.push(pancetta_research::scorecard::FixtureFailure {
                wav: f.display_name.clone(),
                expected: vec!["any decode".into()],
                got: vec![],
            }),
            Err(e) => failures.push(pancetta_research::scorecard::FixtureFailure {
                wav: f.display_name.clone(),
                expected: vec!["any decode".into()],
                got: vec![format!("error: {e}")],
            }),
        }
    }
    let failed = total - passed;
    let pass_rate = if total == 0 { 0.0 } else { passed as f64 / total as f64 };
    Ok(TierResult {
        wavs_processed: total,
        fixtures_total: Some(total),
        fixtures_passed: Some(passed),
        fixtures_failed: Some(failed),
        failures,
        pass_rate: Some(pass_rate),
        ..Default::default()
    })
}

fn git_info(workspace: &std::path::Path) -> GitInfo {
    let run = |args: &[&str]| -> String {
        std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let sha = run(&["rev-parse", "HEAD"]);
    let merge_base = run(&["merge-base", "main", "HEAD"]);
    let dirty = !run(&["status", "--porcelain"]).is_empty();
    GitInfo {
        branch,
        head_sha: sha,
        main_merge_base: merge_base,
        dirty,
    }
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn main() -> anyhow::Result<()> {
    // Preflight gate. If --preflight refuses, the binary refuses too.
    let preflight = std::process::Command::new("./scripts/research-env.sh")
        .arg("--preflight")
        .current_dir(workspace_root()?)
        .status();
    if let Ok(status) = preflight {
        if !status.success() {
            anyhow::bail!("preflight failed; aborting eval");
        }
    }
    // If the script isn't installed (early bootstrap), don't fail — just warn.

    let args = Args::parse()?;
    let workspace = workspace_root()?;
    let started = Instant::now();
    let decoder: Box<dyn DecoderUnderTest> = match args.mode {
        Mode::Ft8 => Box::new(Ft8Decoder::with_default_config()),
    };

    let mut tiers = BTreeMap::new();
    for tier_name in &args.tiers {
        match tier_name.as_str() {
            "fixtures" => {
                let result = run_fixtures_tier(decoder.as_ref(), &workspace)?;
                tiers.insert("fixtures".to_string(), result);
            }
            other => anyhow::bail!(
                "tier '{other}' not supported in plan 1. Only 'fixtures' is wired today; \
                 curated + synth land in plan 2."
            ),
        }
    }

    let mut card = Scorecard {
        schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
        generated_at: Utc::now(),
        mode: args.mode,
        git: git_info(&workspace),
        build: BuildInfo {
            rustc_version: rustc_version(),
            release: cfg!(not(debug_assertions)),
            features: vec!["research-eval".into()],
        },
        harness: HarnessInfo {
            harness_version: env!("CARGO_PKG_VERSION").to_string(),
            host: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            cores_used: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            elapsed_seconds: 0.0,
        },
        config: ConfigInfo {
            decoder: decoder.config_snapshot(),
            seed: args.seed,
            tiers_run: args.tiers.clone(),
        },
        tiers,
        composite: pancetta_research::scorecard::CompositeInfo {
            weights: default_weights(),
            score: 0.0,
            main_baseline_score: None,
            delta_vs_main: None,
        },
        regressions: RegressionFlags::default(),
        notes: format!("Decoder under test: {}", decoder.identity()),
    };
    populate_composite(&mut card, default_weights());
    card.harness.elapsed_seconds = started.elapsed().as_secs_f64();

    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    card.save(&args.output)?;
    println!(
        "wrote scorecard: {} (composite {:.4}, {} tier(s), {:.1}s)",
        args.output.display(),
        card.composite.score,
        args.tiers.len(),
        card.harness.elapsed_seconds,
    );
    Ok(())
}
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build --release -p pancetta-research --bin eval`

Expected: compiles cleanly.

- [ ] **Step 5: Commit**

```bash
git add pancetta-research/src/corpus.rs pancetta-research/src/bin/eval.rs pancetta-research/src/lib.rs
git commit -m "feat(research): eval binary (fixtures tier; first cut)"
```

---

### Task 16: End-to-end smoke — run eval, get a real scorecard

**Files:**
- Create: `pancetta-research/tests/eval_fixtures.rs`

- [ ] **Step 1: Write the integration test**

Create `pancetta-research/tests/eval_fixtures.rs`:

```rust
//! End-to-end: run the eval binary against the fixtures tier and verify the
//! scorecard file lands on disk with a populated `fixtures` tier.

use pancetta_research::scorecard::Scorecard;
use std::process::Command;

#[test]
fn eval_fixtures_produces_valid_scorecard() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tempfile::NamedTempFile::new().unwrap();

    // Run the eval binary via `cargo run` so it picks up the current build.
    let status = Command::new("cargo")
        .args([
            "run",
            "--release",
            "-q",
            "-p",
            "pancetta-research",
            "--bin",
            "eval",
            "--",
            "--tier",
            "fixtures",
            "--mode",
            "ft8",
            "--output",
        ])
        .arg(tmp.path())
        .current_dir(&workspace)
        .status()
        .expect("failed to spawn eval");
    assert!(status.success(), "eval binary failed");

    let card = Scorecard::load(tmp.path()).expect("scorecard must be loadable");
    assert_eq!(card.schema_version, Scorecard::CURRENT_SCHEMA_VERSION);
    let fixtures = card.tiers.get("fixtures").expect("fixtures tier present");
    assert!(fixtures.wavs_processed > 0, "no fixtures discovered");
    assert!(
        fixtures.pass_rate.unwrap() >= 0.0 && fixtures.pass_rate.unwrap() <= 1.0,
        "pass_rate out of range: {:?}",
        fixtures.pass_rate,
    );
    // We don't assert pass_rate == 1.0 because some fixtures may legitimately
    // not be decodable by the default config; the smoke test only verifies the
    // mechanics work. (Plan 2 will tighten this against truth.json.)
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --release -p pancetta-research --test eval_fixtures -- --nocapture`

Expected: 1 passed. You should also see the eval binary's own stdout: "wrote scorecard: ... (composite X.XXXX, 1 tier(s), Y.Ys)".

- [ ] **Step 3: Optional — manually run eval and inspect**

Run:

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures --mode ft8 --output /tmp/scorecard.json
cat /tmp/scorecard.json | head -40
```

Expected: well-formed JSON with `schema_version: 1`, populated `fixtures` tier, composite score in `[0, 0.15]` range (since fixtures is weight 0.15 and we haven't run other tiers).

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/tests/eval_fixtures.rs
git commit -m "test(research): end-to-end eval --tier fixtures smoke"
```

---

## Phase G — CI guard test + docs

### Task 17: Programmatic CI-guard test

**Files:**
- Create: `pancetta-research/tests/ci_guard.rs`

- [ ] **Step 1: Write the test**

Create `pancetta-research/tests/ci_guard.rs`:

```rust
//! Programmatic guarantee that no GitHub Actions workflow references the
//! research harness. Runs the same grep the bash script does.

use std::path::Path;
use std::process::Command;

#[test]
fn no_workflow_references_research() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let workflows = workspace.join(".github/workflows");
    if !workflows.exists() {
        // No workflows directory; nothing to guard.
        return;
    }
    let status = Command::new(workspace.join("scripts/research-env.sh"))
        .arg("--guard-ci")
        .current_dir(&workspace)
        .status()
        .expect("failed to spawn research-env.sh --guard-ci");
    assert!(
        status.success(),
        "research-env.sh --guard-ci failed; a workflow file references the research harness."
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p pancetta-research --test ci_guard`

Expected: 1 passed.

- [ ] **Step 3: Negative-control verification (manual; do not commit)**

Optionally, temporarily add a string `pancetta-research` to any `.github/workflows/*.yml`, re-run the test, and confirm it *fails*. Then revert.

This step is to verify the guard actually works — it has no commit.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/tests/ci_guard.rs
git commit -m "test(research): ci guard via research-env.sh --guard-ci"
```

---

### Task 18: Update CLAUDE.md to document the new crate

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Locate the Workspace Structure table**

Run: `grep -n "Workspace Structure\|Crate.*Purpose\|pancetta-ft8" CLAUDE.md | head -10`

Expected: find the markdown table listing the 11 workspace crates.

- [ ] **Step 2: Add the new crate row**

In CLAUDE.md, add to the table:

```markdown
| `pancetta-research` | Local-only iteration harness for decoder improvements (scorecards, eval, hypothesis bank). **Excluded from CI; never builds in GitHub Actions.** | Plan 1 of 3 in progress |
```

Bump the table title from "11-crate Cargo workspace" to "12-crate Cargo workspace" if that count appears literally.

- [ ] **Step 3: Add a brief Architecture Highlights entry**

In the Architecture Highlights section of CLAUDE.md, add (in topical order — likely near the decoder discussion):

```markdown
- **Decoder research harness** (`pancetta-research/`, `research/`,
  `scripts/research-env.sh`): a local-only iteration harness for improving
  the decoder. Excluded from `default-members` and CI by construction.
  Spec: `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.
  Plan 1 of 3 (this scaffold) lands first.
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: CLAUDE.md notes pancetta-research crate + harness directory"
```

---

## Phase H — Wrap up

### Task 19: Run full check.sh + push

**Files:** none.

- [ ] **Step 1: Run the full check lane**

Run: `scripts/check.sh`

Expected: all stages green including the new `== research guard-ci ==` step. This may take ~5-10 min with a warm cache.

If the guard-ci step finds anything, fix the workflow file (probably nothing).

- [ ] **Step 2: Verify the research crate stays out of default `cargo test`**

Run: `cargo test 2>&1 | grep -c "Compiling pancetta-research"`

Expected: 0. The default test pass should not build the research crate.

- [ ] **Step 3: Verify the research crate tests pass when invoked explicitly**

Run: `cargo test -p pancetta-research`

Expected: all tests pass (`schema_roundtrip`, `decoder_smoke`, `eval_fixtures`, `ci_guard`, plus the unit tests in `mode` and `metrics`).

- [ ] **Step 4: Update memory**

Add a brief entry to `~/.claude/projects/-Users-thagale-Code-pancetta/memory/` noting:
- The harness scaffold landed (plan 1 of 3 complete).
- Pointers: spec path, plan 1 path, crate path.

Update `MEMORY.md` index with a one-line entry referencing the new memory file.

- [ ] **Step 5: Push**

Run: `git push origin main`

Expected: pre-push hook runs (which is `check.sh` per `feedback_git_https.md` and the repo's pre-push setup), passes, and the commits land on origin.

If the pre-push hook fails on anything, fix it as a NEW commit; do not amend or `--no-verify`.

---

## Self-Review Checklist

Run through this before declaring plan 1 complete:

- [ ] `cargo build` from root does NOT compile pancetta-research.
- [ ] `cargo test` from root does NOT touch pancetta-research tests.
- [ ] `cargo build -p pancetta-research` works.
- [ ] `cargo test -p pancetta-research` passes all 4 integration tests + unit tests.
- [ ] `./scripts/research-env.sh --preflight` runs and reports OK.
- [ ] `./scripts/research-env.sh --guard-ci` exits 0.
- [ ] `scripts/check.sh` exits 0 and includes the guard-ci step.
- [ ] `cargo run --release -p pancetta-research --bin eval -- --tier fixtures --mode ft8 --output /tmp/sc.json` produces a valid scorecard.
- [ ] `research/` directory exists with the skeleton structure.
- [ ] No `.github/workflows/*.yml` mentions `pancetta-research`, `research-env.sh`, or `research/`.
- [ ] `.gitignore` ignores `research/corpus/synth/wavs/` and `training/*/data/`.
- [ ] No file over 50 MB was committed.

---

## What's next

**Plan 2 of 3 — Eval pipeline** will add:

- Curated corpus loader (manifest of 22k WAVs)
- Synth corpus generator (extend `training/neural_osd/generate_data.py` pattern)
- jt9 baseline binary
- Full `eval` with synth, fixtures, and curated tiers
- `compare` binary
- Fixture truth.json so the fixtures tier becomes an exact-message regression test, not just a smoke test

**Plan 3 of 3 — Iteration loop** will add:

- `curate` binary
- `leaderboard` binary
- `research-env.sh` lifecycle subcommands (`--status`, `--cleanup`, `--pin`, `--finalize`)
- JTDX baseline integration (optional add-on)
- Hypothesis-bank bootstrap session — Claude reads decoder source + memory + spec and seeds the initial ~15-25 hypotheses
- First journaled experiment to validate the loop end-to-end

Write each plan after the prior one lands so we incorporate what we learn.
