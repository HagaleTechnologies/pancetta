# Decoder Research Harness — Plan 2 of 3: Eval Pipeline

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn Plan 1's "fixtures smoke" eval into a real benchmark — synth-corpus sensitivity curves with known ground truth, all 13 in-repo fixtures gated against hand-labeled `truth.json`, jt9 (WSJT-X CLI) baseline cached per fixture for apples-to-apples comparison, and a `compare` binary that diffs two scorecards into a focused wins/regressions report. Curated real-corpus tier and the `curate` binary stay in Plan 3.

**Architecture:** The eval binary keeps its single-entry-point shape from Plan 1. Three corpus tiers each get a loader module (synth, fixtures, curated — curated is a stub returning empty for Plan 2). A new `gen-synth` binary produces synth WAVs from a manifest config (encoded message + SNR + impairments → AWGN-mixed audio via pancetta-ft8 modulator). A new `baseline` binary shells out to `jt9` (WSJT-X CLI) over each fixture/synth WAV once and caches results to JSON — the cache is the foundation Plan 3's curated tier will use as ground truth; in Plan 2 it serves as a committed reference snapshot of jt9's behavior. The eval binary reads truth.json for the fixtures tier and the synth manifest for the synth-clean tier, runs the decoder under test, and emits a populated scorecard with real sensitivity numbers per SNR bin.

**Tech Stack:** Rust (existing pancetta-research crate), serde/serde_json, hound for WAV I/O, pancetta-ft8 for encoder + modulator, `rand` + `rand_distr` for AWGN (new dev-dep), and `jt9` external binary from a local WSJT-X install. No Python in this plan (the existing `training/neural_osd/generate_data.py` is for LDPC trajectories, not synth corpus).

**Spec:** `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`

**Prior plan (foundations, merged):** `docs/superpowers/plans/2026-05-18-research-harness-1-foundations.md`

---

## File Map

**Spec correction:**
- Modify: `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md` — Tier 2 description ("82 WAVs" → "13 WAVs" + breakdown).

**Existing pancetta-research files modified:**
- Modify: `pancetta-research/src/corpus.rs` — extend `load_ft8_fixtures` to scan all 4 subdirs (`generated`, `wsjt`, `basicft8`, `jtdx`); add `load_synth_corpus` reading a synth manifest; add stub `load_curated` returning empty.
- Modify: `pancetta-research/src/bin/eval.rs` — wire synth and curated tiers; fixtures tier becomes truth-validated.
- Modify: `pancetta-research/Cargo.toml` — add `rand`, `rand_distr` dev-deps (used by the gen-synth binary and the synth tests, gated behind features so they don't leak into runtime deps).

**New files:**
- Create: `pancetta-research/src/truth.rs` — `FixtureTruth` type + loader for `research/corpus/fixtures/ft8/truth.json`.
- Create: `pancetta-research/src/synth.rs` — synth-manifest types (`SynthManifest`, `SynthEntry`, `SynthChannel`) + serializer/deserializer.
- Create: `pancetta-research/src/bin/gen_synth.rs` — generates synth WAVs from a manifest config; writes both WAVs and the populated manifest.
- Create: `pancetta-research/src/bin/baseline.rs` — runs `jt9` over fixtures + synth WAVs; writes `research/baselines/ft8/<sha>.json` cache.
- Create: `pancetta-research/src/bin/compare.rs` — diffs two scorecards, prints wins/regressions/config-diff.
- Create: `pancetta-research/tests/synth_roundtrip.rs` — generate synth → decode → recovery rate makes sense.
- Create: `pancetta-research/tests/truth_loader.rs` — `FixtureTruth` parses + serializes round-trip.
- Create: `pancetta-research/tests/compare_smoke.rs` — `compare` binary diffs two known scorecards correctly.
- Create: `research/corpus/fixtures/ft8/truth.json` — hand-labeled expected decodes for all 13 fixtures.
- Create: `research/corpus/synth/manifests/clean.config.json` — synth config (input to gen-synth; output overwrites `clean.manifest.json`).
- Create: `pancetta-research/README.md` updates: `gen-synth`, `baseline`, `compare` usage.

**Documentation:**
- Modify: `pancetta-research/README.md` — quick-start with the new binaries.
- Modify: `CLAUDE.md` — under "Known Gaps", note that Plan 2 eval is operational.

---

## Phase A — Spec correction + extended fixtures scan + truth.json

### Task 1: Correct the spec's fixture count

**Files:**
- Modify: `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`

- [ ] **Step 1: Find the offending line**

Run: `grep -n "82 WAVs\|82 fixtures" docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`

Expected: the spec text references "82 WAVs" in the Tier 2 description and possibly the codebase-shape paragraph.

- [ ] **Step 2: Update Tier 2 description**

In the Tier 2 section ("Standard fixtures"), replace the "82 WAVs" claim with the actual inventory:

```markdown
- **Source:** existing `pancetta-ft8/tests/fixtures/wav/` — 13 WAVs across
  four subdirs: `generated/` (3, our encoded test signals), `wsjt/` (3,
  WSJT-X golden), `basicft8/` (5, ft8_lib reference), `jtdx/` (2,
  JTDX-recorded off-air).
- **Ground truth:** hand-labeled in `research/corpus/fixtures/ft8/truth.json`.
  Each fixture entry lists the expected decoded messages; the fixtures
  tier passes iff every expected message appears (or, for off-air WAVs
  where the decoder can't recover full content, the entry uses
  `expect: "any-decode"` to require ≥1 message).
```

- [ ] **Step 3: Update any other "82" mentions**

If `grep -c "82"` finds other "82" near "fixture" or "WAV" context, fix them. If the only mention is in Tier 2 (verified by step 1), skip this step.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md
git commit -m "spec: correct fixture count to actual 13 across four subdirs"
```

---

### Task 2: Extend `load_ft8_fixtures` to scan all four subdirs

**Files:**
- Modify: `pancetta-research/src/corpus.rs`

- [ ] **Step 1: Update the subdir list**

In `load_ft8_fixtures`, change the `for sub in ["generated", "wsjt"]` line to all four subdirs:

```rust
    // All four fixture subdirs: generated/ (our encoded test signals),
    // wsjt/ (WSJT-X golden), basicft8/ (ft8_lib reference), jtdx/
    // (JTDX-recorded off-air). Truth.json holds per-fixture expectations.
    for sub in ["generated", "wsjt", "basicft8", "jtdx"] {
```

Remove the prior "// Plan 1 scans only generated/ and wsjt/ ..." comment block; replace with the comment above.

- [ ] **Step 2: Build + run smoke test**

Run: `cargo test -p pancetta-research --test decoder_smoke -- --nocapture`

Expected: smoke tests still pass. The smoke test only checks `generated/ft8_cq.wav`, which is unaffected.

- [ ] **Step 3: Verify fixture discovery count**

Run a quick check:

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures --mode ft8 --output /tmp/sc.json
grep -E "wavs_processed|fixtures_total" /tmp/sc.json
```

Expected: `"wavs_processed": 13` and `"fixtures_total": 13` (instead of 6 from Plan 1).

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/corpus.rs
git commit -m "feat(research): load_ft8_fixtures scans all 4 fixture subdirs (6 -> 13 WAVs)"
```

---

### Task 3: Discover the actual decoded content per fixture (preparation for truth.json)

**Files:**
- Read-only investigation.

The goal of this task is to run the current decoder over each fixture and capture what it actually decodes. We use that as the *starting* truth (since these fixtures have been in the regression suite and not flagged as broken). For fixtures the decoder can't recover, we mark them with `expect: "any-decode"` (a smoke threshold) or `expect: "skip"` (known-undecodable but kept for sanity).

- [ ] **Step 1: Dump current decodes per fixture**

Run (from the worktree):

```bash
cargo test --release -p pancetta-ft8 --test wav_decode_tests -- --nocapture 2>&1 | \
    grep -E "^[a-z]+/.*\\.wav:|^\\s+\\[" | tee /tmp/fixture_decodes.txt
```

Expected: a list per fixture of "N messages decoded" followed by zero or more "[<snr> dB] <text>" lines. Capture this output for the next task.

- [ ] **Step 2: Categorize each fixture**

Per fixture, assign one of three categories:

- **`exact`**: decoder recovers a specific message (e.g. `generated/ft8_cq.wav` → "CQ TEST <call> <grid>"). Use the actual decoded text from step 1.
- **`any-decode`**: decoder finds ≥1 message but specific content varies (off-air recordings with multiple stations on the band).
- **`skip`**: decoder finds 0 messages today; jt9 baseline (Task 8) may or may not find some. Marking `skip` means "don't penalize for missing, but track if a future change makes us find them."

For the 13 fixtures, expected categorization based on `wav_decode_tests.rs`:

- `generated/*.wav` (3): `exact` — we encoded them, content is known.
- `wsjt/*.wav` (3): `any-decode` (probably 1-3 messages each).
- `basicft8/170923_082000.wav`–`082045.wav` (4): `any-decode` (decoder may find some).
- `basicft8/live_now.wav` (1): `any-decode` or `skip` based on step 1 output.
- `jtdx/*.wav` (2): `any-decode` or `skip` based on step 1 output.

Record categorizations + expected texts in a scratch file (e.g. `/tmp/fixture_truth_draft.json`) for use in Task 4.

- [ ] **Step 3: No commit**

This task is preparation — output feeds into Task 4. Nothing changes in the repo.

---

### Task 4: Author `research/corpus/fixtures/ft8/truth.json`

**Files:**
- Create: `research/corpus/fixtures/ft8/truth.json`

- [ ] **Step 1: Write the JSON file**

Structure (each entry keyed by relative path inside `pancetta-ft8/tests/fixtures/wav/`):

```json
{
  "schema_version": 1,
  "fixtures": {
    "generated/ft8_cq.wav": {
      "category": "exact",
      "expect": ["CQ TEST K1ABC FN42"],
      "notes": "Encoded by pancetta-ft8 generator; content is known."
    },
    "generated/ft8_report.wav": {
      "category": "exact",
      "expect": ["K1ABC W9XYZ -10"],
      "notes": "Generated signal-report fixture."
    },
    "generated/ft8_rr73.wav": {
      "category": "exact",
      "expect": ["K1ABC W9XYZ RR73"],
      "notes": "Generated RR73 fixture."
    },
    "wsjt/170709_135615.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "WSJT-X golden. Specific content varies by decoder run."
    },
    "wsjt/181201_180245.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "WSJT-X golden."
    },
    "wsjt/210703_133430.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "WSJT-X golden."
    },
    "basicft8/170923_082000.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "ft8_lib basicft8 set."
    },
    "basicft8/170923_082015.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "ft8_lib basicft8 set."
    },
    "basicft8/170923_082030.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "ft8_lib basicft8 set."
    },
    "basicft8/170923_082045.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "ft8_lib basicft8 set."
    },
    "basicft8/live_now.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "ft8_lib basicft8 set."
    },
    "jtdx/000000_000001.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "JTDX-recorded off-air."
    },
    "jtdx/190227_155815.wav": {
      "category": "any-decode",
      "expect": ["any-decode"],
      "notes": "JTDX-recorded off-air."
    }
  }
}
```

**Important:** if Task 3's step 1 output showed that any of the `any-decode` fixtures actually returns 0 messages from the current decoder, change that entry's `category` to `skip` and `expect` to `[]`. Don't enforce a regression gate against something the current decoder doesn't satisfy.

Also: the exact-message texts for `generated/*.wav` come from the encoder. If Task 3's output shows the actual decoded text, use *that exact text* (case-sensitive; trim trailing spaces). Don't guess. If the actual decoded text differs from what's in the JSON template above, prefer reality.

- [ ] **Step 2: Verify it parses as JSON**

Run: `python3 -m json.tool research/corpus/fixtures/ft8/truth.json > /dev/null && echo OK`

Expected: "OK" (no parse error).

- [ ] **Step 3: Commit**

```bash
git add research/corpus/fixtures/ft8/truth.json
git commit -m "feat(research): truth.json — hand-labeled expected decodes per fixture"
```

---

### Task 5: `truth.rs` module — loader + types

**Files:**
- Create: `pancetta-research/src/truth.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Write `truth.rs`**

Create `pancetta-research/src/truth.rs`:

```rust
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
```

- [ ] **Step 2: Re-export from lib.rs**

Add to `pancetta-research/src/lib.rs`:

```rust
pub mod truth;
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p pancetta-research`

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/truth.rs pancetta-research/src/lib.rs
git commit -m "feat(research): truth.rs — FixtureTruth loader + types"
```

---

### Task 6: Round-trip test for `truth.rs`

**Files:**
- Create: `pancetta-research/tests/truth_loader.rs`

- [ ] **Step 1: Write the test**

Create `pancetta-research/tests/truth_loader.rs`:

```rust
//! Verify FixtureTruth round-trips through JSON and that the committed
//! `research/corpus/fixtures/ft8/truth.json` parses correctly.

use pancetta_research::truth::{FixtureCategory, FixtureEntry, FixtureTruth};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn truth_round_trips_to_disk() {
    let mut fixtures = BTreeMap::new();
    fixtures.insert(
        "generated/ft8_cq.wav".to_string(),
        FixtureEntry {
            category: FixtureCategory::Exact,
            expect: vec!["CQ TEST K1ABC FN42".to_string()],
            notes: "Test fixture.".to_string(),
        },
    );
    fixtures.insert(
        "wsjt/170709_135615.wav".to_string(),
        FixtureEntry {
            category: FixtureCategory::AnyDecode,
            expect: vec!["any-decode".to_string()],
            notes: "WSJT-X golden.".to_string(),
        },
    );
    let truth = FixtureTruth {
        schema_version: FixtureTruth::CURRENT_SCHEMA_VERSION,
        fixtures,
    };
    let json = serde_json::to_string_pretty(&truth).unwrap();
    let back: FixtureTruth = serde_json::from_str(&json).unwrap();
    assert_eq!(back.fixtures.len(), 2);
    assert_eq!(
        back.get("generated/ft8_cq.wav").unwrap().category,
        FixtureCategory::Exact
    );
}

#[test]
fn committed_truth_json_parses_and_covers_all_fixtures() {
    let path = workspace_root().join("research/corpus/fixtures/ft8/truth.json");
    let truth = FixtureTruth::load(&path).expect("committed truth.json must parse");

    // All 13 fixtures must be present in truth.json.
    let expected_keys = [
        "generated/ft8_cq.wav",
        "generated/ft8_report.wav",
        "generated/ft8_rr73.wav",
        "wsjt/170709_135615.wav",
        "wsjt/181201_180245.wav",
        "wsjt/210703_133430.wav",
        "basicft8/170923_082000.wav",
        "basicft8/170923_082015.wav",
        "basicft8/170923_082030.wav",
        "basicft8/170923_082045.wav",
        "basicft8/live_now.wav",
        "jtdx/000000_000001.wav",
        "jtdx/190227_155815.wav",
    ];
    for key in expected_keys {
        assert!(
            truth.get(key).is_some(),
            "truth.json missing fixture: {key}"
        );
    }
}

#[test]
fn truth_load_rejects_wrong_schema_version() {
    let mut fixtures = BTreeMap::new();
    fixtures.insert(
        "x.wav".to_string(),
        FixtureEntry {
            category: FixtureCategory::Skip,
            expect: vec![],
            notes: String::new(),
        },
    );
    let mut truth = FixtureTruth {
        schema_version: 999,
        fixtures,
    };
    truth.schema_version = 999;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), serde_json::to_string(&truth).unwrap()).unwrap();
    let err = FixtureTruth::load(tmp.path()).unwrap_err();
    assert!(err.to_string().contains("schema_version"));
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p pancetta-research --test truth_loader`

Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add pancetta-research/tests/truth_loader.rs
git commit -m "test(research): FixtureTruth round-trip + committed-file parse"
```

---

## Phase B — Synth corpus generator

### Task 7: Synth manifest types in `synth.rs`

**Files:**
- Create: `pancetta-research/src/synth.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Write `synth.rs`**

Create `pancetta-research/src/synth.rs`:

```rust
//! Synth corpus manifest format. The manifest is the canonical source
//! of truth for synth fixtures: an entry lists the encoded message text,
//! the target SNR (dB), the channel impairments applied, and the WAV
//! path. Regenerating from manifest + seed produces byte-identical WAVs.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SynthChannel {
    /// AWGN only — additive white Gaussian noise at the target SNR.
    Awgn,
    /// AWGN + slow frequency drift (linear, configurable Hz/s).
    AwgnDrift,
    // Future: Watterson channel model (Doppler + multipath fading).
    // Not in Plan 2; leave as enum extension point.
}

/// Top-level synth corpus config — the input to the gen-synth binary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthConfig {
    pub schema_version: u32,
    pub label: String,
    /// Messages to encode. Each will be modulated at every snr_db level
    /// listed, producing `messages.len() * snr_steps.len()` total WAVs.
    pub messages: Vec<String>,
    pub snr_steps_db: Vec<f64>,
    pub channel: SynthChannel,
    /// Deterministic seed; same seed + same config → byte-identical output.
    pub seed: u64,
    /// Output dir relative to workspace root. WAVs land here.
    pub output_dir: PathBuf,
}

/// One generated WAV entry — the unit of synth ground truth.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthEntry {
    pub wav_path: PathBuf,
    pub encoded_message: String,
    pub snr_db: f64,
    pub channel: SynthChannel,
    pub seed_for_this_wav: u64,
}

/// Manifest = config + populated entries. Written after gen-synth runs.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthManifest {
    pub schema_version: u32,
    pub config: SynthConfig,
    pub entries: Vec<SynthEntry>,
}

impl SynthConfig {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}

impl SynthManifest {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn save<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let m: SynthManifest = serde_json::from_str(&s)?;
        anyhow::ensure!(
            m.schema_version == Self::CURRENT_SCHEMA_VERSION,
            "SynthManifest schema_version {} not supported (expected {})",
            m.schema_version,
            Self::CURRENT_SCHEMA_VERSION,
        );
        Ok(m)
    }
}
```

- [ ] **Step 2: Re-export from lib.rs**

Add to `pancetta-research/src/lib.rs`:

```rust
pub mod synth;
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p pancetta-research`

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/synth.rs pancetta-research/src/lib.rs
git commit -m "feat(research): synth.rs — SynthConfig + SynthManifest types"
```

---

### Task 8: `clean.config.json` — first synth config

**Files:**
- Create: `research/corpus/synth/manifests/clean.config.json`

- [ ] **Step 1: Write the config**

Create `research/corpus/synth/manifests/clean.config.json`:

```json
{
  "schema_version": 1,
  "label": "clean",
  "messages": [
    "CQ K1ABC FN42",
    "K1ABC W9XYZ EM48",
    "W9XYZ K1ABC -10",
    "K1ABC W9XYZ R-12",
    "W9XYZ K1ABC RR73",
    "K1ABC W9XYZ 73"
  ],
  "snr_steps_db": [
    -28.0, -26.0, -24.0, -22.0, -20.0, -18.0,
    -16.0, -14.0, -12.0, -10.0
  ],
  "channel": "awgn",
  "seed": 42,
  "output_dir": "research/corpus/synth/wavs/clean"
}
```

This config produces 6 messages × 10 SNR steps = 60 WAVs in `clean/` — small enough to regenerate quickly while spanning the sensitivity range we care about (–28 dB is below current decoder threshold; –10 dB is comfortably above).

- [ ] **Step 2: Verify JSON parses**

Run: `python3 -m json.tool research/corpus/synth/manifests/clean.config.json > /dev/null && echo OK`

Expected: "OK".

- [ ] **Step 3: Commit**

```bash
git add research/corpus/synth/manifests/clean.config.json
git commit -m "feat(research): clean.config.json — first synth corpus config (6 msgs × 10 SNR)"
```

---

### Task 9: `rand` + `rand_distr` deps for synth generation

**Files:**
- Modify: `pancetta-research/Cargo.toml`

- [ ] **Step 1: Check whether `rand` is already in workspace deps**

Run: `grep -E '^rand' Cargo.toml`

Expected: may or may not show `rand` in `[workspace.dependencies]`. If yes, inherit. If no, add inline.

- [ ] **Step 2: Add dependencies**

In `pancetta-research/Cargo.toml`, under `[dependencies]`:

```toml
# Used by gen-synth binary for AWGN noise (Box-Muller via rand_distr).
rand = { workspace = true }
rand_distr = "0.4"
```

If `rand` isn't in `[workspace.dependencies]`, change the line to `rand = "0.8"` (inline). `rand_distr` is always inline because the workspace doesn't currently use it.

- [ ] **Step 3: Verify build**

Run: `cargo build -p pancetta-research`

Expected: pulls rand + rand_distr; builds clean.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/Cargo.toml Cargo.lock
git commit -m "build(research): add rand + rand_distr for synth corpus generation"
```

---

### Task 10: `gen-synth` binary — generate synth WAVs from config

**Files:**
- Create: `pancetta-research/src/bin/gen_synth.rs`

- [ ] **Step 1: Understand the pancetta-ft8 encode + modulate API**

Run:

```bash
grep -n "pub fn encode\|pub fn modulate\|pub fn audio_samples\|pub struct Ft8Message\|pub struct.*Encoder" pancetta-ft8/src/encoder.rs pancetta-ft8/src/modulator.rs pancetta-ft8/src/transmit.rs
```

Expected: locate the public API for "message string → audio samples." Most likely path: `transmit::encode_and_modulate(text, base_freq_hz, sample_rate, [config]) -> Vec<f32>` or similar. Record the exact name.

If the encode+modulate path isn't exposed publicly, you'll need to find what IS public and chain it together (e.g. `Ft8Message::parse(text) -> Ft8Message` + `encoder::encode_message(...) -> tones` + `modulator::modulate(tones, ...) -> samples`).

- [ ] **Step 2: Write the gen-synth binary**

Create `pancetta-research/src/bin/gen_synth.rs`:

```rust
//! gen-synth — generate a synth WAV corpus from a SynthConfig JSON.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin gen-synth -- \
//!     --config research/corpus/synth/manifests/clean.config.json \
//!     --output research/corpus/synth/manifests/clean.manifest.json

use anyhow::Context;
use hound::{SampleFormat, WavSpec, WavWriter};
use pancetta_research::synth::{SynthChannel, SynthConfig, SynthEntry, SynthManifest};
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};
use std::path::{Path, PathBuf};

const SAMPLE_RATE: u32 = 12_000;
const BASE_AUDIO_HZ: f64 = 1500.0;

#[derive(Debug)]
struct Args {
    config: PathBuf,
    output_manifest: PathBuf,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut config: Option<PathBuf> = None;
        let mut output: Option<PathBuf> = None;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--config" => config = Some(iter.next().context("--config needs a value")?.into()),
                "--output" => output = Some(iter.next().context("--output needs a value")?.into()),
                "-h" | "--help" => {
                    eprintln!("usage: gen-synth --config <config.json> --output <manifest.json>");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            config: config.context("--config required")?,
            output_manifest: output.context("--output required")?,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

/// Encode + modulate one FT8 message into 12 kHz mono f32 samples at the
/// canonical 1500 Hz base audio offset. Uses pancetta-ft8's public API.
fn modulate_message(text: &str) -> anyhow::Result<Vec<f32>> {
    // ADJUST: this call must match the actual pancetta-ft8 API located in
    // step 1. The function below is the *intended* shape — engineer fills
    // in with real symbols. Likely candidates:
    //   pancetta_ft8::transmit::encode_and_modulate(text, BASE_AUDIO_HZ, SAMPLE_RATE)
    // or
    //   pancetta_ft8::encoder::encode_and_modulate(text, ...).
    //
    // If the public API exposes encode + modulate as separate steps, chain them.
    let samples = pancetta_ft8::transmit::encode_and_modulate(text, BASE_AUDIO_HZ, SAMPLE_RATE)
        .map_err(|e| anyhow::anyhow!("encode_and_modulate failed for '{text}': {e}"))?;
    Ok(samples)
}

/// Mix AWGN at the target SNR. SNR is measured in dB relative to signal RMS.
fn add_awgn(samples: &mut [f32], snr_db: f64, rng_seed: u64) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(rng_seed);
    // Signal RMS:
    let signal_rms: f64 = (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
        / samples.len() as f64)
        .sqrt();
    // Target noise RMS so that 20*log10(signal_rms / noise_rms) = snr_db:
    let noise_rms = signal_rms / 10f64.powf(snr_db / 20.0);
    let normal = Normal::new(0.0_f64, noise_rms).expect("noise stddev must be finite");
    for s in samples.iter_mut() {
        *s += normal.sample(&mut rng) as f32;
    }
}

fn write_wav(path: &Path, samples: &[f32]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write as 16-bit PCM — matches the rest of the corpus (fixtures + operator
    // recordings are all 16-bit PCM mono 12 kHz).
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(path, spec)?;
    for &s in samples {
        // Clamp to [-1, 1] then scale to i16.
        let clamped = s.clamp(-1.0, 1.0);
        let i = (clamped * 32767.0) as i16;
        w.write_sample(i)?;
    }
    w.finalize()?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let workspace = workspace_root()?;
    let config_path = if args.config.is_absolute() {
        args.config.clone()
    } else {
        workspace.join(&args.config)
    };
    let config_text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    let config: SynthConfig = serde_json::from_str(&config_text)?;
    anyhow::ensure!(
        config.schema_version == SynthConfig::CURRENT_SCHEMA_VERSION,
        "SynthConfig schema_version {} not supported",
        config.schema_version
    );
    anyhow::ensure!(
        config.channel == SynthChannel::Awgn,
        "Plan 2 only supports channel=awgn; got {:?}",
        config.channel
    );

    let output_dir = workspace.join(&config.output_dir);
    let mut entries = Vec::new();
    let mut total = 0usize;
    for msg in &config.messages {
        let base_samples = modulate_message(msg)?;
        for snr_db in &config.snr_steps_db {
            // Per-wav seed is deterministic from (top-level seed, msg index, snr_db).
            let msg_idx = config.messages.iter().position(|m| m == msg).unwrap();
            let seed_for_this_wav = config
                .seed
                .wrapping_add(msg_idx as u64)
                .wrapping_mul(1_000_003)
                .wrapping_add((snr_db.to_bits() as u64).wrapping_mul(7));
            let mut samples = base_samples.clone();
            add_awgn(&mut samples, *snr_db, seed_for_this_wav);
            // Filename: <msg-slug>__<snr>dB.wav (slugify the message text).
            let slug: String = msg
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let filename = format!("{slug}__{:+.1}dB.wav", snr_db);
            let wav_path = output_dir.join(&filename);
            write_wav(&wav_path, &samples)?;
            entries.push(SynthEntry {
                wav_path: PathBuf::from(&config.output_dir).join(&filename),
                encoded_message: msg.clone(),
                snr_db: *snr_db,
                channel: config.channel,
                seed_for_this_wav,
            });
            total += 1;
        }
    }

    let manifest = SynthManifest {
        schema_version: SynthManifest::CURRENT_SCHEMA_VERSION,
        config: config.clone(),
        entries,
    };
    let manifest_path = if args.output_manifest.is_absolute() {
        args.output_manifest.clone()
    } else {
        workspace.join(&args.output_manifest)
    };
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    manifest.save(&manifest_path)?;
    println!(
        "gen-synth: wrote {} WAVs to {}; manifest at {}",
        total,
        output_dir.display(),
        manifest_path.display(),
    );
    Ok(())
}
```

**Important note on the modulate_message function:** the exact pancetta-ft8 API call is a placeholder pending Step 1's API verification. If `pancetta_ft8::transmit::encode_and_modulate` doesn't exist with that signature, replace with what's actually public. The function must produce 12 kHz mono f32 samples of one FT8 message lasting ~12.64 s (the standard FT8 message duration).

If the encode + modulate API isn't exposed as a single function, chain them. If it requires a different config struct, build that struct here.

If no public path exists at all, this is a real blocker — STOP and report. Do NOT silently work around it (e.g. by re-implementing modulation). Instead, expose the needed function on pancetta-ft8 as a separate commit on the same branch, with message `feat(ft8): expose encode_and_modulate for synth corpus`.

- [ ] **Step 2: Build the binary**

Run: `cargo build --release -p pancetta-research --bin gen-synth`

Expected: clean compile. If the modulate_message call is wrong, the compiler error tells you the real name.

- [ ] **Step 3: Run it on the clean config**

Run:

```bash
cargo run --release -p pancetta-research --bin gen-synth -- \
    --config research/corpus/synth/manifests/clean.config.json \
    --output research/corpus/synth/manifests/clean.manifest.json
```

Expected: prints `gen-synth: wrote 60 WAVs to research/corpus/synth/wavs/clean; manifest at research/corpus/synth/manifests/clean.manifest.json`.

Verify WAVs exist: `ls research/corpus/synth/wavs/clean/ | wc -l` should be 60.

- [ ] **Step 4: Verify gitignore behavior**

Run: `git status research/corpus/synth/` — only the manifest under `manifests/` should appear as untracked; the `wavs/` dir is gitignored.

If `wavs/` contents show up as untracked, the `.gitignore` rule `research/corpus/synth/wavs/` isn't catching them — investigate (likely a typo).

- [ ] **Step 5: Commit**

```bash
git add pancetta-research/src/bin/gen_synth.rs research/corpus/synth/manifests/clean.manifest.json
git commit -m "feat(research): gen-synth binary + clean manifest (60 WAVs)"
```

The `clean.manifest.json` IS committed (small JSON), but the WAV files themselves are NOT (gitignored, regeneratable from manifest+seed).

---

### Task 11: Synth corpus loader + round-trip smoke test

**Files:**
- Modify: `pancetta-research/src/corpus.rs`
- Create: `pancetta-research/tests/synth_roundtrip.rs`

- [ ] **Step 1: Add `load_synth_corpus` to corpus.rs**

In `pancetta-research/src/corpus.rs`, append:

```rust
use crate::synth::SynthManifest;

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
```

(The `PathBuf` import may already be present at the top of corpus.rs from Plan 1; if not, add `use std::path::PathBuf;` alongside the existing `use std::path::{Path, PathBuf};`.)

- [ ] **Step 2: Write the round-trip smoke test**

Create `pancetta-research/tests/synth_roundtrip.rs`:

```rust
//! End-to-end: gen-synth produces WAVs the decoder recovers correctly at
//! comfortable SNRs. This is a sensitivity sanity check — confirms the
//! signal-gen → decode pipeline works, not a specific sensitivity claim.

#![cfg(feature = "research-eval")]

use pancetta_research::corpus::load_synth_corpus;
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn synth_corpus_decodes_at_comfortable_snr() {
    let workspace = workspace_root();
    let manifest_path = workspace.join("research/corpus/synth/manifests/clean.manifest.json");

    // If the manifest doesn't exist, regenerate it via gen-synth.
    if !manifest_path.exists() {
        let config = workspace.join("research/corpus/synth/manifests/clean.config.json");
        let status = Command::new("cargo")
            .args([
                "run",
                "--release",
                "-q",
                "-p",
                "pancetta-research",
                "--bin",
                "gen-synth",
                "--",
                "--config",
            ])
            .arg(&config)
            .arg("--output")
            .arg(&manifest_path)
            .current_dir(&workspace)
            .status()
            .expect("gen-synth must run");
        assert!(status.success(), "gen-synth failed");
    }

    let entries = load_synth_corpus(&workspace, &manifest_path)
        .expect("manifest must load");
    assert!(!entries.is_empty(), "manifest should have entries");

    // For each comfortable-SNR entry (>= -14 dB), decoder should recover
    // the exact message. This is a sanity gate; real sensitivity numbers
    // come from the eval binary.
    let decoder = Ft8Decoder::with_default_config();
    let mut comfortable_total = 0;
    let mut comfortable_recovered = 0;
    for entry in &entries {
        if entry.snr_db < -14.0 {
            continue;
        }
        comfortable_total += 1;
        let decodes = decoder
            .decode_wav(&entry.wav_path)
            .expect("decode must not error on synth wav");
        if decodes.iter().any(|d| d.message.contains(&entry.encoded_message)) {
            comfortable_recovered += 1;
        }
    }
    assert!(comfortable_total > 0, "should have some comfortable-SNR entries");
    let rate = comfortable_recovered as f64 / comfortable_total as f64;
    assert!(
        rate >= 0.80,
        "decoder should recover ≥80% of comfortable-SNR synth (≥-14 dB), got {rate:.2} ({comfortable_recovered}/{comfortable_total})"
    );
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test --release -p pancetta-research --features research-eval --test synth_roundtrip -- --nocapture`

Expected: 1 passed. The test regenerates the manifest if missing, decodes all comfortable-SNR entries (≥ –14 dB), expects ≥80% recovery. With 6 messages × 4 SNR steps (–14, –12, –10) = 24 entries, threshold is ~19 recovered.

If recovery drops below 80%, either the synth gen is broken or the decoder default config is significantly worse than expected. Investigate before continuing.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/corpus.rs pancetta-research/tests/synth_roundtrip.rs
git commit -m "feat(research): load_synth_corpus + comfortable-SNR sanity test"
```

---

## Phase C — jt9 baseline binary

### Task 12: `baseline` binary — wrap jt9 CLI, cache decodes

**Files:**
- Create: `pancetta-research/src/bin/baseline.rs`

- [ ] **Step 1: Verify jt9 is installed and locate it**

Run: `which jt9 || which /Applications/wsjtx.app/Contents/MacOS/jt9 || ls -la $(brew --prefix 2>/dev/null)/bin/jt9 2>/dev/null`

Expected: prints a path to the `jt9` binary. WSJT-X on macOS typically installs jt9 at `/Applications/wsjtx.app/Contents/MacOS/jt9`. Record the path.

If `jt9` is NOT installed, abort this task and report — operator needs to install WSJT-X (or jt9 standalone) before Plan 2 can complete the baseline. Don't skip; the baseline is the whole point of "vs WSJT-X" framing.

- [ ] **Step 2: Test jt9 CLI manually**

Run with a known-good WAV:

```bash
JT9=$(which jt9 || echo /Applications/wsjtx.app/Contents/MacOS/jt9)
$JT9 -8 -d 3 pancetta-ft8/tests/fixtures/wav/wsjt/210703_133430.wav
```

Expected: prints decoded messages. Note the output format (typical jt9 output: `HHMMSS  SNR  DT  Freq  ~  <message>`).

- [ ] **Step 3: Write the baseline binary**

Create `pancetta-research/src/bin/baseline.rs`:

```rust
//! baseline — runs jt9 (WSJT-X CLI) over fixture and synth WAVs;
//! caches decodes to JSON for the eval binary to consume.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin baseline -- \
//!     --tier fixtures --mode ft8

use anyhow::Context;
use pancetta_research::corpus::{load_ft8_fixtures, load_synth_corpus};
use pancetta_research::Mode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BaselineDecode {
    pub message: String,
    pub freq_hz: f64,
    pub dt_s: f64,
    pub snr_db: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BaselineCache {
    pub schema_version: u32,
    pub wav_path: String,
    pub wav_sha256: String,
    pub decoder_identity: String,
    pub decodes: Vec<BaselineDecode>,
    pub elapsed_seconds: f64,
}

impl BaselineCache {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}

#[derive(Debug)]
struct Args {
    tier: String,
    mode: Mode,
    jt9_path: PathBuf,
    synth_manifest: Option<PathBuf>,
    force: bool,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut tier: Option<String> = None;
        let mut mode: Option<Mode> = None;
        let mut jt9_path: Option<PathBuf> = None;
        let mut synth_manifest: Option<PathBuf> = None;
        let mut force = false;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--tier" => tier = Some(iter.next().context("--tier needs a value")?),
                "--mode" => {
                    mode = Some(
                        iter.next()
                            .context("--mode needs a value")?
                            .parse::<Mode>()
                            .map_err(|e| anyhow::anyhow!("{e}"))?,
                    );
                }
                "--jt9-path" => jt9_path = Some(iter.next().context("--jt9-path needs a value")?.into()),
                "--synth-manifest" => synth_manifest = Some(iter.next().context("--synth-manifest needs a value")?.into()),
                "--force" => force = true,
                "-h" | "--help" => {
                    eprintln!("usage: baseline --tier <fixtures|synth> --mode ft8 [--jt9-path PATH] [--synth-manifest PATH] [--force]");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        // Default jt9 path: try `which jt9` then macOS WSJT-X install path.
        let jt9_path = jt9_path
            .or_else(|| {
                Command::new("which")
                    .arg("jt9")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| PathBuf::from(s.trim()))
                    .filter(|p| p.exists())
            })
            .unwrap_or_else(|| PathBuf::from("/Applications/wsjtx.app/Contents/MacOS/jt9"));
        anyhow::ensure!(
            jt9_path.exists(),
            "jt9 not found at {}; install WSJT-X or pass --jt9-path",
            jt9_path.display()
        );
        Ok(Self {
            tier: tier.context("--tier required")?,
            mode: mode.context("--mode required")?,
            jt9_path,
            synth_manifest,
            force,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Parse one line of jt9 stdout. Typical format: `120000  5  0.4 1500 ~  CQ K1ABC FN42`.
/// Returns None for non-decode lines.
fn parse_jt9_line(line: &str) -> Option<BaselineDecode> {
    // jt9's output starts with a 6-digit time (HHMMSS) followed by SNR, DT, freq, ~, then message.
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    // Expect: time, snr, dt, freq, "~", message...
    let snr: f64 = parts[1].parse().ok()?;
    let dt: f64 = parts[2].parse().ok()?;
    let freq: f64 = parts[3].parse().ok()?;
    if parts[4] != "~" {
        return None;
    }
    let message = parts[5..].join(" ");
    Some(BaselineDecode {
        message,
        freq_hz: freq,
        dt_s: dt,
        snr_db: snr,
    })
}

fn run_jt9(jt9_path: &Path, wav_path: &Path) -> anyhow::Result<(Vec<BaselineDecode>, f64)> {
    let started = std::time::Instant::now();
    let output = Command::new(jt9_path)
        .args(["-8", "-d", "3"])
        .arg(wav_path)
        .output()
        .with_context(|| format!("running {} on {}", jt9_path.display(), wav_path.display()))?;
    let elapsed = started.elapsed().as_secs_f64();
    if !output.status.success() {
        anyhow::bail!(
            "jt9 failed on {}: {}",
            wav_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let decodes = stdout
        .lines()
        .filter_map(parse_jt9_line)
        .collect();
    Ok((decodes, elapsed))
}

fn cache_path(workspace: &Path, mode: Mode, wav_hash: &str) -> PathBuf {
    workspace
        .join("research/baselines")
        .join(mode.as_str())
        .join(format!("{wav_hash}.json"))
}

fn process_wav(
    workspace: &Path,
    mode: Mode,
    wav_path: &Path,
    jt9_path: &Path,
    force: bool,
) -> anyhow::Result<()> {
    let wav_sha = sha256_file(wav_path)?;
    let out = cache_path(workspace, mode, &wav_sha);
    if out.exists() && !force {
        return Ok(());
    }
    let (decodes, elapsed) = run_jt9(jt9_path, wav_path)?;
    let cache = BaselineCache {
        schema_version: BaselineCache::CURRENT_SCHEMA_VERSION,
        wav_path: wav_path
            .strip_prefix(workspace)
            .unwrap_or(wav_path)
            .to_string_lossy()
            .into_owned(),
        wav_sha256: wav_sha,
        decoder_identity: format!("jt9 ({})", jt9_path.display()),
        decodes,
        elapsed_seconds: elapsed,
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, serde_json::to_string_pretty(&cache)?)?;
    println!(
        "baseline: {} decodes from {} ({:.2}s) -> {}",
        cache.decodes.len(),
        wav_path.strip_prefix(workspace).unwrap_or(wav_path).display(),
        cache.elapsed_seconds,
        out.strip_prefix(workspace).unwrap_or(&out).display(),
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let workspace = workspace_root()?;
    let wavs: Vec<PathBuf> = match args.tier.as_str() {
        "fixtures" => load_ft8_fixtures(&workspace)?
            .into_iter()
            .map(|f| f.wav_path)
            .collect(),
        "synth" => {
            let manifest = args
                .synth_manifest
                .clone()
                .unwrap_or_else(|| workspace.join("research/corpus/synth/manifests/clean.manifest.json"));
            load_synth_corpus(&workspace, &manifest)?
                .into_iter()
                .map(|e| e.wav_path)
                .collect()
        }
        other => anyhow::bail!("unknown tier '{other}'. Use 'fixtures' or 'synth'."),
    };
    println!(
        "baseline: processing {} WAVs (tier={}, mode={})",
        wavs.len(),
        args.tier,
        args.mode
    );
    for wav in &wavs {
        process_wav(&workspace, args.mode, wav, &args.jt9_path, args.force)?;
    }
    println!("baseline: done.");
    Ok(())
}
```

- [ ] **Step 4: Add `sha2` dependency**

In `pancetta-research/Cargo.toml`, add to `[dependencies]`:

```toml
sha2 = "0.10"
```

If `sha2` is in `[workspace.dependencies]`, use `sha2 = { workspace = true }` instead. (Likely is — it's a common dep.)

- [ ] **Step 5: Build the binary**

Run: `cargo build --release -p pancetta-research --bin baseline`

Expected: clean compile.

- [ ] **Step 6: Run it on fixtures**

Run:

```bash
cargo run --release -p pancetta-research --bin baseline -- --tier fixtures --mode ft8
```

Expected:
- prints "baseline: processing 13 WAVs (tier=fixtures, mode=ft8)"
- for each fixture, prints "baseline: N decodes from <path> (X.XXs) -> research/baselines/ft8/<sha>.json"
- creates ~13 JSON files in `research/baselines/ft8/`.

Verify: `ls research/baselines/ft8/*.json | wc -l` ≈ 13.

- [ ] **Step 7: Run it on synth**

Run:

```bash
cargo run --release -p pancetta-research --bin baseline -- --tier synth --mode ft8
```

Expected: processes 60 synth WAVs. Adds ~60 more cache files.

This is slow (60 jt9 invocations × ~1-3s each = 1-3 min). If a single jt9 invocation crashes or hangs, that's a bug worth flagging — capture which WAV and what jt9 printed.

- [ ] **Step 8: Commit**

```bash
git add pancetta-research/src/bin/baseline.rs pancetta-research/Cargo.toml Cargo.lock research/baselines/
git commit -m "feat(research): baseline binary (jt9 wrapper, per-WAV JSON cache)"
```

The baseline JSON files are *committed* — they're tiny (KB per WAV) and let downstream eval runs be deterministic without re-running jt9.

---

## Phase D — Full eval binary

### Task 13: Extend eval to use truth.json for fixtures + handle synth + curated stubs

**Files:**
- Modify: `pancetta-research/src/bin/eval.rs`

- [ ] **Step 1: Update the fixtures tier to use truth.json**

Replace the body of `run_fixtures_tier` in `pancetta-research/src/bin/eval.rs`. The new version loads `FixtureTruth` and uses per-fixture category to score:

```rust
use pancetta_research::truth::{FixtureCategory, FixtureTruth};

fn run_fixtures_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
) -> anyhow::Result<TierResult> {
    let truth_path = workspace.join("research/corpus/fixtures/ft8/truth.json");
    let truth = FixtureTruth::load(&truth_path)?;
    let fixtures = load_ft8_fixtures(workspace)?;
    let total = fixtures.len() as u32;
    let mut passed = 0u32;
    let mut failures = Vec::new();
    for f in &fixtures {
        let entry = truth.get(&f.display_name);
        let decodes_result = decoder.decode_wav(&f.wav_path);
        match (decodes_result, entry) {
            (Ok(decodes), Some(entry)) => match entry.category {
                FixtureCategory::Exact => {
                    let all_present = entry
                        .expect
                        .iter()
                        .all(|expected| decodes.iter().any(|d| d.message.contains(expected)));
                    if all_present {
                        passed += 1;
                    } else {
                        failures.push(pancetta_research::scorecard::FixtureFailure {
                            wav: f.display_name.clone(),
                            expected: entry.expect.clone(),
                            got: decodes.iter().map(|d| d.message.clone()).collect(),
                        });
                    }
                }
                FixtureCategory::AnyDecode => {
                    if !decodes.is_empty() {
                        passed += 1;
                    } else {
                        failures.push(pancetta_research::scorecard::FixtureFailure {
                            wav: f.display_name.clone(),
                            expected: vec!["any-decode".into()],
                            got: vec![],
                        });
                    }
                }
                FixtureCategory::Skip => {
                    // Don't count toward pass/fail; just track.
                    // Decrement implicit "total" to keep pass_rate honest.
                    // But for simplicity, count Skip as a pass (no regression risk
                    // since we explicitly chose not to gate this fixture).
                    passed += 1;
                }
            },
            (Ok(decodes), None) => {
                // Fixture exists on disk but not in truth.json — informational only.
                failures.push(pancetta_research::scorecard::FixtureFailure {
                    wav: f.display_name.clone(),
                    expected: vec![format!(
                        "no truth.json entry for {} — add one before counting as pass/fail",
                        f.display_name
                    )],
                    got: decodes.iter().map(|d| d.message.clone()).collect(),
                });
            }
            (Err(e), _) => failures.push(pancetta_research::scorecard::FixtureFailure {
                wav: f.display_name.clone(),
                expected: entry.map(|e| e.expect.clone()).unwrap_or_default(),
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
```

- [ ] **Step 2: Add a synth tier runner**

Add to `eval.rs`:

```rust
use pancetta_research::corpus::{load_synth_corpus, SynthCorpusEntry};
use pancetta_research::scorecard::SnrBin;
use std::collections::BTreeMap;

fn run_synth_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
    manifest_path: &std::path::Path,
) -> anyhow::Result<TierResult> {
    let entries = load_synth_corpus(workspace, manifest_path)?;
    // Group by snr_db bin.
    let mut bins: BTreeMap<i64, (u32, u32)> = BTreeMap::new(); // key = snr*10 to avoid float keys
    let mut wavs_processed = 0u32;
    for e in &entries {
        wavs_processed += 1;
        let bin_key = (e.snr_db * 10.0).round() as i64;
        let bin = bins.entry(bin_key).or_insert((0, 0));
        bin.0 += 1; // attempts
        match decoder.decode_wav(&e.wav_path) {
            Ok(decodes) => {
                if decodes
                    .iter()
                    .any(|d| d.message.contains(&e.encoded_message))
                {
                    bin.1 += 1; // decoded
                }
            }
            Err(_) => {
                // Decode error — counts as failed attempt.
            }
        }
    }
    let mut by_snr: Vec<SnrBin> = bins
        .iter()
        .map(|(k, (attempts, decoded))| SnrBin {
            snr_db: (*k as f64) / 10.0,
            attempts: *attempts,
            decoded: *decoded,
            fp: 0,
        })
        .collect();
    by_snr.sort_by(|a, b| a.snr_db.partial_cmp(&b.snr_db).unwrap());
    // Find SNR @ 50% and 90% recovery (first bin where decoded/attempts >= threshold).
    let snr_at_50 = first_threshold_db(&by_snr, 0.50);
    let snr_at_90 = first_threshold_db(&by_snr, 0.90);
    Ok(TierResult {
        wavs_processed,
        by_snr_db: by_snr,
        snr_at_50pct_recovery_db: snr_at_50,
        snr_at_90pct_recovery_db: snr_at_90,
        ..Default::default()
    })
}

/// Lowest SNR (in dB) where recovery >= threshold. Bins must be sorted by SNR asc.
fn first_threshold_db(bins: &[SnrBin], threshold: f64) -> Option<f64> {
    for bin in bins {
        if bin.attempts > 0 && (bin.decoded as f64) / (bin.attempts as f64) >= threshold {
            return Some(bin.snr_db);
        }
    }
    None
}
```

- [ ] **Step 3: Wire synth + curated into the main loop**

In `main()`, where the tier loop currently lives, replace the match arms for the synth + curated tiers:

```rust
    for tier_name in &args.tiers {
        match tier_name.as_str() {
            "fixtures" => {
                let result = run_fixtures_tier(decoder.as_ref(), &workspace)?;
                tiers.insert("fixtures".to_string(), result);
            }
            "synth-clean" => {
                let manifest = workspace.join("research/corpus/synth/manifests/clean.manifest.json");
                anyhow::ensure!(
                    manifest.exists(),
                    "synth manifest missing at {}; run `cargo run -p pancetta-research --bin gen-synth -- --config research/corpus/synth/manifests/clean.config.json --output research/corpus/synth/manifests/clean.manifest.json`",
                    manifest.display()
                );
                let result = run_synth_tier(decoder.as_ref(), &workspace, &manifest)?;
                tiers.insert("synth-clean".to_string(), result);
            }
            "curated-hard-200" | "curated-hard-1000" => {
                eprintln!(
                    "warn: tier '{tier_name}' is a stub in plan 2; populated in plan 3. Skipping."
                );
            }
            other => anyhow::bail!("unknown tier '{other}'"),
        }
    }
```

- [ ] **Step 4: Verify build**

Run: `cargo build --release -p pancetta-research --bin eval`

Expected: clean compile.

- [ ] **Step 5: Run eval with both tiers**

Run:

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean --mode ft8 --output /tmp/sc_plan2.json
```

Expected: prints "wrote scorecard: /tmp/sc_plan2.json (composite X.XXXX, 2 tier(s), Y.Ys)". Composite should be well above the Plan 1 baseline of 0.1250 since synth-clean now contributes (30% weight × normalized SNR).

Inspect:

```bash
python3 -c "import json; d=json.load(open('/tmp/sc_plan2.json')); print(json.dumps(d['tiers'], indent=2)[:1500])"
```

Expected: `fixtures` has `wavs_processed: 13`; `synth-clean` has 10 SNR bins and `snr_at_50pct_recovery_db` populated.

- [ ] **Step 6: Commit**

```bash
git add pancetta-research/src/bin/eval.rs
git commit -m "feat(research): eval gains synth + truth-validated fixtures tiers"
```

---

## Phase E — Compare binary

### Task 14: `compare` binary — scorecard diff

**Files:**
- Create: `pancetta-research/src/bin/compare.rs`

- [ ] **Step 1: Write the binary**

Create `pancetta-research/src/bin/compare.rs`:

```rust
//! compare — diff two scorecards into a focused wins/regressions report.

use anyhow::Context;
use pancetta_research::scorecard::Scorecard;
use std::path::PathBuf;

#[derive(Debug)]
struct Args {
    a: PathBuf,
    b: PathBuf,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut args = std::env::args().skip(1);
        let a = args.next().context("usage: compare A.json B.json")?.into();
        let b = args.next().context("usage: compare A.json B.json")?.into();
        Ok(Self { a, b })
    }
}

fn fmt_pct(x: f64) -> String {
    format!("{:.4}", x)
}

fn fmt_snr(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{:+.1} dB", v),
        None => "n/a".to_string(),
    }
}

fn config_diff(a: &serde_json::Value, b: &serde_json::Value) -> Vec<(String, String, String)> {
    let mut diffs = Vec::new();
    diff_recursive("decoder", a, b, &mut diffs);
    diffs
}

fn diff_recursive(
    prefix: &str,
    a: &serde_json::Value,
    b: &serde_json::Value,
    out: &mut Vec<(String, String, String)>,
) {
    match (a, b) {
        (serde_json::Value::Object(am), serde_json::Value::Object(bm)) => {
            let mut keys: Vec<&String> = am.keys().chain(bm.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let next_prefix = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                match (am.get(k), bm.get(k)) {
                    (Some(av), Some(bv)) => diff_recursive(&next_prefix, av, bv, out),
                    (Some(av), None) => {
                        out.push((next_prefix, value_to_string(av), "<unset>".into()))
                    }
                    (None, Some(bv)) => {
                        out.push((next_prefix, "<unset>".into(), value_to_string(bv)))
                    }
                    (None, None) => {}
                }
            }
        }
        (av, bv) if av != bv => {
            out.push((prefix.to_string(), value_to_string(av), value_to_string(bv)));
        }
        _ => {}
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let a = Scorecard::load(&args.a).with_context(|| format!("loading A: {}", args.a.display()))?;
    let b = Scorecard::load(&args.b).with_context(|| format!("loading B: {}", args.b.display()))?;

    println!(
        "A: {} (sha {}, score {})",
        args.a.display(),
        &a.git.head_sha[..8.min(a.git.head_sha.len())],
        fmt_pct(a.composite.score)
    );
    println!(
        "B: {} (sha {}, score {} {}{})",
        args.b.display(),
        &b.git.head_sha[..8.min(b.git.head_sha.len())],
        fmt_pct(b.composite.score),
        if b.composite.score >= a.composite.score { "+" } else { "" },
        fmt_pct(b.composite.score - a.composite.score),
    );
    println!();

    let mut wins: Vec<String> = Vec::new();
    let mut regressions: Vec<String> = Vec::new();

    // Walk each tier present in both.
    let tier_keys: Vec<&String> = a.tiers.keys().chain(b.tiers.keys()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    for tier in tier_keys {
        match (a.tiers.get(tier), b.tiers.get(tier)) {
            (Some(at), Some(bt)) => {
                // SNR @ 50% — lower is better.
                if at.snr_at_50pct_recovery_db != bt.snr_at_50pct_recovery_db {
                    let delta = bt.snr_at_50pct_recovery_db.unwrap_or(0.0) - at.snr_at_50pct_recovery_db.unwrap_or(0.0);
                    let bucket = if delta < 0.0 { &mut wins } else { &mut regressions };
                    bucket.push(format!(
                        "  {tier:<20}  SNR@50%       {} → {}  ({:+.1} dB)",
                        fmt_snr(at.snr_at_50pct_recovery_db),
                        fmt_snr(bt.snr_at_50pct_recovery_db),
                        delta,
                    ));
                }
                // Pass rate — higher is better.
                if at.pass_rate != bt.pass_rate {
                    let delta = bt.pass_rate.unwrap_or(0.0) - at.pass_rate.unwrap_or(0.0);
                    let bucket = if delta > 0.0 { &mut wins } else { &mut regressions };
                    bucket.push(format!(
                        "  {tier:<20}  pass_rate     {:.4} → {:.4}  ({:+.4})",
                        at.pass_rate.unwrap_or(0.0),
                        bt.pass_rate.unwrap_or(0.0),
                        delta,
                    ));
                }
                // Decode rate — higher is better.
                if at.decode_rate != bt.decode_rate {
                    let delta = bt.decode_rate.unwrap_or(0.0) - at.decode_rate.unwrap_or(0.0);
                    let bucket = if delta > 0.0 { &mut wins } else { &mut regressions };
                    bucket.push(format!(
                        "  {tier:<20}  decode_rate   {:.4} → {:.4}  ({:+.4})",
                        at.decode_rate.unwrap_or(0.0),
                        bt.decode_rate.unwrap_or(0.0),
                        delta,
                    ));
                }
            }
            (Some(_), None) => regressions.push(format!("  {tier:<20}  removed in B")),
            (None, Some(_)) => wins.push(format!("  {tier:<20}  added in B")),
            (None, None) => {}
        }
    }

    if !wins.is_empty() {
        println!("WINS:");
        for w in &wins {
            println!("{w}");
        }
        println!();
    }
    if !regressions.is_empty() {
        println!("REGRESSIONS:");
        for r in &regressions {
            println!("{r}");
        }
        println!();
    } else {
        println!("REGRESSIONS:\n  (none)\n");
    }

    let diffs = config_diff(&a.config.decoder, &b.config.decoder);
    if !diffs.is_empty() {
        println!("CONFIG DIFF:");
        for (k, av, bv) in diffs {
            println!("  decoder.{k:<40} {av} → {bv}");
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Build the binary**

Run: `cargo build --release -p pancetta-research --bin compare`

Expected: clean compile.

- [ ] **Step 3: Generate two scorecards to diff**

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean --mode ft8 --output /tmp/sc_a.json
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean --mode ft8 --output /tmp/sc_b.json
cargo run --release -p pancetta-research --bin compare /tmp/sc_a.json /tmp/sc_b.json
```

Expected: A vs B are identical (same code, same seed) → diff prints REGRESSIONS: (none), WINS: empty, CONFIG DIFF: empty. Confirms compare runs end-to-end.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/bin/compare.rs
git commit -m "feat(research): compare binary — scorecard diff with wins/regressions"
```

---

### Task 15: `compare_smoke` test — verify diff math

**Files:**
- Create: `pancetta-research/tests/compare_smoke.rs`

- [ ] **Step 1: Write the test**

Create `pancetta-research/tests/compare_smoke.rs`:

```rust
//! End-to-end: compare binary correctly identifies wins, regressions, and
//! no-change between scorecards constructed by hand.

#![cfg(feature = "research-eval")]

use pancetta_research::scorecard::{
    BuildInfo, CompositeInfo, ConfigInfo, GitInfo, HarnessInfo, RegressionFlags, Scorecard,
    TierResult,
};
use pancetta_research::Mode;
use serde_json::json;
use std::collections::BTreeMap;
use std::process::Command;

fn make_scorecard(score: f64, pass_rate: f64, snr50: f64) -> Scorecard {
    let mut tiers = BTreeMap::new();
    tiers.insert(
        "fixtures".to_string(),
        TierResult {
            wavs_processed: 13,
            fixtures_total: Some(13),
            fixtures_passed: Some(13),
            pass_rate: Some(pass_rate),
            ..Default::default()
        },
    );
    tiers.insert(
        "synth-clean".to_string(),
        TierResult {
            wavs_processed: 60,
            snr_at_50pct_recovery_db: Some(snr50),
            ..Default::default()
        },
    );
    let mut weights = BTreeMap::new();
    weights.insert("fixtures_pass_rate".to_string(), 0.15);
    weights.insert("snr_50pct_synth_clean".to_string(), 0.30);
    Scorecard {
        schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
        generated_at: chrono::Utc::now(),
        mode: Mode::Ft8,
        git: GitInfo {
            branch: "test".into(),
            head_sha: "abc1234".into(),
            main_merge_base: "abc1234".into(),
            dirty: false,
        },
        build: BuildInfo {
            rustc_version: "1.85.0".into(),
            release: true,
            features: vec![],
        },
        harness: HarnessInfo {
            harness_version: "test".into(),
            host: "darwin/arm64".into(),
            cores_used: 1,
            elapsed_seconds: 0.0,
        },
        config: ConfigInfo {
            decoder: json!({"placeholder": "config"}),
            seed: 42,
            tiers_run: vec!["fixtures".into(), "synth-clean".into()],
        },
        tiers,
        composite: CompositeInfo {
            weights,
            score,
            main_baseline_score: None,
            delta_vs_main: None,
        },
        regressions: RegressionFlags::default(),
        notes: String::new(),
    }
}

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn compare_detects_improvement() {
    let a = tempfile::NamedTempFile::new().unwrap();
    let b = tempfile::NamedTempFile::new().unwrap();
    make_scorecard(0.50, 1.0, -20.0).save(a.path()).unwrap();
    make_scorecard(0.55, 1.0, -22.0).save(b.path()).unwrap();

    let output = Command::new("cargo")
        .args([
            "run", "--release", "-q", "-p", "pancetta-research", "--bin", "compare", "--",
        ])
        .arg(a.path())
        .arg(b.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");
    assert!(output.status.success(), "compare should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("WINS:"), "should report wins");
    assert!(stdout.contains("SNR@50%"), "should mention SNR delta");
    assert!(
        stdout.contains("REGRESSIONS:\n  (none)"),
        "no regressions expected; got: {stdout}"
    );
}

#[test]
fn compare_detects_regression() {
    let a = tempfile::NamedTempFile::new().unwrap();
    let b = tempfile::NamedTempFile::new().unwrap();
    make_scorecard(0.55, 1.0, -22.0).save(a.path()).unwrap();
    make_scorecard(0.50, 0.85, -20.0).save(b.path()).unwrap();

    let output = Command::new("cargo")
        .args([
            "run", "--release", "-q", "-p", "pancetta-research", "--bin", "compare", "--",
        ])
        .arg(a.path())
        .arg(b.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("REGRESSIONS:"), "should report regressions");
    assert!(stdout.contains("pass_rate"), "should mention pass_rate delta");
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --release -p pancetta-research --features research-eval --test compare_smoke -- --nocapture`

Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add pancetta-research/tests/compare_smoke.rs
git commit -m "test(research): compare binary detects wins + regressions"
```

---

## Phase F — Wrap up

### Task 16: Update `pancetta-research/README.md`

**Files:**
- Modify: `pancetta-research/README.md`

- [ ] **Step 1: Replace the "Quick start" section**

Find the existing quick-start (`## Quick start` heading) and replace with:

```markdown
## Quick start

```bash
# Build everything
cargo build --release -p pancetta-research

# Generate the synth corpus (60 WAVs: 6 messages × 10 SNR steps)
cargo run --release -p pancetta-research --bin gen-synth -- \
    --config research/corpus/synth/manifests/clean.config.json \
    --output research/corpus/synth/manifests/clean.manifest.json

# Cache jt9 baseline over fixtures + synth (once; tiny JSON per WAV; committed)
cargo run --release -p pancetta-research --bin baseline -- --tier fixtures --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier synth --mode ft8

# Score current decoder against all tiers
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean --mode ft8 --output research/scorecards/main.json

# Diff two scorecards
cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/main.json research/scorecards/experiment-X.json

# Disk hygiene check
./scripts/research-env.sh --preflight
```

WSJT-X must be installed locally for `baseline` to find `jt9`. On macOS,
the default expected path is `/Applications/wsjtx.app/Contents/MacOS/jt9`;
override with `--jt9-path /path/to/jt9` if needed.
```

- [ ] **Step 2: Commit**

```bash
git add pancetta-research/README.md
git commit -m "docs(research): README quick-start covers Plan 2 binaries"
```

---

### Task 17: Generate and commit `research/scorecards/main.json`

**Files:**
- Create: `research/scorecards/main.json`

- [ ] **Step 1: Generate a fresh main scorecard**

Run:

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean --mode ft8 --output research/scorecards/main.json
```

Expected: writes `research/scorecards/main.json`. This becomes the baseline that future experiments diff against.

- [ ] **Step 2: Verify the scorecard is sane**

```bash
python3 -c "
import json
d = json.load(open('research/scorecards/main.json'))
print(f\"composite: {d['composite']['score']:.4f}\")
print(f\"fixtures pass_rate: {d['tiers']['fixtures'].get('pass_rate')}\")
print(f\"synth-clean snr@50%: {d['tiers']['synth-clean'].get('snr_at_50pct_recovery_db')}\")
print(f\"synth-clean snr@90%: {d['tiers']['synth-clean'].get('snr_at_90pct_recovery_db')}\")
"
```

Expected: composite > 0.4, pass_rate >= 0.85, snr_at_50 in the range [-22, -16] dB (a rough sanity bound — adjust based on actual results).

- [ ] **Step 3: Commit**

```bash
git add research/scorecards/main.json
git commit -m "feat(research): commit Plan 2 main scorecard baseline"
```

---

### Task 18: Run full check.sh and push

**Files:** none.

- [ ] **Step 1: Run check.sh**

Run: `./scripts/check.sh > /tmp/check_plan2.log 2>&1; echo exit: $?; tail -5 /tmp/check_plan2.log`

Expected: exit 0.

If anything fails:
- fmt: run `cargo fmt --all` and commit as a separate `style:` commit.
- clippy: investigate and fix; if it's pre-existing, file as a follow-up.
- workspace tests: a real failure — debug before continuing.

- [ ] **Step 2: Run the research-only test lane**

Run: `cargo test --release -p pancetta-research --features research-eval`

Expected: all tests pass including `synth_roundtrip`, `compare_smoke`, `eval_fixtures`.

- [ ] **Step 3: Memory update**

Update `~/.claude/projects/-Users-thagale-Code-pancetta/memory/project_pancetta_status.md` to reflect Plan 2 landing. Add an entry to `MEMORY.md` if a new memory file is created.

- [ ] **Step 4: Push branch**

Run: `git push -u origin <branch-name>` from the worktree.

The controller (not subagent) handles push. Subagent stops at step 3.

---

## Self-Review Checklist

Before declaring Plan 2 complete:

- [ ] `cargo test -p pancetta-research --features research-eval` — all tests pass.
- [ ] `cargo test -p pancetta-research` (no features) — eval_fixtures/synth_roundtrip/compare_smoke skipped; truth_loader/schema_roundtrip/decoder_smoke/ci_guard pass.
- [ ] `./scripts/research-env.sh --preflight` — OK.
- [ ] `./scripts/research-env.sh --guard-ci` — OK.
- [ ] `scripts/check.sh` — exit 0.
- [ ] `research/scorecards/main.json` exists with all 13 fixtures + 10 synth-clean SNR bins.
- [ ] `research/corpus/synth/manifests/clean.manifest.json` exists with 60 entries.
- [ ] `research/baselines/ft8/` has ≥13 JSON files (one per fixture WAV; +60 if synth baseline was cached).
- [ ] `research/corpus/synth/wavs/` is gitignored.
- [ ] No file > 50 MB was committed.
- [ ] CLAUDE.md still accurate (Plan 1 + Plan 2 status both visible).

---

## What's next

**Plan 3 of 3 — Iteration loop** will add:

- `curate` binary (rank operator recordings by "interesting-ness" using cached jt9 baseline output)
- Curated corpus loader + `--tier curated-hard-200` wiring in eval
- `leaderboard` binary
- `research-env.sh` lifecycle subcommands (`--status`, `--cleanup`, `--pin`, `--finalize`)
- Hypothesis-bank bootstrap session (Claude reads source + memory + spec, seeds 15-25 hypotheses)
- First journaled experiment to validate the loop end-to-end
- Optional: JTDX baseline integration (if `jtdx-cli` is installable scriptably; else manual decode export)

Write Plan 3 after Plan 2 lands.
