# Decoder Research Harness — Plan 3 of 3: Iteration Loop

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the loop. Plan 1 gave us the framework; Plan 2 gave us real numbers against fixtures + synth. Plan 3 brings in the operator's 22k real-world WAVs (curated tier), wires the iteration mechanics (leaderboard, experiment lifecycle in `research-env.sh`, hypothesis-bank bootstrap), and validates the whole pipeline with a first journaled experiment. After this, Claude can pick an idea, branch, implement, eval, journal — repeatedly, in normal Claude Code sessions.

**Architecture:** A new `curate` binary scans `~/.pancetta/recordings/` (~22k WAVs), scores each using pancetta-only signals (decode count, estimated noise floor, decoder-vs-bands disagreement), and produces three manifests: Hard-200 (canonical eval), Hard-1000 (broader regression), Wild-50 (random sanity). The `baseline` binary gets a `--manifest` flag to run jt9 over just the curated WAVs (one-time, ~10 min). The `eval` binary's `curated-hard-200` / `curated-hard-1000` tier stubs become real, comparing pancetta decodes against jt9 baseline. A `leaderboard` binary reads `research/scorecards/history/` and prints a ranked table. `research-env.sh` gains `--status`, `--cleanup`, `--pin`, `--finalize` for experiment lifecycle management. The hypothesis bank gets bootstrapped (15-25 entries seeded from decoder source + spec + memory). The first journaled experiment investigates Plan 2's "1 of 6 synth messages plateaus at ~83%" finding — exercises the full journal+disposition flow.

**Tech Stack:** Existing pancetta-research crate (Rust 2021), serde/serde_json, bash for `research-env.sh` extensions, pancetta-ft8 decoder for curation scoring. No new heavy dependencies. Operator's WSJT-X install (jt9) for the curated baseline pass.

**Spec:** `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`

**Prior plans (merged):**
- Plan 1 (foundations): `docs/superpowers/plans/2026-05-18-research-harness-1-foundations.md`
- Plan 2 (eval pipeline): `docs/superpowers/plans/2026-05-20-research-harness-2-eval-pipeline.md`

---

## File Map

**New pancetta-research source files:**
- Create: `pancetta-research/src/curated.rs` — `CuratedManifest`, `CuratedEntry`, `load_curated_corpus` loader.
- Create: `pancetta-research/src/noise.rs` — `estimate_noise_floor_db(samples)` helper for curation scoring.
- Create: `pancetta-research/src/bin/curate.rs` — produces Hard-200/Hard-1000/Wild-50 manifests.
- Create: `pancetta-research/src/bin/leaderboard.rs` — ranks scorecards in `research/scorecards/history/`.

**Modified pancetta-research source files:**
- Modify: `pancetta-research/src/lib.rs` — re-export `curated`, `noise` modules.
- Modify: `pancetta-research/src/bin/eval.rs` — implement `curated-hard-200` / `curated-hard-1000` tiers (currently stubs).
- Modify: `pancetta-research/src/bin/baseline.rs` — accept `--manifest <path>` to run over curated WAVs.
- Modify: `pancetta-research/Cargo.toml` — `[[bin]] name = "curate"` and `[[bin]] name = "leaderboard"` entries.
- Modify: `pancetta-research/src/scorecard.rs` — extend `TierResult` with new curated-specific fields if Plan 2's existing fields don't cover them (most likely they do).

**New / modified scripts:**
- Modify: `scripts/research-env.sh` — add `--status`, `--cleanup`, `--pin`, `--finalize` subcommands.

**Hypothesis bank + experiment journal:**
- Modify: `research/hypothesis_bank.md` — bootstrap with 15-25 entries.
- Create: `research/experiments/2026-05-20-synth-plateau-investigation.md` — first journaled experiment (no-code-change investigation of the 1-of-6 plateau).

**Tests:**
- Create: `pancetta-research/tests/curate_smoke.rs` — `curate` produces 3 valid manifests when run against a tiny sample dir.
- Create: `pancetta-research/tests/leaderboard_smoke.rs` — `leaderboard` ranks two scorecards correctly.
- Create: `pancetta-research/tests/research_env_lifecycle.rs` — `research-env.sh --status` and `--pin` produce expected output.

**Documentation:**
- Modify: `pancetta-research/README.md` — add `curate`, `leaderboard`, lifecycle subcommands to quick-start.
- Modify: `CLAUDE.md` — Plan 3 landed; harness is operational.
- Create: `docs/RUNBOOK.md` section "Decoder research iteration loop" — operator guide for running an experiment.

**Updated artifacts:**
- Create: `research/corpus/curated/ft8/hard_200.manifest.json` (committed; ~70 KB)
- Create: `research/corpus/curated/ft8/hard_1000.manifest.json` (committed; ~340 KB)
- Create: `research/corpus/curated/ft8/wild_50.manifest.json` (committed; ~18 KB)
- Update: `research/baselines/ft8/*.json` (~200-1250 new files for the curated set, depending on overlap)
- Update: `research/scorecards/main.json` (regenerated with all tiers populated)

---

## Phase A — Experiment lifecycle in research-env.sh

### Task 1: `research-env.sh --status` subcommand

**Files:**
- Modify: `scripts/research-env.sh`

- [ ] **Step 1: Locate the current placeholder for `--status`**

Run: `grep -n -- '--status\|--cleanup\|--pin\|--finalize' scripts/research-env.sh`

Expected: see the existing `--status|--cleanup|--pin|--finalize)` case arm that prints `"Subcommand $CMD lands in plan 3 (iteration loop)."` and exits 0. We'll split these four into separate arms now that they're being implemented.

- [ ] **Step 2: Remove the placeholder arm**

Delete the lines:

```bash
    --status|--cleanup|--pin|--finalize)
        echo "Subcommand $CMD lands in plan 3 (iteration loop)."
        exit 0
        ;;
```

- [ ] **Step 3: Implement `cmd_status` function and `--status` case arm**

Add a `cmd_status` function definition near the other `cmd_*` functions (e.g. after `cmd_guard_ci`):

```bash
# List experiments: their state (per-journal frontmatter), branch existence,
# artifact disk usage. Reads research/experiments/*.md + git for branch info.
cmd_status() {
    local exp_dir="${RESEARCH_DIR}/experiments"
    local artifacts_dir="${ARTIFACTS_DIR}/weights"
    local total=0
    local merged=0
    local shelved=0
    local in_progress=0
    local deferred=0

    echo "== Experiments =="
    if [ ! -d "$exp_dir" ]; then
        echo "  (no experiments dir yet)"
        return
    fi
    # Each experiment has a YAML frontmatter block at the top of its .md;
    # we parse just the `state:` line.
    for f in "$exp_dir"/*.md; do
        [ -f "$f" ] || continue
        # Skip placeholder .gitkeep or non-frontmatter files.
        head -1 "$f" | grep -q '^---' || continue
        total=$((total + 1))
        local slug
        slug=$(basename "$f" .md)
        local state
        state=$(awk '/^state:/ { print $2; exit }' "$f")
        local delta
        delta=$(awk '/^delta_vs_main:/ { print $2; exit }' "$f")
        local branch
        branch=$(awk '/^branch:/ { print $2; exit }' "$f")
        local branch_exists="-"
        if [ -n "$branch" ] && git -C "$REPO_ROOT" rev-parse --verify "$branch" >/dev/null 2>&1; then
            branch_exists="✓"
        fi
        local artifact_size="-"
        if [ -d "$artifacts_dir/$slug" ]; then
            artifact_size=$(du -sh "$artifacts_dir/$slug" 2>/dev/null | awk '{print $1}')
        fi
        printf "  [%s] %-40s  delta=%-10s branch=%s artifacts=%s\n" \
            "${state:-?}" "$slug" "${delta:-?}" "$branch_exists" "$artifact_size"
        case "$state" in
            merged) merged=$((merged + 1)) ;;
            shelved) shelved=$((shelved + 1)) ;;
            evaluated|implementing|planned) in_progress=$((in_progress + 1)) ;;
            deferred) deferred=$((deferred + 1)) ;;
        esac
    done
    if [ "$total" -eq 0 ]; then
        echo "  (no experiments yet — bootstrap the hypothesis bank to start)"
    else
        echo
        printf "  Total: %d   merged: %d   shelved: %d   in-progress: %d   deferred: %d\n" \
            "$total" "$merged" "$shelved" "$in_progress" "$deferred"
    fi
}
```

In the `case "$CMD"` block (where `--preflight`, `--audit`, `--guard-ci` are dispatched), add the new arm:

```bash
    --status) cmd_status ;;
```

Place it after `--guard-ci`, before the help arm.

- [ ] **Step 4: Test `--status` with no experiments**

Run: `./scripts/research-env.sh --status`

Expected:

```
== Experiments ==
  (no experiments yet — bootstrap the hypothesis bank to start)
```

(The `research/experiments/.gitkeep` is filtered out by the `head -1 "$f" | grep -q '^---'` check.)

- [ ] **Step 5: Commit**

```bash
git add scripts/research-env.sh
git commit -m "feat(research): research-env.sh --status lists experiments"
```

---

### Task 2: `research-env.sh --pin <slug>` subcommand

**Files:**
- Modify: `scripts/research-env.sh`

- [ ] **Step 1: Implement `cmd_pin` function**

Add to `scripts/research-env.sh` near the other `cmd_*` functions:

```bash
# Mark an experiment's on-disk artifacts as "pinned" — exempt from default
# retention purges. State recorded in a sidecar file under the artifact dir.
# Usage: research-env.sh --pin <slug>
cmd_pin() {
    local slug="$1"
    if [ -z "$slug" ]; then
        echo "usage: research-env.sh --pin <slug>" >&2
        exit 2
    fi
    local artifact_root="${ARTIFACTS_DIR}/weights/${slug}"
    if [ ! -d "$artifact_root" ]; then
        # Still allow pinning to "reserve" the slot for future artifacts.
        mkdir -p "$artifact_root"
        echo "(no artifacts yet for $slug; created reservation)"
    fi
    echo "pinned: $(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$artifact_root/.pinned"
    echo "Pinned $slug. Artifacts at $artifact_root will not be auto-purged."
}
```

- [ ] **Step 2: Add `--pin` case arm**

In the `case "$CMD"` block, add:

```bash
    --pin) cmd_pin "$1" ;;
```

(Note: `cmd_pin` takes `"$1"` — by the time the case statement runs, `shift` already consumed the subcommand flag itself; `$1` is now the next positional argument. Verify by reading the script's arg handling near the top.)

- [ ] **Step 3: Test --pin**

```bash
./scripts/research-env.sh --pin test-slug
ls -la ~/.pancetta/research_artifacts/weights/test-slug/
```

Expected: directory created, `.pinned` file with timestamp. Cleanup: `rm -rf ~/.pancetta/research_artifacts/weights/test-slug/`.

- [ ] **Step 4: Commit**

```bash
git add scripts/research-env.sh
git commit -m "feat(research): research-env.sh --pin <slug> reserves artifact dir from purge"
```

---

### Task 3: `research-env.sh --cleanup` subcommand

**Files:**
- Modify: `scripts/research-env.sh`

- [ ] **Step 1: Implement `cmd_cleanup` function**

Add to `scripts/research-env.sh`:

```bash
# Purge expired artifacts: experiments shelved >14 days, merged-but-not-promoted
# >30 days. Pinned dirs (.pinned file present) are always preserved.
#
# This is a dry-run by default. Pass --execute to actually delete.
cmd_cleanup() {
    local execute=0
    if [ "${1:-}" = "--execute" ]; then
        execute=1
    fi
    local artifact_root="${ARTIFACTS_DIR}/weights"
    if [ ! -d "$artifact_root" ]; then
        echo "no artifacts dir; nothing to clean"
        return
    fi
    local now_epoch
    now_epoch=$(date -u +%s)
    local total_freed_kb=0
    local count=0

    for d in "$artifact_root"/*; do
        [ -d "$d" ] || continue
        local slug
        slug=$(basename "$d")
        # Skip pinned.
        if [ -f "$d/.pinned" ]; then
            continue
        fi
        # Look up the corresponding journal.
        local journal=""
        for cand in "${RESEARCH_DIR}/experiments/"*"${slug}.md"; do
            [ -f "$cand" ] || continue
            journal="$cand"
            break
        done
        if [ -z "$journal" ]; then
            # Orphan artifact — no journal. Default to keeping (operator may
            # be mid-experiment with no journal yet).
            continue
        fi
        local state
        state=$(awk '/^state:/ { print $2; exit }' "$journal")
        local last_updated
        last_updated=$(awk '/^last_updated:/ { print $2; exit }' "$journal")
        # Convert ISO timestamp to epoch. macOS BSD date uses -j -f; GNU uses -d.
        local last_epoch
        if last_epoch=$(date -j -f "%Y-%m-%dT%H:%M:%SZ" "$last_updated" "+%s" 2>/dev/null); then
            :
        elif last_epoch=$(date -d "$last_updated" "+%s" 2>/dev/null); then
            :
        else
            # Couldn't parse — skip.
            continue
        fi
        local age_days
        age_days=$(( (now_epoch - last_epoch) / 86400 ))

        local retention_days
        case "$state" in
            shelved) retention_days=14 ;;
            merged) retention_days=30 ;;
            *) continue ;;  # deferred / in-progress / planned: don't auto-purge
        esac

        if [ "$age_days" -ge "$retention_days" ]; then
            local size_kb
            size_kb=$(du -sk "$d" 2>/dev/null | awk '{print $1}')
            size_kb=${size_kb:-0}
            count=$((count + 1))
            total_freed_kb=$((total_freed_kb + size_kb))
            if [ "$execute" -eq 1 ]; then
                rm -rf "$d"
                printf "PURGED: %-50s (state=%s, age=%dd, %d KB)\n" \
                    "$slug" "$state" "$age_days" "$size_kb"
            else
                printf "WOULD PURGE: %-50s (state=%s, age=%dd, %d KB)\n" \
                    "$slug" "$state" "$age_days" "$size_kb"
            fi
        fi
    done

    local total_mb
    total_mb=$(awk -v kb="$total_freed_kb" 'BEGIN { printf "%.1f", kb / 1024 }')
    if [ "$execute" -eq 1 ]; then
        echo
        echo "Cleanup: purged $count artifact dir(s), freed ${total_mb} MB."
    else
        echo
        echo "Cleanup (dry-run): would purge $count artifact dir(s), freeing ${total_mb} MB."
        echo "Re-run with --execute to actually delete."
    fi
}
```

- [ ] **Step 2: Add `--cleanup` case arm**

```bash
    --cleanup) cmd_cleanup "$1" ;;
```

- [ ] **Step 3: Test --cleanup**

```bash
./scripts/research-env.sh --cleanup
```

Expected: with no experiments yet, prints "Cleanup (dry-run): would purge 0 artifact dir(s), freeing 0.0 MB. Re-run with --execute to actually delete."

- [ ] **Step 4: Commit**

```bash
git add scripts/research-env.sh
git commit -m "feat(research): research-env.sh --cleanup purges expired artifacts (dry-run by default)"
```

---

### Task 4: `research-env.sh --finalize <slug>` subcommand

**Files:**
- Modify: `scripts/research-env.sh`

- [ ] **Step 1: Implement `cmd_finalize` function**

Add to `scripts/research-env.sh`:

```bash
# Finalize an experiment: rename its branch scorecard to history/<date>-<slug>.json,
# update the experiment journal's `last_updated`, and (if the experiment is merged
# or shelved) mark its branch eligible for cleanup. This is called as part of the
# merge/shelve workflow.
#
# Usage: research-env.sh --finalize <slug>
cmd_finalize() {
    local slug="$1"
    if [ -z "$slug" ]; then
        echo "usage: research-env.sh --finalize <slug>" >&2
        exit 2
    fi
    local journal=""
    for cand in "${RESEARCH_DIR}/experiments/"*"${slug}.md"; do
        [ -f "$cand" ] || continue
        journal="$cand"
        break
    done
    if [ -z "$journal" ]; then
        echo "no journal found for slug '$slug' (expected research/experiments/*-${slug}.md)" >&2
        exit 2
    fi
    local state
    state=$(awk '/^state:/ { print $2; exit }' "$journal")
    local branch
    branch=$(awk '/^branch:/ { print $2; exit }' "$journal")

    # Date prefix: take the slug's leading YYYY-MM-DD if present, else use today.
    local date_prefix
    if [[ "$slug" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2} ]]; then
        date_prefix="${BASH_REMATCH[0]}"
    else
        date_prefix=$(date -u +%Y-%m-%d)
    fi
    # The journal might already have the date prefix in the slug (preferred).
    # Strip it for the scorecard target name; if the journal slug = "synth-plateau",
    # the scorecard target is "history/2026-05-20-synth-plateau.json".
    local target_slug
    target_slug="${slug#${date_prefix}-}"

    # Move branch scorecard to history/, if it exists.
    local branch_scorecard="${RESEARCH_DIR}/scorecards/${branch##*/}.json"
    local history_scorecard="${RESEARCH_DIR}/scorecards/history/${date_prefix}-${target_slug}.json"
    if [ -f "$branch_scorecard" ]; then
        mkdir -p "${RESEARCH_DIR}/scorecards/history"
        mv "$branch_scorecard" "$history_scorecard"
        echo "moved: $branch_scorecard -> $history_scorecard"
    else
        echo "(no branch scorecard at $branch_scorecard — nothing to move)"
    fi

    # Update the journal's last_updated timestamp.
    local now_iso
    now_iso=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    # Use a portable sed in-place edit. On macOS, sed -i wants an empty backup arg.
    if sed --version >/dev/null 2>&1; then
        # GNU sed.
        sed -i "s/^last_updated:.*/last_updated: ${now_iso}/" "$journal"
    else
        # BSD sed (macOS).
        sed -i "" "s/^last_updated:.*/last_updated: ${now_iso}/" "$journal"
    fi

    echo "finalized $slug (state=$state, scorecard=$history_scorecard)"
}
```

- [ ] **Step 2: Add `--finalize` case arm**

```bash
    --finalize) cmd_finalize "$1" ;;
```

- [ ] **Step 3: Smoke-test `--finalize` shape**

Without a real experiment to finalize, we can't fully exercise this — the integration test in Task 14 covers it. For now:

```bash
./scripts/research-env.sh --finalize  # expect error: missing slug
./scripts/research-env.sh --finalize nonexistent-slug  # expect error: no journal found
```

Both should exit 2 with clear messages.

- [ ] **Step 4: Commit**

```bash
git add scripts/research-env.sh
git commit -m "feat(research): research-env.sh --finalize moves branch scorecard to history/"
```

---

### Task 5: Integration test for lifecycle subcommands

**Files:**
- Create: `pancetta-research/tests/research_env_lifecycle.rs`

- [ ] **Step 1: Write the test**

Create `pancetta-research/tests/research_env_lifecycle.rs`:

```rust
//! Integration smoke for research-env.sh lifecycle subcommands.
//! Gated on research-eval feature since it shells out.

#![cfg(feature = "research-eval")]

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run_env(args: &[&str]) -> std::process::Output {
    Command::new(workspace_root().join("scripts/research-env.sh"))
        .args(args)
        .current_dir(workspace_root())
        .output()
        .expect("research-env.sh must run")
}

#[test]
fn status_prints_empty_when_no_experiments() {
    let out = run_env(&["--status"]);
    assert!(out.status.success(), "--status should exit 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("== Experiments =="),
        "expected status banner; got: {s}"
    );
    // Either "(no experiments yet — bootstrap..." OR a real experiment listing.
    // Both are valid — the test runs against whatever's on disk.
}

#[test]
fn pin_requires_slug() {
    let out = run_env(&["--pin"]);
    assert!(!out.status.success(), "--pin without slug should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("usage:"), "expected usage message; got: {err}");
}

#[test]
fn finalize_requires_slug() {
    let out = run_env(&["--finalize"]);
    assert!(!out.status.success(), "--finalize without slug should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("usage:"), "expected usage message; got: {err}");
}

#[test]
fn cleanup_dry_run_by_default() {
    let out = run_env(&["--cleanup"]);
    assert!(out.status.success(), "--cleanup should exit 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("dry-run") || s.contains("Cleanup"),
        "expected dry-run output; got: {s}"
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --release -p pancetta-research --features research-eval --test research_env_lifecycle -- --nocapture`

Expected: 4 passed.

- [ ] **Step 3: Commit**

```bash
git add pancetta-research/tests/research_env_lifecycle.rs
git commit -m "test(research): research-env.sh lifecycle subcommands smoke"
```

---

## Phase B — Curate binary

### Task 6: `noise.rs` helper — estimate noise floor

**Files:**
- Create: `pancetta-research/src/noise.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Write `noise.rs`**

Create `pancetta-research/src/noise.rs`:

```rust
//! Cheap noise-floor estimator used by the curate binary.
//!
//! For an FT8 WAV at 12 kHz mono, "noise floor" is approximated as the
//! median absolute amplitude of the lower 25th percentile of samples.
//! This catches busy bands (high noise floor from many overlapping signals)
//! without needing a full FFT-based spectral estimate.

/// Returns an estimated noise floor in dB (relative to full-scale ±1.0).
/// Higher = noisier. Typical clean-band: -30 dB; busy-band: -20 to -15 dB.
pub fn estimate_noise_floor_db(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return -100.0;
    }
    let mut abs: Vec<f32> = samples.iter().map(|s| s.abs()).collect();
    // Median of the lower 25% of |samples|.
    let q1_count = (abs.len() / 4).max(1);
    abs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lower_quartile = &abs[..q1_count];
    let median = lower_quartile[lower_quartile.len() / 2] as f64;
    if median <= 0.0 {
        return -100.0;
    }
    20.0 * median.log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_has_low_noise_floor() {
        let samples = vec![0.0_f32; 1000];
        assert!(estimate_noise_floor_db(&samples) <= -50.0);
    }

    #[test]
    fn full_scale_signal_has_high_noise_floor() {
        let samples: Vec<f32> = (0..1000).map(|_| 0.5).collect();
        let floor = estimate_noise_floor_db(&samples);
        assert!(floor > -10.0, "got {floor}");
    }

    #[test]
    fn empty_samples_returns_sentinel() {
        assert_eq!(estimate_noise_floor_db(&[]), -100.0);
    }
}
```

- [ ] **Step 2: Re-export from `lib.rs`**

Add to `pancetta-research/src/lib.rs`:

```rust
pub mod noise;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p pancetta-research noise::tests`

Expected: 3 passed.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/noise.rs pancetta-research/src/lib.rs
git commit -m "feat(research): noise.rs — cheap noise-floor estimator for curation"
```

---

### Task 7: `curated.rs` — manifest types + loader

**Files:**
- Create: `pancetta-research/src/curated.rs`
- Modify: `pancetta-research/src/lib.rs`

- [ ] **Step 1: Write `curated.rs`**

Create `pancetta-research/src/curated.rs`:

```rust
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
```

- [ ] **Step 2: Re-export from `lib.rs`**

Add to `pancetta-research/src/lib.rs`:

```rust
pub mod curated;
```

- [ ] **Step 3: Build**

Run: `cargo build -p pancetta-research`

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/curated.rs pancetta-research/src/lib.rs
git commit -m "feat(research): curated.rs — CuratedManifest types + loader"
```

---

### Task 8: `curate` binary — produce Hard-200/Hard-1000/Wild-50 manifests

**Files:**
- Create: `pancetta-research/src/bin/curate.rs`
- Modify: `pancetta-research/Cargo.toml`

- [ ] **Step 1: Add `[[bin]] name = "curate"`**

In `pancetta-research/Cargo.toml`, add an entry next to the existing `[[bin]]` entries:

```toml
[[bin]]
name = "curate"
path = "src/bin/curate.rs"
```

- [ ] **Step 2: Write `curate.rs`**

Create `pancetta-research/src/bin/curate.rs`:

```rust
//! curate — score operator recording WAVs and produce three manifests:
//!   - hard_200.manifest.json (top 200 by interest score)
//!   - hard_1000.manifest.json (top 1000)
//!   - wild_50.manifest.json (50 random from full corpus)
//!
//! Scoring uses pancetta-only signals (decode count, noise floor) — no jt9
//! call. The baseline binary runs jt9 over the curated set as a separate
//! step.
//!
//! Usage:
//!   cargo run --release -p pancetta-research --bin curate -- \
//!     --source-dir ~/.pancetta/recordings \
//!     --output-prefix research/corpus/curated/ft8

use anyhow::Context;
use chrono::Utc;
use pancetta_research::curated::{CuratedEntry, CuratedManifest, ScoreBreakdown};
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use pancetta_research::noise::estimate_noise_floor_db;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const SCORE_W_DECODE_COUNT: f64 = 1.0;
const SCORE_W_NOISE_FLOOR: f64 = 0.05; // dB scaled: -20 dB → +1.0 boost
const SCORE_W_SNR_DIVERSITY: f64 = 0.5;

#[derive(Debug)]
struct Args {
    source_dir: PathBuf,
    output_prefix: PathBuf,
    sample_size: Option<usize>, // limit for fast iteration; None = full corpus
    seed: u64,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut source_dir: Option<PathBuf> = None;
        let mut output_prefix: Option<PathBuf> = None;
        let mut sample_size: Option<usize> = None;
        let mut seed: u64 = 42;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--source-dir" => {
                    source_dir = Some(iter.next().context("--source-dir needs a value")?.into())
                }
                "--output-prefix" => {
                    output_prefix =
                        Some(iter.next().context("--output-prefix needs a value")?.into())
                }
                "--sample-size" => {
                    sample_size = Some(iter.next().context("--sample-size needs a value")?.parse()?)
                }
                "--seed" => seed = iter.next().context("--seed needs a value")?.parse()?,
                "-h" | "--help" => {
                    eprintln!("usage: curate --source-dir <dir> --output-prefix <dir> [--sample-size N] [--seed N]");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            source_dir: source_dir.context("--source-dir required")?,
            output_prefix: output_prefix.context("--output-prefix required")?,
            sample_size,
            seed,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn discover_wavs(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |ext| ext == "wav") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

fn read_wav_samples(path: &Path) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "WAV not 12kHz mono: {}",
        path.display()
    );
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
    };
    Ok(samples)
}

fn score_wav(
    decoder: &dyn DecoderUnderTest,
    path: &Path,
) -> anyhow::Result<(f64, ScoreBreakdown, String)> {
    let samples = read_wav_samples(path)?;
    let noise = estimate_noise_floor_db(&samples);
    let sha = sha256_file(path)?;
    let decodes = decoder.decode_wav(path).unwrap_or_default();
    let decode_count = decodes.len() as u32;
    let mean_snr = if decodes.is_empty() {
        None
    } else {
        let sum: f64 = decodes.iter().map(|d| d.snr_db).sum();
        Some(sum / decodes.len() as f64)
    };
    // SNR-diversity proxy: lower mean SNR (more weak decodes) = more interesting.
    let snr_score = mean_snr.map_or(0.0, |m| (-m / 20.0).max(0.0));
    let score = SCORE_W_DECODE_COUNT * (decode_count as f64)
        + SCORE_W_NOISE_FLOOR * (-noise) // higher noise = bigger negative number = bigger boost
        + SCORE_W_SNR_DIVERSITY * snr_score;
    Ok((
        score,
        ScoreBreakdown {
            pancetta_decode_count: decode_count,
            noise_floor_db: noise,
            mean_decoded_snr_db: mean_snr,
        },
        sha,
    ))
}

fn write_manifest(
    label: &str,
    entries: Vec<CuratedEntry>,
    scoring_decoder: &str,
    output_path: &Path,
) -> anyhow::Result<()> {
    let manifest = CuratedManifest {
        schema_version: CuratedManifest::CURRENT_SCHEMA_VERSION,
        label: label.to_string(),
        generated_at: Utc::now().to_rfc3339(),
        scoring_decoder: scoring_decoder.to_string(),
        entries,
    };
    manifest.save(output_path)?;
    println!(
        "wrote {} entries to {}",
        manifest.entries.len(),
        output_path.display()
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let workspace = workspace_root()?;

    // 1. Discover WAVs.
    let mut wavs = discover_wavs(&args.source_dir)?;
    println!("discovered {} WAVs in {}", wavs.len(), args.source_dir.display());
    if let Some(n) = args.sample_size {
        let mut rng = rand::rngs::StdRng::seed_from_u64(args.seed);
        wavs.shuffle(&mut rng);
        wavs.truncate(n);
        println!("sampled {} for scoring", wavs.len());
    }
    let total = wavs.len();

    // 2. Score each. Use pancetta decoder with default config.
    let decoder = Ft8Decoder::with_default_config();
    let mut scored: Vec<(PathBuf, String, f64, ScoreBreakdown)> = Vec::with_capacity(total);
    for (i, wav) in wavs.iter().enumerate() {
        match score_wav(&decoder, wav) {
            Ok((score, breakdown, sha)) => scored.push((wav.clone(), sha, score, breakdown)),
            Err(e) => {
                eprintln!("warn: scoring {} failed: {e}", wav.display());
            }
        }
        if (i + 1) % 100 == 0 || i + 1 == total {
            println!("  scored {}/{}", i + 1, total);
        }
    }

    // 3. Sort by score descending; emit Hard-200 + Hard-1000.
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let mk_entry = |(p, sha, s, b): &(PathBuf, String, f64, ScoreBreakdown)| CuratedEntry {
        wav_path: p.clone(),
        wav_sha256: sha.clone(),
        interest_score: *s,
        score_breakdown: b.clone(),
    };
    let scoring_decoder = decoder.identity();

    let output_prefix = if args.output_prefix.is_absolute() {
        args.output_prefix.clone()
    } else {
        workspace.join(&args.output_prefix)
    };
    let hard_200: Vec<_> = scored.iter().take(200).map(mk_entry).collect();
    let hard_1000: Vec<_> = scored.iter().take(1000).map(mk_entry).collect();
    write_manifest(
        "hard_200",
        hard_200,
        &scoring_decoder,
        &output_prefix.join("hard_200.manifest.json"),
    )?;
    write_manifest(
        "hard_1000",
        hard_1000,
        &scoring_decoder,
        &output_prefix.join("hard_1000.manifest.json"),
    )?;

    // 4. Random Wild-50 sample (different seed branch for diversity).
    let mut rng = rand::rngs::StdRng::seed_from_u64(args.seed.wrapping_add(13));
    let mut wild_pool: Vec<_> = scored.iter().collect();
    wild_pool.shuffle(&mut rng);
    let wild_50: Vec<_> = wild_pool.iter().take(50).map(|t| mk_entry(t)).collect();
    write_manifest(
        "wild_50",
        wild_50,
        &scoring_decoder,
        &output_prefix.join("wild_50.manifest.json"),
    )?;

    Ok(())
}
```

- [ ] **Step 3: Build the binary**

Run: `cargo build --release -p pancetta-research --bin curate`

Expected: clean.

- [ ] **Step 4: Smoke-run on a tiny sample**

Without running over the full 22k, do a quick smoke against a sampled subset:

```bash
cargo run --release -p pancetta-research --bin curate -- \
    --source-dir ~/.pancetta/recordings \
    --output-prefix /tmp/curate_smoke \
    --sample-size 50 --seed 42
ls -la /tmp/curate_smoke/
```

Expected: three files — `hard_200.manifest.json` (with 50 entries since only 50 sampled), `hard_1000.manifest.json` (also 50), `wild_50.manifest.json` (50). Takes 1-3 min.

If `sample_size < 200`, the Hard-200 manifest will be smaller than its name — that's expected for the smoke test. Plan 3 ships Hard-200 from the full ~22k corpus in Task 15.

- [ ] **Step 5: Verify the manifest shape**

```bash
python3 -c "
import json
m = json.load(open('/tmp/curate_smoke/hard_200.manifest.json'))
print(f'label={m[\"label\"]}, entries={len(m[\"entries\"])}, scoring={m[\"scoring_decoder\"]}')
print('first entry:', json.dumps(m['entries'][0], indent=2)[:400])
"
```

Expected output: label=hard_200, entries=50, scoring identity present; first entry has wav_path / wav_sha256 / interest_score / score_breakdown fields.

- [ ] **Step 6: Cleanup smoke artifacts**

```bash
rm -rf /tmp/curate_smoke
```

- [ ] **Step 7: Commit**

```bash
git add pancetta-research/src/bin/curate.rs pancetta-research/Cargo.toml
git commit -m "feat(research): curate binary — produces Hard-200/Hard-1000/Wild-50 manifests"
```

---

### Task 9: `curate_smoke` test

**Files:**
- Create: `pancetta-research/tests/curate_smoke.rs`

- [ ] **Step 1: Write the test**

Create `pancetta-research/tests/curate_smoke.rs`:

```rust
//! End-to-end: `curate` binary produces 3 valid manifests when run against
//! a few synth WAVs (the only deterministic corpus we control).
//!
//! Gated on research-eval since it spawns cargo run --bin.

#![cfg(feature = "research-eval")]

use pancetta_research::curated::CuratedManifest;
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn curate_produces_three_manifests() {
    let workspace = workspace_root();
    let source = workspace.join("research/corpus/synth/wavs/clean");
    if !source.exists() {
        // Synth corpus not generated yet. Skip rather than fail — the
        // operator can pre-populate by running gen-synth.
        eprintln!("warn: synth wav dir missing at {}; skipping", source.display());
        return;
    }
    let out_dir = tempfile::tempdir().expect("tempdir");
    let out_prefix = out_dir.path().to_path_buf();

    let status = Command::new("cargo")
        .args([
            "run",
            "--release",
            "-q",
            "-p",
            "pancetta-research",
            "--bin",
            "curate",
            "--",
            "--source-dir",
        ])
        .arg(&source)
        .arg("--output-prefix")
        .arg(&out_prefix)
        .arg("--sample-size")
        .arg("30")
        .arg("--seed")
        .arg("42")
        .current_dir(&workspace)
        .status()
        .expect("curate must run");
    assert!(status.success(), "curate failed");

    for label in ["hard_200", "hard_1000", "wild_50"] {
        let path = out_prefix.join(format!("{label}.manifest.json"));
        let manifest = CuratedManifest::load(&path)
            .unwrap_or_else(|e| panic!("manifest {label} must load: {e}"));
        assert_eq!(manifest.label, label);
        assert!(!manifest.entries.is_empty(), "{label} should have entries");
        // Every entry has the required fields.
        for e in &manifest.entries {
            assert!(!e.wav_sha256.is_empty(), "wav_sha256 must be set");
            assert!(e.wav_path.is_absolute(), "wav_path must be absolute");
        }
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --release -p pancetta-research --features research-eval --test curate_smoke -- --nocapture`

Expected: 1 passed (or 1 ignored with warn if synth dir not present — regenerate via `cargo run --bin gen-synth ...` first if needed).

- [ ] **Step 3: Commit**

```bash
git add pancetta-research/tests/curate_smoke.rs
git commit -m "test(research): curate binary produces 3 valid manifests"
```

---

## Phase C — Curated tier + baseline extension

### Task 10: `baseline` binary accepts `--manifest <path>`

**Files:**
- Modify: `pancetta-research/src/bin/baseline.rs`

- [ ] **Step 1: Add a `--manifest` arg to the baseline binary**

In `pancetta-research/src/bin/baseline.rs`, find the `Args` struct + `parse` function. Add a `manifest: Option<PathBuf>` field that takes a CuratedManifest path. When `--tier curated-hard-200` (or 1000) is passed, the binary loads the curated manifest and runs jt9 over its entries.

Update the tier dispatch in `main()`. Where it currently has:

```rust
    let wavs: Vec<PathBuf> = match args.tier.as_str() {
        "fixtures" => ...
        "synth" => ...
        other => anyhow::bail!("unknown tier '{other}'. Use 'fixtures' or 'synth'."),
    };
```

Replace with:

```rust
    let wavs: Vec<PathBuf> = match args.tier.as_str() {
        "fixtures" => load_ft8_fixtures(&workspace)?
            .into_iter()
            .map(|f| f.wav_path)
            .collect(),
        "synth" => {
            let manifest = args
                .synth_manifest
                .clone()
                .unwrap_or_else(|| {
                    workspace.join("research/corpus/synth/manifests/clean.manifest.json")
                });
            load_synth_corpus(&workspace, &manifest)?
                .into_iter()
                .map(|e| e.wav_path)
                .collect()
        }
        "curated-hard-200" | "curated-hard-1000" | "wild-50" => {
            let label = match args.tier.as_str() {
                "curated-hard-200" => "hard_200",
                "curated-hard-1000" => "hard_1000",
                "wild-50" => "wild_50",
                _ => unreachable!(),
            };
            let manifest = args.manifest.clone().unwrap_or_else(|| {
                workspace
                    .join("research/corpus/curated/ft8")
                    .join(format!("{label}.manifest.json"))
            });
            pancetta_research::curated::load_curated_corpus(&manifest)?
                .into_iter()
                .map(|e| e.wav_path)
                .collect()
        }
        other => anyhow::bail!(
            "unknown tier '{other}'. Use 'fixtures', 'synth', 'curated-hard-200', 'curated-hard-1000', or 'wild-50'."
        ),
    };
```

Add `--manifest` to the arg parser:

```rust
                "--manifest" => manifest = Some(iter.next().context("--manifest needs a value")?.into()),
```

Add `manifest` to the `Args` struct + `parse` function (alongside `synth_manifest`).

- [ ] **Step 2: Build**

Run: `cargo build --release -p pancetta-research --bin baseline`

Expected: clean.

- [ ] **Step 3: Smoke test against a small manifest**

Create a tiny test manifest with 2-3 fixture WAVs and run baseline against it:

```bash
# Use synth WAVs as a stand-in for curated (since curated isn't generated yet).
cat > /tmp/test_curated.manifest.json <<EOF
{
  "schema_version": 1,
  "label": "test",
  "generated_at": "2026-05-20T00:00:00Z",
  "scoring_decoder": "test",
  "entries": [
    {
      "wav_path": "$(pwd)/pancetta-ft8/tests/fixtures/wav/generated/ft8_cq.wav",
      "wav_sha256": "test",
      "interest_score": 1.0,
      "score_breakdown": {"pancetta_decode_count": 1, "noise_floor_db": -30.0}
    }
  ]
}
EOF
cargo run --release -p pancetta-research --bin baseline -- \
    --tier curated-hard-200 --mode ft8 --manifest /tmp/test_curated.manifest.json
```

Expected: processes 1 WAV, prints "baseline: N decodes from ...".

- [ ] **Step 4: Commit**

```bash
git add pancetta-research/src/bin/baseline.rs
git commit -m "feat(research): baseline accepts --manifest + curated tiers"
```

---

### Task 11: `eval` binary implements curated tiers

**Files:**
- Modify: `pancetta-research/src/bin/eval.rs`

- [ ] **Step 1: Add a `run_curated_tier` function**

In `pancetta-research/src/bin/eval.rs`, near the existing `run_fixtures_tier` and `run_synth_tier`, add:

```rust
use pancetta_research::curated::{load_curated_corpus, CuratedEntry};
use pancetta_research::scorecard::PerWavFailure;

fn run_curated_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
    manifest_path: &std::path::Path,
) -> anyhow::Result<TierResult> {
    let entries: Vec<CuratedEntry> = load_curated_corpus(manifest_path)?;
    let total = entries.len() as u32;
    if total == 0 {
        return Ok(TierResult {
            wavs_processed: 0,
            ..Default::default()
        });
    }
    let mut truth_decodes_total = 0u32;
    let mut truth_recovered = 0u32;
    let mut novel_decodes = 0u32;
    let mut wsjtx_total = 0u32;
    let mut per_wav_failures: Vec<PerWavFailure> = Vec::new();

    for entry in &entries {
        // Look up the jt9 baseline cache for this WAV's SHA.
        let baseline_path = workspace
            .join("research/baselines/ft8")
            .join(format!("{}.json", entry.wav_sha256));
        let baseline_decodes: Vec<String> = if baseline_path.exists() {
            let s = std::fs::read_to_string(&baseline_path)?;
            let cache: serde_json::Value = serde_json::from_str(&s)?;
            cache
                .get("decodes")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            // No baseline cached — treat as 0 truth decodes for this WAV.
            Vec::new()
        };
        wsjtx_total += baseline_decodes.len() as u32;
        truth_decodes_total += baseline_decodes.len() as u32;

        let our_decodes = decoder.decode_wav(&entry.wav_path).unwrap_or_default();
        // Match: a baseline decode is "recovered" if we produced a message
        // containing the same callsign tokens. Conservative substring check.
        let mut recovered_here = 0u32;
        for truth_msg in &baseline_decodes {
            if our_decodes.iter().any(|d| d.message.trim() == truth_msg.trim()) {
                recovered_here += 1;
            }
        }
        truth_recovered += recovered_here;

        // "Novel" decodes: ones in our output that aren't in baseline.
        for ours in &our_decodes {
            if !baseline_decodes.iter().any(|t| t.trim() == ours.message.trim()) {
                novel_decodes += 1;
            }
        }

        // Per-WAV failure tracking for the top 20 worst gaps.
        let gap = baseline_decodes.len() as i64 - recovered_here as i64;
        if gap > 0 {
            per_wav_failures.push(PerWavFailure {
                wav_hash: entry.wav_sha256.clone(),
                truth: baseline_decodes.len() as u32,
                recovered: recovered_here,
                wsjtx: baseline_decodes.len() as u32,
                jtdx: 0, // Plan 3 doesn't wire JTDX; field stays 0.
            });
        }
    }

    // Keep top-20 worst gaps for the per_wav_top_failures field.
    per_wav_failures
        .sort_by(|a, b| (b.truth - b.recovered).cmp(&(a.truth - a.recovered)));
    per_wav_failures.truncate(20);

    let decode_rate = if truth_decodes_total == 0 {
        0.0
    } else {
        truth_recovered as f64 / truth_decodes_total as f64
    };
    let vs_wsjtx_pct = if wsjtx_total == 0 {
        0.0
    } else {
        100.0 * truth_recovered as f64 / wsjtx_total as f64
    };

    Ok(TierResult {
        wavs_processed: total,
        truth_decodes_total: Some(truth_decodes_total),
        truth_decodes_recovered: Some(truth_recovered),
        decode_rate: Some(decode_rate),
        novel_decodes: Some(novel_decodes),
        wsjtx_decoded: Some(wsjtx_total),
        vs_wsjtx_pct: Some(vs_wsjtx_pct),
        per_wav_top_failures: per_wav_failures,
        ..Default::default()
    })
}
```

- [ ] **Step 2: Wire the curated tiers into the dispatch in `main()`**

Find the existing tier-dispatch match. Replace the curated-hard-200/curated-hard-1000 stub arms with:

```rust
            "curated-hard-200" | "curated-hard-1000" | "wild-50" => {
                let label = match tier_name.as_str() {
                    "curated-hard-200" => "hard_200",
                    "curated-hard-1000" => "hard_1000",
                    "wild-50" => "wild_50",
                    _ => unreachable!(),
                };
                let manifest = workspace
                    .join("research/corpus/curated/ft8")
                    .join(format!("{label}.manifest.json"));
                anyhow::ensure!(
                    manifest.exists(),
                    "curated manifest missing at {}. Run: cargo run --release -p pancetta-research --bin curate -- --source-dir ~/.pancetta/recordings --output-prefix research/corpus/curated/ft8",
                    manifest.display()
                );
                let result = run_curated_tier(decoder.as_ref(), &workspace, &manifest)?;
                tiers.insert(tier_name.to_string(), result);
            }
```

- [ ] **Step 3: Build**

Run: `cargo build --release -p pancetta-research --bin eval`

Expected: clean.

- [ ] **Step 4: Smoke-test against a fake manifest**

(Real curated eval lands in Task 15 after the curate binary is run over the full corpus.)

```bash
# Use the small test manifest from Task 10's smoke step.
cat > /tmp/test_curated.manifest.json <<EOF
{
  "schema_version": 1,
  "label": "test",
  "generated_at": "2026-05-20T00:00:00Z",
  "scoring_decoder": "test",
  "entries": [
    {
      "wav_path": "$(pwd)/pancetta-ft8/tests/fixtures/wav/generated/ft8_cq.wav",
      "wav_sha256": "test",
      "interest_score": 1.0,
      "score_breakdown": {"pancetta_decode_count": 1, "noise_floor_db": -30.0}
    }
  ]
}
EOF
mkdir -p research/corpus/curated/ft8
cp /tmp/test_curated.manifest.json research/corpus/curated/ft8/hard_200.manifest.json

cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --output /tmp/sc_curated.json

# Sanity:
python3 -c "
import json
d = json.load(open('/tmp/sc_curated.json'))
t = d['tiers']['curated-hard-200']
print(f'wavs={t[\"wavs_processed\"]} truth={t.get(\"truth_decodes_total\")} recovered={t.get(\"truth_decodes_recovered\")} decode_rate={t.get(\"decode_rate\")}')
"

# Cleanup the test manifest before committing — Task 15 produces the real one.
rm research/corpus/curated/ft8/hard_200.manifest.json
```

Expected: `wavs=1 truth=N recovered=R decode_rate=...` — N is 0 (no baseline cached for "test" sha) which means decode_rate=0.0. The point of this smoke is "binary runs, scorecard JSON parses." The real eval comes in Task 15.

- [ ] **Step 5: Commit**

```bash
git add pancetta-research/src/bin/eval.rs
git commit -m "feat(research): eval implements curated-hard-200/1000 + wild-50 tiers"
```

---

## Phase D — Leaderboard

### Task 12: `leaderboard` binary

**Files:**
- Create: `pancetta-research/src/bin/leaderboard.rs`
- Modify: `pancetta-research/Cargo.toml`

- [ ] **Step 1: Add `[[bin]] name = "leaderboard"`**

In `pancetta-research/Cargo.toml`:

```toml
[[bin]]
name = "leaderboard"
path = "src/bin/leaderboard.rs"
```

- [ ] **Step 2: Write `leaderboard.rs`**

Create `pancetta-research/src/bin/leaderboard.rs`:

```rust
//! leaderboard — rank all scorecards in research/scorecards/history/ by composite
//! score. Reads main.json as the current baseline reference.
//!
//! Output: a markdown table sorted by composite score descending.

use anyhow::Context;
use pancetta_research::scorecard::Scorecard;
use std::path::PathBuf;

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn load_all_scorecards(workspace: &PathBuf) -> anyhow::Result<Vec<(PathBuf, Scorecard)>> {
    let mut out = Vec::new();
    let history = workspace.join("research/scorecards/history");
    if history.exists() {
        for entry in std::fs::read_dir(&history)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "json") {
                match Scorecard::load(&path) {
                    Ok(c) => out.push((path, c)),
                    Err(e) => eprintln!("warn: skipping {}: {e}", path.display()),
                }
            }
        }
    }
    let main_path = workspace.join("research/scorecards/main.json");
    if main_path.exists() {
        match Scorecard::load(&main_path) {
            Ok(c) => out.push((main_path, c)),
            Err(e) => eprintln!("warn: main.json: {e}"),
        }
    }
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    let workspace = workspace_root()?;
    let mut all = load_all_scorecards(&workspace)?;
    if all.is_empty() {
        println!("no scorecards found in research/scorecards/");
        return Ok(());
    }
    all.sort_by(|a, b| {
        b.1.composite
            .score
            .partial_cmp(&a.1.composite.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("# Decoder Research Leaderboard");
    println!();
    println!(
        "| Rank | Score | Slug | Branch | Date | Pass | SNR@50 |"
    );
    println!(
        "|------|-------|------|--------|------|------|--------|"
    );
    for (i, (path, card)) in all.iter().enumerate() {
        let slug = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace(".json", "");
        let fixtures_pass = card
            .tiers
            .get("fixtures")
            .and_then(|t| t.pass_rate)
            .map(|p| format!("{:.3}", p))
            .unwrap_or_else(|| "-".into());
        let snr50 = card
            .tiers
            .get("synth-clean")
            .and_then(|t| t.snr_at_50pct_recovery_db)
            .map(|s| format!("{:+.1}", s))
            .unwrap_or_else(|| "-".into());
        let date = card.generated_at.to_rfc3339()[..10].to_string();
        println!(
            "| {} | {:.4} | {} | {} | {} | {} | {} |",
            i + 1,
            card.composite.score,
            slug,
            card.git.branch,
            date,
            fixtures_pass,
            snr50,
        );
    }
    Ok(())
}
```

- [ ] **Step 3: Build the binary**

Run: `cargo build --release -p pancetta-research --bin leaderboard`

Expected: clean.

- [ ] **Step 4: Run it against the current state**

```bash
cargo run --release -p pancetta-research --bin leaderboard
```

Expected: prints a markdown table with one row (just main.json from Plan 2; no history entries yet). Score should be 0.3000.

- [ ] **Step 5: Commit**

```bash
git add pancetta-research/src/bin/leaderboard.rs pancetta-research/Cargo.toml
git commit -m "feat(research): leaderboard binary — markdown table ranked by composite"
```

---

### Task 13: `leaderboard_smoke` test

**Files:**
- Create: `pancetta-research/tests/leaderboard_smoke.rs`

- [ ] **Step 1: Write the test**

Create `pancetta-research/tests/leaderboard_smoke.rs`:

```rust
//! End-to-end: leaderboard reads history/ + main.json and emits a sorted
//! markdown table.

#![cfg(feature = "research-eval")]

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn leaderboard_prints_table_with_main_json() {
    let workspace = workspace_root();
    let out = Command::new("cargo")
        .args([
            "run", "--release", "-q", "-p", "pancetta-research", "--bin", "leaderboard",
        ])
        .current_dir(&workspace)
        .output()
        .expect("leaderboard must run");
    assert!(out.status.success(), "leaderboard should exit 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("# Decoder Research Leaderboard"),
        "expected header; got: {s}"
    );
    // main.json from Plan 2 should appear in the table.
    assert!(
        s.contains("main") || s.contains("0.3"),
        "expected a row for main.json or its score; got: {s}"
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --release -p pancetta-research --features research-eval --test leaderboard_smoke -- --nocapture`

Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add pancetta-research/tests/leaderboard_smoke.rs
git commit -m "test(research): leaderboard prints table with main.json row"
```

---

## Phase E — Generate real curated corpus + baseline + scorecard

### Task 14: Run curate over the full operator corpus

**Files:** none — produces manifest JSON files.

- [ ] **Step 1: Verify the operator corpus exists**

```bash
find ~/.pancetta/recordings -name "*.wav" -type f | wc -l
```

Expected: ~22,464 (or some large number).

- [ ] **Step 2: Run curate over the full corpus**

```bash
cargo run --release -p pancetta-research --bin curate -- \
    --source-dir ~/.pancetta/recordings \
    --output-prefix research/corpus/curated/ft8 \
    --seed 42
```

This is the heavy lift — 22k WAVs × decode + noise estimate = 5-15 minutes total. The binary prints `scored N/22464` every 100 WAVs.

Expected output: three manifest files in `research/corpus/curated/ft8/`:
- `hard_200.manifest.json` — 200 entries
- `hard_1000.manifest.json` — 1000 entries
- `wild_50.manifest.json` — 50 entries

- [ ] **Step 3: Spot-check the top-ranked WAVs**

```bash
python3 -c "
import json
m = json.load(open('research/corpus/curated/ft8/hard_200.manifest.json'))
print('top-5 by interest:')
for e in m['entries'][:5]:
    print(f\"  {e['wav_path'].split('/')[-1]}: score={e['interest_score']:.2f} decodes={e['score_breakdown']['pancetta_decode_count']} noise={e['score_breakdown']['noise_floor_db']:.1f}\")
print('bottom-5 by interest (still in Hard-200):')
for e in m['entries'][-5:]:
    print(f\"  {e['wav_path'].split('/')[-1]}: score={e['interest_score']:.2f} decodes={e['score_breakdown']['pancetta_decode_count']} noise={e['score_breakdown']['noise_floor_db']:.1f}\")
"
```

Expected: top-ranked WAVs have high decode counts (>10) and/or high noise floors. Bottom-of-Hard-200 still has decode counts >1.

- [ ] **Step 4: Commit the manifests**

```bash
git add research/corpus/curated/ft8/
git commit -m "feat(research): curate real corpus — Hard-200 + Hard-1000 + Wild-50 manifests"
```

The committed JSON manifests are small (~70-340 KB each); they reference WAVs in `~/.pancetta/recordings/` by absolute path.

---

### Task 15: Run baseline (jt9) over the curated Hard-200 + Hard-1000 + Wild-50

**Files:** none — produces baseline JSON cache files.

- [ ] **Step 1: Run baseline for Hard-200**

```bash
cargo run --release -p pancetta-research --bin baseline -- \
    --tier curated-hard-200 --mode ft8
```

Expected: 200 jt9 invocations, 200 new JSON cache files in `research/baselines/ft8/`. Takes ~5-10 minutes.

- [ ] **Step 2: Run baseline for Hard-1000**

```bash
cargo run --release -p pancetta-research --bin baseline -- \
    --tier curated-hard-1000 --mode ft8
```

Expected: 1000 jt9 invocations, ~800 new cache files (200 overlap with Hard-200). Takes ~30-45 minutes — kick this off in a separate terminal and continue with other tasks. The baseline binary skips cached results, so re-running is idempotent.

- [ ] **Step 3: Run baseline for Wild-50**

```bash
cargo run --release -p pancetta-research --bin baseline -- \
    --tier wild-50 --mode ft8
```

Expected: 50 jt9 invocations. Some overlap with Hard-1000 (random sample) so most should already be cached.

- [ ] **Step 4: Verify cache count**

```bash
ls research/baselines/ft8/*.json | wc -l
```

Expected: 73 (Plan 2) + ~800-1000 new = ~900-1100. Some overlap is normal.

- [ ] **Step 5: Commit the new baseline files**

```bash
git add research/baselines/ft8/
git commit -m "feat(research): baseline jt9 cache for Hard-200 + Hard-1000 + Wild-50 (~1000 new files)"
```

Total size of `research/` after this commit: 100-300 MB, well under the 500 MB cap.

---

### Task 16: Regenerate `research/scorecards/main.json` with all tiers

**Files:** modify `research/scorecards/main.json`.

- [ ] **Step 1: Run eval against all tiers**

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 \
    --output research/scorecards/main.json
```

Expected: prints "wrote scorecard: research/scorecards/main.json (composite X.XXXX, 5 tier(s), Y.Ys)" where:
- composite is much higher than Plan 2's 0.3000 since the real_decode_rate term (weight 0.50) now contributes
- 5 tiers populated

- [ ] **Step 2: Verify the scorecard**

```bash
python3 -c "
import json
d = json.load(open('research/scorecards/main.json'))
print(f'composite: {d[\"composite\"][\"score\"]:.4f}')
for t, tier in d['tiers'].items():
    print(f'  {t}: pass={tier.get(\"pass_rate\")} decode_rate={tier.get(\"decode_rate\")} snr50={tier.get(\"snr_at_50pct_recovery_db\")} vs_wsjtx={tier.get(\"vs_wsjtx_pct\")}')
"
```

Sanity bounds:
- `fixtures.pass_rate = 1.0`
- `synth-clean.snr_at_50pct_recovery_db ≈ -20 dB`
- `curated-hard-200.decode_rate` is the headline number — likely 0.2-0.5 (pancetta-vs-jt9 on busy bands)
- `vs_wsjtx_pct` reflects how much of jt9's output we recover
- composite likely 0.35-0.55 depending on real_decode_rate

- [ ] **Step 3: Commit the regenerated baseline**

```bash
git add research/scorecards/main.json
git commit -m "feat(research): Plan 3 baseline scorecard — all 5 tiers populated"
```

This is now the "bar to beat" for every future experiment.

---

## Phase F — Hypothesis bank bootstrap + first journaled experiment

### Task 17: Hypothesis-bank bootstrap

**Files:**
- Modify: `research/hypothesis_bank.md`

This is a CLAUDE-DRIVEN ANALYSIS TASK. The implementer reads decoder source, the spec, CLAUDE.md "Known Gaps", and the Plan 3 scorecard, then seeds the hypothesis bank with 15-25 entries.

- [ ] **Step 1: Read source material**

Read each of these and capture notes:

- `CLAUDE.md` (especially "Known Gaps and TODOs" section)
- `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md` (the "What's next" section + composite metric weights)
- `research/scorecards/main.json` (current numbers — particularly any tier that underperforms)
- `pancetta-ft8/src/decoder.rs` lines 1-200 (top-level decode entry + config)
- `pancetta-ft8/src/ap.rs` lines 1-100 (AP/a-priori search)
- `pancetta-ft8/src/osd.rs` lines 1-100 (OSD post-LDPC)
- `pancetta-ft8/src/sync.rs` lines 1-100 (sync candidate search)
- `pancetta-ft8/src/signal_processing.rs` lines 1-100 (DSP frontend)
- The memory file `~/.claude/projects/-Users-thagale-Code-pancetta/memory/project_decoder_status.md` and `project_decoder_sensitivity.md`

- [ ] **Step 2: Draft 15-25 hypothesis entries**

Per the spec's hypothesis-bank format, each entry has:
- ID (`hb-NNN`)
- Title
- `mode: ft8` (or `cross-mode` for shared-DSP ideas)
- `status: pending`
- `priority_score: 0-10` (computed from the rubric in the spec)
- `estimated_effort: <number> sessions`
- `expected_delta: <description>`
- `defensible_prior: yes | no | partial`
- `wild_card: true | false`
- `evidence_for:` (bullet list)
- `evidence_against:` (bullet list)
- `notes:` (description of the intended change)

Aim for:
- 8-12 well-defensible ideas (multi-pass subtract, sync candidate count sweep, AP-survival retune, OSD beta sweep, neural OSD v2, DSP frontend, etc.)
- 4-6 partial-prior ideas
- 2-4 wild-cards (`wild_card: true`)

Replace the contents of `research/hypothesis_bank.md` after the header section:

```markdown
# Hypothesis Bank

last_updated: 2026-05-20T<HH:MM:SS>Z
current_focus_mode: ft8
wild_card_ratio_target: 0.20
wild_cards_run: 0
exploitation_run: 0
current_ratio: 0.0

## Active (ranked by score)

### hb-001 — Multi-pass subtract-and-redecode  [PRIORITY: 9.2]
  mode: ft8
  status: pending
  priority_score: 9.2
  estimated_effort: 2-3 sessions
  expected_delta: +0.05 to +0.15 real decode rate
  defensible_prior: yes (WSJT-X does this; biggest known gap per memory)
  wild_card: false
  evidence_for:
    - Real decode rate ~5-10% of WSJT-X on identical bands per memory
    - Multi-pass is the documented WSJT-X advantage in busy conditions
  evidence_against:
    - Risk of compounding FPs across passes
  notes: |
    Subtract residual at decoded candidate's freq+time, re-run sync on residual.
    Bound passes (max 3). Confidence threshold before subtracting (re memory: previous attempt was prone to FPs).

### hb-002 — Synth plateau investigation  [PRIORITY: 8.5]
  mode: ft8
  status: pending
  priority_score: 8.5
  estimated_effort: 1 session
  expected_delta: unknown (diagnostic)
  defensible_prior: yes (observed in Plan 2 main.json)
  wild_card: false
  evidence_for:
    - synth-clean recovery plateaus at ~83% (5 of 6 messages) across all comfortable-SNR bins
    - This is reproducible — one of the 6 synth messages consistently fails
  evidence_against: []
  notes: |
    Identify which message fails (W9XYZ K1ABC -10? K1ABC W9XYZ R-12?), inspect
    why (encoder/decoder roundtrip quirk? message-type-specific decoder bug?).
    Pure investigation; output is a journal entry + ≥1 follow-up hypothesis.

### hb-003 — Sync candidate count sweep  [PRIORITY: 7.8]
  mode: ft8
  status: pending
  priority_score: 7.8
  estimated_effort: 1 session
  expected_delta: +0.02 to +0.05 real decode rate
  defensible_prior: yes (decoder uses fixed candidate cap)
  wild_card: false
  evidence_for:
    - Hard-200 has WAVs with >10 truth decodes; sync may discard weak candidates
  notes: |
    Sweep MAX_SYNC_CANDIDATES across [16, 32, 64, 128, 256] (default likely 32).
    Plot decode_rate vs candidate count to find inflection.

### hb-004 — AP-survival gate retune  [PRIORITY: 7.4]
  mode: ft8
  status: pending
  priority_score: 7.4
  estimated_effort: 1-2 sessions
  expected_delta: +0.01 to +0.05 real decode rate
  defensible_prior: yes (memory flags this as Task #33 stronghold)
  wild_card: false
  notes: |
    AP-survival gate currently blocks the natural air-only attack but may be
    too aggressive. Retune on Hard-200 to find lowest threshold that doesn't
    introduce FPs.

### hb-005 — OSD beta + iteration sweep  [PRIORITY: 6.8]
  mode: ft8
  status: pending
  priority_score: 6.8
  estimated_effort: 1 session
  expected_delta: +0.01 to +0.03
  defensible_prior: yes (memory: OSD-2 default contradicts safety comment)
  wild_card: false
  notes: |
    Sweep OSD beta and max iterations. Resolve the OSD-2 vs OSD-1 default
    contradiction the memory flagged. Also test OSD on multi-pass residuals.

### hb-006 — Neural OSD v2 (larger model + more training data)  [PRIORITY: 6.5]
  mode: ft8
  status: pending
  priority_score: 6.5
  estimated_effort: 2-3 sessions
  expected_delta: +0.02 to +0.05
  defensible_prior: partial (v1 exists; growing model unproven on FT8)
  wild_card: false
  notes: |
    Extend training/neural_osd/ — bigger CNN, more training data (~1M samples),
    train on MPS GPU. Re-export weights. Compare against neural_osd_v1.

### hb-007 — Wider FFT window for sync  [PRIORITY: 5.9]
  mode: ft8
  status: pending
  priority_score: 5.9
  estimated_effort: 1 session
  expected_delta: +0.01
  defensible_prior: partial
  notes: |
    Memory flags this as a previous shelved attempt — gain was eaten by added
    latency in sync window correlation. Worth revisiting if we ever overhaul
    sync to be FFT-bound instead of time-domain bound.

### hb-008 — DSP frontend: spectral whitening before sync  [PRIORITY: 5.5]
  mode: ft8
  status: pending
  priority_score: 5.5
  estimated_effort: 2 sessions
  expected_delta: +0.02 to +0.04 (especially on noisy bands)
  defensible_prior: partial (used in WSJT-X)
  wild_card: false
  notes: |
    Apply per-bin AGC / spectral whitening to flatten the noise floor before
    sync correlation. Helps decoder ignore high-amplitude carriers near
    weak FT8 signals.

### hb-009 — Per-callsign hash-table-aided decode  [PRIORITY: 4.8]
  mode: ft8
  status: pending
  priority_score: 4.8
  estimated_effort: 2 sessions
  expected_delta: +0.005 to +0.02
  defensible_prior: yes (WSJT-X does this)
  notes: |
    Build a rolling table of recently-decoded callsigns. Use as AP hint for
    the next slot's decode: if "K1ABC" was just decoded, treat it as a
    high-prior token.

### hb-010 — Tone-symbol re-soft-decision after first decode  [PRIORITY: 4.5]
  mode: ft8
  status: pending
  wild_card: false
  estimated_effort: 1-2 sessions
  notes: |
    After a decode succeeds, recompute LLRs assuming the decoded message is
    correct, then redo OSD with the refined LLRs. Marginal but cheap.

### hb-011 — Sync threshold relax with FP detector  [PRIORITY: 4.2]
  mode: ft8
  status: pending
  wild_card: false
  estimated_effort: 1 session
  notes: |
    Lower the sync correlation threshold to admit more candidates, then
    use a downstream FP detector (CRC + AP consistency check) to filter.

### hb-012 — Random gate disabled (wild)  [PRIORITY: 4.1]  [WILD]
  mode: ft8
  status: pending
  wild_card: true
  estimated_effort: 1 session
  notes: |
    What if we just disable the AP-survival false-positive gate entirely
    and see what falls out? We'd expect FPs but we'd also see which decodes
    the gate is currently suppressing. Cheap eval — easy to revert.

### hb-013 — Synthesize +noise + retrain neural OSD on adversarial samples  [PRIORITY: 3.8]  [WILD]
  mode: ft8
  status: pending
  wild_card: true
  estimated_effort: 3 sessions
  notes: |
    Generate adversarial samples: known message + carefully tuned noise that
    confuses the current decoder. Train neural OSD on these to harden it.
    Heavily speculative — may not generalize.

### hb-014 — Decoder ensemble (run twice with shuffled candidate order)  [PRIORITY: 3.5]  [WILD]
  mode: ft8
  status: pending
  wild_card: true
  estimated_effort: 1 session
  notes: |
    Run two passes of the decoder with different candidate-order seeds; take
    the union of CRC-valid decodes. Tests whether order-dependence in greedy
    decoding is leaking decodes.

### hb-015 — Cross-mode: investigate shared DSP frontend  [PRIORITY: 3.2]
  mode: cross-mode
  status: pending
  wild_card: false
  estimated_effort: 1 session
  notes: |
    DSP module is shared between ft8 + future ft4/js8. Audit for any FT8-specific
    assumptions that would prevent reuse. Not a metric-mover for FT8; foundational
    work for future modes.

## Shelved (kept for reference)

(empty — first hypotheses haven't been tried yet)

## Graduated (merged to main)

(empty)
```

Adjust ID numbers, priority scores, and entries based on what the actual decoder source reveals. The list above is illustrative; the implementer is expected to read the actual source and adjust. Aim for 15-25 entries total.

- [ ] **Step 3: Commit**

```bash
git add research/hypothesis_bank.md
git commit -m "feat(research): bootstrap hypothesis bank with 15-25 entries"
```

---

### Task 18: First journaled experiment — synth plateau investigation

**Files:**
- Create: `research/experiments/2026-05-20-synth-plateau-investigation.md`

This experiment is an investigation (no code change). It exercises the journal+disposition workflow and answers: "Which of the 6 synth messages fails to decode at any SNR, and why?"

- [ ] **Step 1: Identify the failing message**

Run:

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier synth-clean --mode ft8 --output /tmp/synth_breakdown.json
python3 -c "
import json
d = json.load(open('/tmp/synth_breakdown.json'))
m = json.load(open('research/corpus/synth/manifests/clean.manifest.json'))

# For each comfortable SNR, decode and see which messages we got
import subprocess
import wave

# We need to actually decode each WAV and see which messages succeeded.
# Reading the synth manifest, we know what should have decoded.
# The snr_at_50% is computed in eval; for the by_snr_db breakdown
# we know aggregate per-bin but not per-message.
# Run eval with a debug flag, OR write a one-off here.

# Simplest: for each WAV at SNR=-10 dB (comfortable), check our decoder output.
import struct
print('Synth-clean recovery by message at -10 dB:')
"
```

Then write a one-off probe script to identify the failing message:

```bash
cat > /tmp/probe_synth.rs <<'EOF'
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use pancetta_research::synth::SynthManifest;
use std::path::Path;

fn main() {
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let manifest = SynthManifest::load(workspace.join("research/corpus/synth/manifests/clean.manifest.json")).unwrap();
    let decoder = Ft8Decoder::with_default_config();
    println!("Message | -22dB | -18dB | -14dB | -10dB");
    println!("---|---|---|---|---");
    for msg in &manifest.config.messages {
        let mut results = vec![];
        for snr in [-22.0, -18.0, -14.0, -10.0] {
            let entry = manifest.entries.iter().find(|e| e.encoded_message == *msg && (e.snr_db - snr).abs() < 0.01).unwrap();
            let path = workspace.join(&entry.wav_path);
            let decodes = decoder.decode_wav(&path).unwrap_or_default();
            let ok = decodes.iter().any(|d| d.message.contains(msg));
            results.push(if ok { "OK" } else { "FAIL" });
        }
        println!("{} | {}", msg, results.join(" | "));
    }
}
EOF
```

Actually a simpler approach: add a temporary `[[bin]]` entry, build, run, then delete. But for the journal, what matters is the OBSERVATION, not the test code.

A cleaner way: read the eval scorecard's `synth-clean.by_snr_db` for each SNR and the manifest, then manually deduce which message is missing. We can do this by running eval with a verbose flag and printing per-message recovery, but that requires temporarily modifying `run_synth_tier`.

The simplest path for the journal: implementer runs the one-off probe (either as a quick `cargo test`-style scratch test or a temporary main.rs in `examples/`), identifies the failing message, then writes the journal documenting the finding.

For the plan, just commit to: identify the failing message and document it. Implementation detail (one-off probe or eval modification) is the implementer's choice — keep changes ephemeral.

- [ ] **Step 2: Write the journal entry**

Create `research/experiments/2026-05-20-synth-plateau-investigation.md`:

```markdown
---
slug: synth-plateau-investigation
mode: ft8
state: shelved
created: 2026-05-20T<HH:MM:SS>Z
last_updated: 2026-05-20T<HH:MM:SS>Z
branch: (none — investigation only)
parent_hypothesis: hb-002
wild_card: false
scorecard: (n/a — no code change)
delta_vs_main: 0.0
disposition: shelved-with-findings
---

## Hypothesis

Plan 2's main.json shows synth-clean recovery plateauing at ~83% (5 of 6
messages) across all comfortable-SNR bins (-18 dB to -10 dB). One of the
6 synthesized messages consistently fails to decode at every SNR level
tested. Identify which message and why.

## Change

None. This is a pure investigation — read the eval output, run a one-off
probe to identify the failing message, document findings.

## Result

Failing message: **<FILL IN — the message that the implementer identified
as never decoding>**

Per-message recovery at -10 dB (comfortable SNR):

| Message | Decodes |
|---|---|
| CQ K1ABC FN42 | OK |
| K1ABC W9XYZ EM48 | OK |
| W9XYZ K1ABC -10 | <OK or FAIL> |
| K1ABC W9XYZ R-12 | <OK or FAIL> |
| W9XYZ K1ABC RR73 | OK |
| K1ABC W9XYZ 73 | OK |

Suspected cause: <e.g., the signal-report messages have specific bit
patterns that the decoder mis-handles; or, the message uses unusual
characters; or, the encoder/decoder roundtrip differs for that specific
message type>.

## Disposition

Shelved — no code change. Investigation produced a clear follow-up
hypothesis (hb-XXX, see below).

## Learnings

- The plateau is real and reproducible (not a measurement artifact).
- The decoder default config has a specific blind spot for <message type>.
- The synth corpus design choice of 6 messages spans message types but the
  failing one represents <~17% of common QSO traffic | a rare message type>.
- For the eval composite to keep moving, we either need to fix this
  blind spot OR add more variety to the synth corpus so one failure
  doesn't dominate.

## Follow-ups added to hypothesis bank

- **hb-XXX**: <specific fix proposal — e.g., "investigate why <message
  type> fails — likely in <encoder/decoder/modulator path>">.
- **hb-XXX+1**: (optional) Extend synth corpus to N message types so
  individual failures are diluted in the metric.
```

The `<...>` placeholders are filled in by the implementer based on what they actually find. The journal frontmatter date and timestamps are set when committing.

- [ ] **Step 3: Update the hypothesis bank**

Add the new follow-up hypothesis to `research/hypothesis_bank.md` under "Active". Mark hb-002 (the parent) as `status: shelved` (investigation done; learning captured).

- [ ] **Step 4: Commit**

```bash
git add research/experiments/2026-05-20-synth-plateau-investigation.md \
        research/hypothesis_bank.md
git commit -m "journal(research): first experiment — synth plateau investigation (shelved with findings)"
```

This commit demonstrates the journal-on-main pattern: shelved experiments leave the journal markdown on main forever, even though no code changes were made. The hypothesis bank tracks the follow-ups for future iterations.

---

## Phase G — Documentation + wrap-up

### Task 19: `pancetta-research/README.md` covers Plan 3 binaries

**Files:**
- Modify: `pancetta-research/README.md`

- [ ] **Step 1: Update Quick start**

Replace the existing Quick start block with one that covers all 5 binaries:

```markdown
## Quick start

```bash
# Build everything
cargo build --release -p pancetta-research

# 1. Generate the synth corpus (60 WAVs: 6 messages × 10 SNR steps)
cargo run --release -p pancetta-research --bin gen-synth -- \
    --config research/corpus/synth/manifests/clean.config.json \
    --output research/corpus/synth/manifests/clean.manifest.json

# 2. Curate the operator's real-world WAVs into 3 ranked manifests
cargo run --release -p pancetta-research --bin curate -- \
    --source-dir ~/.pancetta/recordings \
    --output-prefix research/corpus/curated/ft8

# 3. Cache jt9 baseline over all tiers (one-time, ~45 min total)
cargo run --release -p pancetta-research --bin baseline -- --tier fixtures --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier synth --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier curated-hard-200 --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier curated-hard-1000 --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier wild-50 --mode ft8

# 4. Score the current decoder against all tiers
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 \
    --output research/scorecards/main.json

# 5. Rank all scorecards in research/scorecards/
cargo run --release -p pancetta-research --bin leaderboard

# 6. Diff two scorecards
cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/main.json research/scorecards/history/2026-05-20-experiment-X.json

# Experiment lifecycle (research-env.sh)
./scripts/research-env.sh --status              # list experiments + state
./scripts/research-env.sh --pin <slug>          # protect artifacts from purge
./scripts/research-env.sh --finalize <slug>     # move branch scorecard to history/
./scripts/research-env.sh --cleanup             # dry-run purge of expired artifacts
./scripts/research-env.sh --cleanup --execute   # actually purge
./scripts/research-env.sh --preflight           # disk-cap check before eval
```

WSJT-X must be installed locally for `baseline` to find `jt9`. On macOS,
the default expected path is `/Applications/wsjtx.app/Contents/MacOS/jt9`;
override with `--jt9-path /path/to/jt9` if needed.
```

- [ ] **Step 2: Update plan-status list**

Find the existing plan-status list and update Plan 3 to "complete":

```markdown
- Plan 1 of 3 (foundations): `...` — complete
- Plan 2 of 3 (eval pipeline + corpus): `...` — complete
- Plan 3 of 3 (curation + leaderboard + lifecycle): `docs/superpowers/plans/2026-05-20-research-harness-3-iteration-loop.md` — complete
```

- [ ] **Step 3: Commit**

```bash
git add pancetta-research/README.md
git commit -m "docs(research): README quick-start covers Plan 3 binaries + lifecycle"
```

---

### Task 20: Update `CLAUDE.md`

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update the Architecture Highlights entry**

Find the "Decoder research harness" bullet under Architecture Highlights. Replace with:

```markdown
- **Decoder research harness** (`pancetta-research/`, `research/`,
  `scripts/research-env.sh`): a local-only iteration harness for improving
  the decoder. Excluded from `default-members` and CI by construction.
  Spec: `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.
  Plans 1-3 complete; the loop is operational. Run `./scripts/research-env.sh --status`
  to see active experiments; read `research/hypothesis_bank.md` for the
  current backlog.
```

- [ ] **Step 2: Add Known Gaps entries (if any new ones surfaced)**

In the "Known Gaps and TODOs" section, add (only if Task 18's investigation
revealed new known issues — leave out if not):

```markdown
- Decoder consistently fails on `<failing message>` (synth-clean recovery
  plateaus at ~83%). Diagnosed in
  `research/experiments/2026-05-20-synth-plateau-investigation.md`; follow-up
  in hypothesis bank hb-XXX.
```

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: CLAUDE.md Plan 3 complete; research loop operational"
```

---

### Task 21: Add a RUNBOOK section for the research loop

**Files:**
- Modify: `docs/RUNBOOK.md`

- [ ] **Step 1: Add the section at the end of RUNBOOK**

Append to `docs/RUNBOOK.md`:

```markdown
---

## Decoder Research Iteration Loop

The research harness produces scorecards and tracks experiments. This
loop is local-only (never in CI) and runs in normal Claude Code sessions
(not headless).

### First-time setup (one-shot)

```bash
# Build the research crate.
cargo build --release -p pancetta-research

# Generate the synth corpus.
cargo run --release -p pancetta-research --bin gen-synth -- \
    --config research/corpus/synth/manifests/clean.config.json \
    --output research/corpus/synth/manifests/clean.manifest.json

# Curate the operator's real-world WAVs.
cargo run --release -p pancetta-research --bin curate -- \
    --source-dir ~/.pancetta/recordings --output-prefix research/corpus/curated/ft8

# Cache jt9 baseline for all tiers (~45 min total; one-time).
for tier in fixtures synth curated-hard-200 curated-hard-1000 wild-50; do
    cargo run --release -p pancetta-research --bin baseline -- --tier "$tier" --mode ft8
done

# Compute the main.json baseline.
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 --output research/scorecards/main.json
```

### Running an experiment (Claude-driven)

When you want to try an idea, ask Claude to:

> Pick the highest-priority hypothesis from `research/hypothesis_bank.md`
> and run a full experiment cycle.

Claude will:
1. Pick a hypothesis (respecting the wild-card budget ratio in the bank).
2. Create a worktree + branch: `experiment/ft8/<slug>`.
3. Write `research/experiments/<date>-<slug>.md` with hypothesis text.
4. Implement the change on the branch.
5. Run eval: `cargo run --bin eval -- --tier ... --output research/scorecards/<branch>.json`.
6. Run compare: `cargo run --bin compare -- research/scorecards/main.json research/scorecards/<branch>.json`.
7. Decide merge/shelve/defer based on composite delta + regression flags.
8. If WIN: PR to main, scorecard moves to `history/`, journal updated, hypothesis-bank entry marked graduated.
9. If LOSS: journal documents learnings, follow-ups added to bank, branch deleted, artifacts marked for 14-day purge.
10. Report to operator and either continue (per `/loop`) or stop.

### Checking on the loop's state

```bash
./scripts/research-env.sh --status           # what's in flight, what shelved, what merged
cargo run --release -p pancetta-research --bin leaderboard   # ranked scorecards
./scripts/research-env.sh --preflight        # disk-cap check
```

### Stopping or recovering

- `Ctrl-C` an in-progress eval to abort (eval is idempotent; rerun resumes).
- `./scripts/research-env.sh --pin <slug>` to protect a specific experiment's artifacts from purge.
- `./scripts/research-env.sh --cleanup` (dry-run) shows what would be purged; add `--execute` to actually remove.
```

- [ ] **Step 2: Commit**

```bash
git add docs/RUNBOOK.md
git commit -m "docs: RUNBOOK section for the research iteration loop"
```

---

### Task 22: Final check + push

**Files:** none.

- [ ] **Step 1: Run scripts/check.sh full lane**

Run: `./scripts/check.sh > /tmp/check_p3.log 2>&1; echo exit: $?; tail -5 /tmp/check_p3.log`

Expected: exit 0.

If any failure:
- fmt: `cargo fmt --all` and commit as `style: cargo fmt after Plan 3`.
- clippy: investigate and fix; if pre-existing, file as follow-up.
- workspace tests: real failure — debug.

- [ ] **Step 2: Run the research-eval test lane**

Run: `cargo test --release -p pancetta-research --features research-eval 2>&1 | tail -25`

Expected: all tests pass — including the new ones from Plan 3: `curate_smoke`, `leaderboard_smoke`, `research_env_lifecycle`.

- [ ] **Step 3: Verify default-skip still works**

Run: `cargo build 2>&1 | grep -c "Compiling pancetta-research" || echo 0`

Expected: 0 — pancetta-research stays excluded from default-members.

- [ ] **Step 4: Memory update**

Update `~/.claude/projects/-Users-thagale-Code-pancetta/memory/project_pancetta_status.md` to reflect Plan 3 landing. Note:
- The loop is operational.
- main.json composite is now <X.XXXX> (the real Plan 3 number with all tiers).
- The first journaled experiment has shipped (the synth plateau investigation).
- The hypothesis bank has N entries.

- [ ] **Step 5: Push branch**

Run from the worktree:

```bash
git push -u origin feature/research-harness-plan-3
```

The controller (not subagent) handles push.

---

## Self-Review Checklist

Before declaring Plan 3 complete:

- [ ] `cargo test -p pancetta-research --features research-eval` — all tests pass.
- [ ] `cargo test -p pancetta-research` (no features) — slow/corpus-touching tests skipped.
- [ ] `./scripts/research-env.sh --status` works.
- [ ] `./scripts/research-env.sh --pin nonexistent` exits with usage error.
- [ ] `./scripts/research-env.sh --cleanup` dry-runs cleanly.
- [ ] `./scripts/research-env.sh --finalize` requires a slug.
- [ ] `./scripts/research-env.sh --preflight` and `--guard-ci` still work.
- [ ] `./scripts/check.sh` — exit 0.
- [ ] `research/corpus/curated/ft8/*.manifest.json` — 3 files (hard_200, hard_1000, wild_50).
- [ ] `research/baselines/ft8/` — ~1000+ files.
- [ ] `research/scorecards/main.json` — 5 tiers, composite ~0.35-0.55.
- [ ] `research/hypothesis_bank.md` — 15-25 active entries; first follow-up from the synth-plateau investigation added.
- [ ] `research/experiments/2026-05-20-synth-plateau-investigation.md` — committed with findings.
- [ ] `pancetta-research/README.md` — quick-start covers all 5 binaries + lifecycle.
- [ ] CLAUDE.md — Architecture Highlights reflects Plan 3 complete.
- [ ] `docs/RUNBOOK.md` — Research loop section added.
- [ ] No file > 50 MB committed.

---

## What's next

**Plan 3 is the last planned plan.** After this lands, the harness is
fully operational. From here, every "next thing" is an *experiment*
journaled in `research/experiments/`, driven by ideas from
`research/hypothesis_bank.md`.

Likely first real experiment (after the synth-plateau investigation):
**hb-001 Multi-pass subtract-and-redecode** — the biggest known WSJT-X
gap per memory. Expected delta is +0.05 to +0.15 real decode rate.

The operator can also independently:
- Rotate the leaked `gho_*` GitHub OAuth token (carryover from 2026-04-29).
- Run Phase 5 on-air verification at the rig (code is wired + security-hardened).
- Use `/loop` to have Claude run experiments back-to-back on autopilot.

The `research/scorecards/history/` directory grows with every experiment;
the leaderboard ranks them; the hypothesis bank tracks what's tried,
what's learned, and what's next.
