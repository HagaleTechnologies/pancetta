# Rubato 4.0 Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate `pancetta-dsp`'s `rubato` dependency from 3.0.0 to 4.0.0 with bit-identical resampling output on the live audio DSP pipeline, closing out dependabot PR #126.

**Architecture:** This plan was written after direct source-level comparison of rubato 3.0.0 vs 4.0.0 (fetched into the local cargo registry cache and read directly — not from web docs) and, critically, after actually compiling and running the migration end-to-end against this repo before writing a single plan step. That verification found the real migration surface is much narrower than dependabot's PR description suggests ("Resampler trait split into Adjustable+Resizable, non-trivial rewrite"): the `Resampler` trait is unchanged as the base trait pancetta uses; `Adjustable`/`Resizable` are new, purely *additive* sub-traits that pancetta never implements or calls. There are exactly two breaking changes that touch pancetta's code, both confined to `pancetta-dsp/src/resampler.rs`:

1. `Resampler::process()`'s signature collapsed `(input_offset: usize, active_channels_mask: Option<&[bool]>)` into a single `indexing: Option<&Indexing>` parameter. Pancetta's two call sites (`process()` and `flush()`) both pass `(0, None)` today; passing `None` for `indexing` reproduces the exact same defaults (offset 0, no channel mask) per rubato's own doc comment.
2. `SincInterpolationParameters.f_cutoff` changed from `f32` to `Option<f32>`. **This was not caught by source inspection alone — it only surfaced when the migration was actually compiled against rubato 4.0.0.** `None` now means "auto-select cutoff" (a new 4.0.0 feature, not available in 3.0.0), which would be a real behavior change. `Some(0.95)` preserves the exact original hardcoded value and is the correct, behavior-preserving fix.

No other pancetta-dsp file needs changes. `pipeline.rs`'s `ResamplingStage` only calls `AudioResampler`'s own wrapper methods (`process(&[f32], &mut Vec<f32>)`, `reset()`) — never rubato's `Resampler` trait directly — so it recompiles unchanged. `lib.rs`'s re-exports and `factory::create_ft8_resampler()` are untouched.

**On verification:** the goal (as originally framed) called for verifying "against the existing decoder eval harness/fixtures." That harness does not exist for this migration — `pancetta-research` (home of `eval.rs` and the WAV-corpus fixtures) does not depend on `pancetta-dsp` at all, and `pancetta`'s `loopback_qso` integration test encodes and decodes at a fixed 12000 Hz on both ends, so it never exercises resampling either. Neither path would catch a resampling regression. Instead, Task 1 adds a dedicated bit-exact characterization ("golden vector") regression test directly to `resampler.rs`, with baseline values captured by actually running the test against the current (pre-migration) rubato 3.0.0 code in this repo. Task 2's migration is verified by confirming that exact same test still passes — i.e., the resampler's numerical output is unchanged, not just "close enough."

Everything in this plan (the golden-vector values, the compiler errors, the final green test run) was independently reproduced against this repo before writing the plan; none of it is predicted.

**Tech Stack:** Rust workspace crate `pancetta-dsp`; `rubato` 4.0.0 (async SINC resampler) replacing 3.0.0; `audioadapter`/`audioadapter-buffers` 4.0.0 (pulled in transitively by rubato, no direct pancetta-dsp dependency on these crates).

## Global Constraints

- Pin `rubato = "4.0"` in `pancetta-dsp/Cargo.toml` — matches the existing `"3.0"` pin's style (caret-implicit, comment preserved).
- `mode=FT8` paths must remain byte-identical (CLAUDE.md invariant) — enforced here by the golden-vector test asserting exact (not approximate) output length and a bit-pattern checksum over every output sample.
- No behavior change to any pancetta-dsp module other than `resampler.rs`'s two `process()` call sites and the `f_cutoff` field literal.
- Full workspace test gate must stay green: `cargo test --workspace --features transmit` (per CLAUDE.md's documented build command).
- `pancetta-dsp` carries `#![allow(missing_docs)]` (not `warn`/`deny`) — no new doc-comment obligations from this migration.

---

### Task 1: Add a golden-vector characterization test against current (rubato 3.0.0) behavior

**Files:**
- Modify: `pancetta-dsp/src/resampler.rs` (test module only, lines 326-352 today)

**Interfaces:**
- Consumes: `AudioResampler::new_ft8_optimized() -> Result<Self>`, `AudioResampler::process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<usize>`, `AudioResampler::flush(&mut self) -> Result<Vec<f32>>` — all pre-existing public methods, unchanged by this task.
- Produces: `golden_test_signal(sample_rate: f32, num_samples: usize) -> Vec<f32>` and `golden_checksum(samples: &[f32]) -> u64`, private test helpers used only within this test module. Test name `test_resampler_golden_vector_48k_to_12k`, referenced by Task 2's verification step.

This is a characterization test, not new-feature TDD: the values it asserts are the actual current, correct behavior of the resampler, captured by running the test once. There is no red step for the test itself — it passes immediately once the (already-known-correct) baseline values are in place. The "red" step for this migration happens in Task 2, when the dependency bump breaks compilation.

- [ ] **Step 1: Add the golden-vector test to the test module**

In `pancetta-dsp/src/resampler.rs`, inside `mod tests` (after the existing `test_decimator_creation` test, before the closing `}` of the module):

```rust
    fn golden_test_signal(sample_rate: f32, num_samples: usize) -> Vec<f32> {
        (0..num_samples)
            .map(|n| {
                let t = n as f32 / sample_rate;
                0.5 * (2.0 * std::f32::consts::PI * 1500.0 * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * 800.0 * t).sin()
                    + 0.2 * (2.0 * std::f32::consts::PI * 2200.0 * t).sin()
            })
            .collect()
    }

    fn golden_checksum(samples: &[f32]) -> u64 {
        samples
            .iter()
            .fold(0u64, |acc, &s| acc.wrapping_mul(31).wrapping_add(s.to_bits() as u64))
    }

    #[test]
    fn test_resampler_golden_vector_48k_to_12k() {
        let input = golden_test_signal(48000.0, 12_345);
        let mut resampler = AudioResampler::new_ft8_optimized().unwrap();

        let mut output = Vec::new();
        resampler.process(&input, &mut output).unwrap();
        output.extend(resampler.flush().unwrap());

        assert_eq!(output.len(), 4095, "resampler output length changed");
        assert_eq!(
            golden_checksum(&output),
            17831856880251919678,
            "resampler output diverged from golden vector"
        );
    }
```

The three-tone signal (1500/800/2200 Hz) sits inside the FT8 audio passband and needs no external RNG dependency, so the fixture is bit-reproducible across toolchains. `12_345` input samples is not a multiple of the resampler's internal chunk size, so this exercises both the multi-chunk loop in `process()` and the zero-padded remainder path in `flush()` — the same code paths the live audio pipeline drives. The checksum folds `f32::to_bits()` (not summed floats), so this is an exact-reproducibility check: any change to the resampler's numerical output, however small, changes the checksum. `4095` and `17831856880251919678` are the real values captured by running this exact test against the current, unmigrated rubato 3.0.0 build in this repo — not placeholders.

- [ ] **Step 2: Run the test to confirm the captured baseline is correct**

Run: `cargo test -p pancetta-dsp test_resampler_golden_vector_48k_to_12k -- --nocapture`

Expected:
```
running 1 test
test resampler::tests::test_resampler_golden_vector_48k_to_12k ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

- [ ] **Step 3: Run the full pancetta-dsp suite to confirm no other test regressed**

Run: `cargo test -p pancetta-dsp`

Expected: `test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out` for the lib test binary (16 pre-existing tests + the 1 new golden-vector test), plus `test result: ok. 2 passed` for doc-tests.

- [ ] **Step 4: Commit**

```bash
git add pancetta-dsp/src/resampler.rs
git commit -m "test(dsp): add golden-vector regression test for resampler output"
```

---

### Task 2: Migrate to rubato 4.0.0

**Files:**
- Modify: `pancetta-dsp/Cargo.toml:21`
- Modify: `pancetta-dsp/src/resampler.rs:73` (the `SincInterpolationParameters` literal), `pancetta-dsp/src/resampler.rs:158` (inside `process()`), `pancetta-dsp/src/resampler.rs:235` (inside `flush()`)

**Interfaces:**
- Consumes: Task 1's `test_resampler_golden_vector_48k_to_12k` as the regression gate for this task's changes.
- Produces: no new interfaces — `AudioResampler`'s public API is unchanged; only its internal rubato call sites move.

- [ ] **Step 1: Bump the dependency**

In `pancetta-dsp/Cargo.toml`, change:

```toml
rubato = "3.0"  # High-quality resampling
```

to:

```toml
rubato = "4.0"  # High-quality resampling
```

- [ ] **Step 2: Build and confirm it fails exactly as expected**

Run: `cargo build -p pancetta-dsp`

Expected: two distinct errors, reproduced verbatim from an actual build against rubato 4.0.0 in this repo:

```
error[E0308]: mismatched types
  --> pancetta-dsp/src/resampler.rs:73:23
   |
73 |             f_cutoff: 0.95, // Preserve most of the frequency content
   |                       ^^^^ expected `Option<f32>`, found floating-point number
   |
   = note: expected enum `Option<f32>`
              found type `{float}`
help: try wrapping the expression in `Some`
   |
73 |             f_cutoff: Some(0.95), // Preserve most of the frequency content
   |                       +++++    +

error[E0061]: this method takes 2 arguments but 3 arguments were supplied
   --> pancetta-dsp/src/resampler.rs:158:18
    |
158 |                 .process(&input_adapter, 0, None)
    |                  ^^^^^^^                 - unexpected argument #2 of type `{integer}`
    |
help: remove the extra argument
    |
158 -                 .process(&input_adapter, 0, None)
158 +                 .process(&input_adapter, None)
    |

error[E0061]: this method takes 2 arguments but 3 arguments were supplied
  --> pancetta-dsp/src/resampler.rs:235:18
```

- [ ] **Step 3: Fix the `SincInterpolationParameters` literal**

In `pancetta-dsp/src/resampler.rs`, inside `AudioResampler::new()`, change:

```rust
            f_cutoff: 0.95, // Preserve most of the frequency content
```

to:

```rust
            f_cutoff: Some(0.95), // Preserve most of the frequency content
```

`None` in rubato 4.0.0 means "auto-select the cutoff" (a new feature, not present in 3.0.0) — that would silently change the resampler's frequency response. `Some(0.95)` is the explicit-override form and reproduces the exact value pancetta has always used.

- [ ] **Step 4: Fix the two `process()` call sites**

In `pancetta-dsp/src/resampler.rs`, inside `process()` (around line 158), change:

```rust
            let output_chunk = self
                .resampler
                .process(&input_adapter, 0, None)
                .map_err(|e| ResamplerError::ProcessingFailed {
                    message: format!("Resampling failed: {}", e),
                })?;
```

to:

```rust
            let output_chunk = self
                .resampler
                .process(&input_adapter, None)
                .map_err(|e| ResamplerError::ProcessingFailed {
                    message: format!("Resampling failed: {}", e),
                })?;
```

Inside `flush()` (around line 235), change:

```rust
            let output_chunk = self
                .resampler
                .process(&input_adapter, 0, None)
                .map_err(|e| ResamplerError::ProcessingFailed {
                    message: format!("Final resampling failed: {}", e),
                })?;
```

to:

```rust
            let output_chunk = self
                .resampler
                .process(&input_adapter, None)
                .map_err(|e| ResamplerError::ProcessingFailed {
                    message: format!("Final resampling failed: {}", e),
                })?;
```

`None` for the new single `indexing` parameter reproduces the old `(0, None)` defaults exactly (offset 0, no active-channel mask) per rubato's own doc comment on `Resampler::process()`.

- [ ] **Step 5: Build and confirm success**

Run: `cargo build -p pancetta-dsp`

Expected: `Finished` with zero errors and zero warnings (verified: `Finished \`dev\` profile [optimized + debuginfo] target(s) in 12.31s` against this exact diff).

- [ ] **Step 6: Run the full pancetta-dsp suite, confirming the golden vector is unchanged**

Run: `cargo test -p pancetta-dsp`

Expected: identical to Task 1 Step 3 — `test result: ok. 17 passed; 0 failed`, including `test resampler::tests::test_resampler_golden_vector_48k_to_12k ... ok` with the *same* checksum assertion (`17831856880251919678`) now passing against rubato 4.0.0's output. This is the proof the migration is behavior-preserving, not just compiling.

- [ ] **Step 7: Run clippy**

Run: `cargo clippy -p pancetta-dsp --all-targets -- -D warnings`

Expected: `Finished` with zero warnings (verified clean against this exact diff).

- [ ] **Step 8: Commit**

```bash
git add pancetta-dsp/Cargo.toml pancetta-dsp/src/resampler.rs Cargo.lock
git commit -m "chore(dsp): migrate rubato 3.0 -> 4.0

process() collapses (input_offset, active_channels_mask) into a single
Option<&Indexing> parameter; SincInterpolationParameters.f_cutoff becomes
Option<f32>. Both fixed to reproduce identical prior behavior, verified
bit-identical via the golden-vector regression test."
```

---

### Task 3: Full-workspace verification, docs, and PR #126 cleanup

**Files:**
- Modify: `docs/DECISIONS/config-and-platform.md` (append a dated entry)
- No other file changes — this task is verification + documentation only.

**Interfaces:**
- Consumes: the committed state from Task 2 (rubato 4.0.0, golden-vector test green).
- Produces: nothing new — closes out the plan.

- [ ] **Step 1: Run the full workspace test gate**

Run: `cargo test --workspace --features transmit 2>&1 | tee /tmp/rubato-migration-workspace-test.log`

Expected: no `FAILED` lines and no `^error` lines. Verify with:

```bash
grep -c "^test result: ok" /tmp/rubato-migration-workspace-test.log
grep -c "FAILED\|^error" /tmp/rubato-migration-workspace-test.log
```

Expected: the first command's count matches the number of test binaries in the workspace (nonzero, no drop from the pre-migration baseline); the second command returns `0`. Do not trust a background-task wrapper's own reported exit code for this run — grep the raw log directly (this repo has a recorded history of background exit-code mismatches on disk-pressure failures; grepping the log itself is the only reliable check).

- [ ] **Step 2: Run the workspace clippy gate**

Run: `cargo clippy --workspace --features transmit`

Expected: `Finished` with zero errors (this mirrors `scripts/check.sh`'s own `clippy` step, so a clean run here means the pre-push hook's clippy stage will also pass).

- [ ] **Step 3: Run the loopback QSO integration test**

Run: `cargo test -p pancetta --test loopback_qso`

Expected: all tests pass. This test does not exercise the resampler (it encodes/decodes at a fixed 12000 Hz on both ends), so this step is defense-in-depth confirming the dependency bump didn't destabilize anything else in the `pancetta` binary crate that transitively depends on `pancetta-dsp`.

- [ ] **Step 4: Append a dated entry to the platform decisions doc**

In `docs/DECISIONS/config-and-platform.md`, add a new `##` section (placement: anywhere after the existing sections, order in this file is not chronological):

```markdown
## Rubato 3.0 -> 4.0 migration (2026-07-14)

`pancetta-dsp/src/resampler.rs`: migrated off rubato 3.0.0 (dependabot PR #126, deferred because
the raw version bump alone didn't compile). The `Resampler` trait itself is unchanged in 4.0 —
`Adjustable`/`Resizable` are new additive sub-traits pancetta never touches, not a split of the
trait pancetta uses. Two real breaking changes, both confined to this one file:
`Resampler::process()` collapsed `(input_offset: usize, active_channels_mask: Option<&[bool]>)`
into a single `Option<&Indexing>`; `SincInterpolationParameters.f_cutoff` changed from `f32` to
`Option<f32>` (`None` now means "auto-select cutoff", a new 4.0 feature — `Some(0.95)` preserves
the original explicit value). Verified behavior-preserving via a new bit-exact characterization
test (`test_resampler_golden_vector_48k_to_12k`) whose baseline was captured against the live
rubato 3.0.0 code before migrating, then re-asserted unchanged against 4.0.0 — confirmed the
migration is a pure call-site fix with zero numerical impact. Note for future migrations of this
crate: neither `pancetta-research`'s eval harness nor `pancetta`'s `loopback_qso` test exercises
`pancetta-dsp`'s resampler at all (`pancetta-research` doesn't depend on `pancetta-dsp`;
`loopback_qso` runs entirely at a fixed 12000 Hz), so a dedicated resampler-level regression test
is the only thing that actually catches a resampling regression in this codebase today.
```

- [ ] **Step 5: Commit the docs update**

```bash
git add docs/DECISIONS/config-and-platform.md
git commit -m "docs: record rubato 4.0 migration in config-and-platform decisions"
```

- [ ] **Step 6: Close dependabot PR #126 as superseded**

This is a manual `gh` step, not a code change — run after the branch from Tasks 1-3 is pushed and merged (or once the controller confirms the migration is complete):

```bash
gh pr comment 126 --body "Superseded by the manually-executed migration in docs/superpowers/plans/2026-07-14-rubato-4-migration.md — the raw version bump in this PR doesn't compile on its own (rubato 4.0 breaking changes: \`Resampler::process()\` signature, \`SincInterpolationParameters.f_cutoff\` type). Closing in favor of the reviewed migration."
gh pr close 126
```

Confirm afterward with `gh pr view 126 --json state` — expected `"state": "CLOSED"`.
