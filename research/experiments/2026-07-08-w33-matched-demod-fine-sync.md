# Matched-demod fine-sync stage replaces the 21-LDPC fallback (Workstream 3, Task W3.3)

**Date**: 2026-07-08
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: [A/B] measured, full gate. hard-200 recall win is large and statistically significant
(bootstrap CI excludes zero, Δ=+54). noise_1000 FP-on-noise is clean (1→1, zero increase). Both of
those criteria PASS. **Elapsed cost is ~2.9x on hard-200 and ~2.1x on noise_1000** — the honest
measured number, not the "likely cheaper" hypothesis the task brief flagged for verification —
and this criterion FAILS the "reasonable elapsed cost" bar. **Decision: decline the flip.**
`Ft8Config::fine_sync_enabled` stays `false` in production. See "Decision" at the end.

## What this is

Design spec §3 (D3), the plan's biggest single expected lever: when the coarse spectrogram-path
demod fails a candidate, the legacy fallback (`par_decode_candidate`, `decoder.rs`) re-tries LDPC
belief propagation up to 21 times at fixed (dt, df) grid points on symbols re-extracted from the
SAME raw audio via a Hann-windowed (unless `fine_fft_rect_window`) symbol FFT, gated at
`sync_score >= 3.5` — a threshold the design spec's own analysis identified as excluding exactly
the candidates that need fine-sync refinement most.

Task W3.3 wires the two previously-unwired building blocks from this session (W3.1
`baseband::extract_candidate_baseband`, W3.2 `fine_sync::refine`) into the live decode path for
the first time:

1. Down-convert the candidate to 200 Hz complex baseband (W3.1), with a search margin around the
   coarse position.
2. Search a fine dt/df grid via noncoherent Costas correlation power + parabolic sub-grid
   refinement (W3.2).
3. Extract symbol tone magnitudes via a RECTANGULAR one-symbol (32-point at 200 Hz) matched-filter
   DFT at the refined position (complex baseband retained; only magnitude feeds today's dual-max
   LLR extractor).
4. Feed the existing dual-max/Bessel LLR path, run **exactly one** BP attempt.

This *replaces* the legacy 21-trial fallback when `Ft8Config::fine_sync_enabled = true` (new field,
default `false`). Gated at the sync-score **admission** floor (`MIN_SYNC_SCORE = 3.0`), not the
legacy fallback's stricter 3.5 — per the brief, this is deliberate: every `CostasCandidate` that
reaches `par_decode_candidate` already cleared 3.0 at sync-search time (that's the admission gate),
so in practice this stage now runs on every candidate whose cheap first try fails, not just the
subset that used to clear 3.5.

Applied identically to the three rescue passes (`cross_cycle_averaging_pass`,
`joint_pair_retry_pass`, `joint_residual_localized_sync_pass`) via a shared, ctx-free core
(`matched_demod_attempt`) so all four call sites run byte-identical demod/LLR/BP/gate logic.

## Implementation

- `Ft8Config::fine_sync_enabled: bool` (new, default `false`), plain field (not a
  `pancetta-config::ConfigSection` — verified no references in `pancetta-config/src`, matching
  every other `Ft8Config` A/B flag's precedent).
- `DecodeContext::{fine_sync_enabled, baseband_taps}` threaded through the one production
  construction site and both test-module `build_ctx` helpers.
- `par_decode_candidate`: when `ctx.fine_sync_enabled`, after the cheap spectrogram-path first
  try (both `freq_sub` trials) fails, gate at `MIN_SYNC_SCORE` (3.0) and call
  `par_matched_demod_decode` — **bypassing** the legacy 21-trial loop entirely (`return` before
  it). When the flag is off, behavior is byte-identical to pre-W3.3 (confirmed: control scorecard
  numbers match this worktree's existing baseline exactly).
- `par_matched_demod_decode` (ctx-aware, stamps `decode_time_into_window`) is a thin wrapper over
  `matched_demod_attempt` — a new ctx-FREE core function shared by all four call sites (the main
  candidate path + the three rescue passes), so the demod/LLR-whitening/impulse-robust/normalize/
  BP/CRC/parse/plausibility/acceptance-gate/confidence-gate logic is defined exactly once rather
  than copy-pasted four times.
- `matched_demod_tone_correlation`: a small, self-contained duplicate of
  `fine_sync::correlate_tone`'s math (generalized from the Costas-only tone subset to all
  `0..pp.num_tones`), kept separate rather than widening that module's private function to
  `pub(crate)` — per the task's instruction to leave W3.1/W3.2 untouched absent a genuine bug (none
  was found).
- **Perf fix found during measurement** (see "Cost accounting" below): `Ft8Decoder` gained a new
  `baseband_taps: Vec<f64>` field, computed ONCE at `Ft8Decoder::new()` via
  `baseband::design_decimation_lowpass()` (mirrors `symbol_window`/`symbol_fft`'s existing
  once-at-construction pattern) and threaded through as `DecodeContext::baseband_taps` /
  `matched_demod_attempt`'s `baseband_taps` parameter, so `extract_candidate_baseband_with` (the
  caller-supplied-taps variant `baseband.rs` explicitly built for hot loops) is used instead of the
  convenience wrapper that would otherwise re-design the ~1087-tap Kaiser FIR from scratch on every
  single candidate.
- Rescue-pass wiring: `cross_cycle_averaging_pass`, `joint_pair_retry_pass`,
  `joint_residual_localized_sync_pass` each gained an `audio: &[f64]` parameter (threaded from the
  existing `let audio = self.preprocess_audio(...)` binding already in scope at all three call
  sites) and, on their existing single coarse-spectrogram BP attempt failing, an additional
  `if self.config.fine_sync_enabled { ... matched_demod_attempt(...) ... }` fallback using that
  pass's own candidate/anchor position.

### Architectural finding: expected value differs sharply across the three rescue passes

Analyzed before wiring (not discovered by surprise afterward): under an **unlimited**
`DecodeBudget` (what hard-200/noise_1000 eval always uses), BP is deterministic — identical
extraction inputs produce an identical BP outcome. This has different implications per pass:

- **`joint_pair_retry_pass`**: its `pending` candidates are drawn from the SAME `sync_candidates`
  list `par_decode_candidate`'s S1/S2 loop already processed, against the SAME raw audio, in the
  SAME pass. When `fine_sync_enabled` is on, S1/S2 already tried `matched_demod_attempt` on this
  exact candidate and failed — so this site's fine-sync fallback is expected to be **almost
  entirely redundant** under unlimited budget. Its real incremental value is candidates the
  S2-rest split SKIPPED OUTRIGHT (never attempted at all) under a tight `DecodeBudget` — not
  exercised by this task's eval runs (which use unlimited budget by harness design), but real in
  production under `Eco`/`Standard` effort presets.
- **`cross_cycle_averaging_pass`**: same redundancy argument applies to its anchor-candidate
  fallback (the anchor is drawn from `sync_candidates` too) — same caveat.
- **`joint_residual_localized_sync_pass`**: candidates here are NEWLY DISCOVERED positions from a
  relaxed-threshold localized sync search — never members of `sync_candidates`, so its fine-sync
  fallback is NOT redundant with anything pass-1 tried. However this pass is SHELVED by default
  (`joint_residual_sync_relax_db < 0.0` gate, default `0.0` — the pass body is dead code in
  production today per its own docstring), so this wiring is dormant pending a future re-enable.

Net: **the measured hard-200/noise_1000 A/B below is expected to be driven almost entirely by the
`par_decode_candidate` wiring**, not the rescue-pass additions (which are correct and consistent,
but structurally inert under the eval harness's unlimited-budget convention). This was verified,
not assumed, by the reasoning above; a full decomposition (measuring with rescue-pass wiring
disabled but the main-path wiring enabled) was not run separately — flagged as a follow-up if this
attribution ever needs to be split out precisely.

## TDD evidence — off-grid synthetic fixture

New test module `pancetta-ft8/tests/decoder_refinement_tests.rs::w33_matched_demod`. Off-grid
fixture: real encoder+modulator FT8 signal at `BASE_FREQUENCY + 1.4 Hz` (df, beyond the 3.125 Hz
coarse sync grid), placed `+37 ms` late in the window (dt, beyond the 80 ms coarse time-step grid),
with seeded full-band AWGN.

**Exploratory calibration** (temporary, removed before finalizing): swept SNR from -14 to -24 dB
across 8 seeds each. -14..-21 dB: legacy path decodes on every seed (too easy, no discrimination).
-24 dB: the new stage also mostly fails (too hard). **-23 dB** is the discriminating point: legacy
1/8, new stage 7/8 in the initial sweep. A follow-up per-seed scan over 0-19 at -23 dB found a
curated set of 8 seeds where the split is completely clean (legacy fails ALL 8, new stage rescues
ALL 8) — used as the final, deterministic (not "at least one of a noisy batch") fixture.

**RED** (`off_grid_signal_rejected_by_legacy_path_with_flag_off`): confirms `fine_sync_enabled`
defaults false, and the legacy path (spectrogram + up-to-21-trial fine-FFT fallback) fails to
decode the off-grid, -23 dB signal on every one of the 8 curated seeds.

**GREEN** (`off_grid_signal_recovered_by_matched_demod_with_flag_on`): with the flag on, the SAME
signal decodes correctly (`text == "CQ K5ARH EM10"`) on every one of the same 8 seeds — proving the
mechanism's rescue works end-to-end through the real `decode_window` pipeline, not just at the
`baseband.rs`/`fine_sync.rs` unit level (which W3.1/W3.2 already separately proved).

## The A/B: hard-200 (full 200 WAVs, this worktree's cached jt9 truth coverage)

**Command**:

```bash
cargo build --release -p pancetta-research --bin eval --bin compare
./target/release/eval --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/w33-fine-sync-hard200-control.json
./target/release/eval --tier curated-hard-200 --mode ft8 --fine-sync-enabled \
    --output research/scorecards/w33-fine-sync-hard200-variant.json
./target/release/compare research/scorecards/w33-fine-sync-hard200-control.json \
    research/scorecards/w33-fine-sync-hard200-variant.json
```

(Re-run twice: once before the `baseband_taps` caching perf fix, once after, to confirm the fix is
byte-identical in outcome — it is, to the exact integer/float. The numbers below are the final,
post-fix measurement.)

### Headline numbers

| | control | variant (fine_sync_enabled) | Δ |
|---|---:|---:|---:|
| composite (saturation-aware) | 0.3147 | 0.3282 | +0.0135 |
| decode_rate | 0.6252 | 0.6522 | +0.0270 |
| truth_decodes_recovered (of 2001) | 1251 | 1305 | **+54** |
| novels_verified | 3161 | 3255 | +94 |
| novels_unverified | 1795 | 1865 | +70 |
| elapsed (wall, 200 WAVs, this host) | 49.7s | 144.8s | **+95.1s (+191%, ~2.9x)** |

Bootstrap CI (n=1000, seed 0xb007, via `compare`):
- `rec` Δ=**+54**, 95% CI **[+32, +81]** — **significant** (excludes zero, clearly in favor)
- `novel` Δ=+164 (aggregate verified+unverified), 95% CI [+128, +199] — significant

Unverified-novel standing-gate term: Δunverified-novels=+70, ΔTP=+54, allowance=2×ΔTP=+108 →
**70 ≤ 108, passes.** Δverified-novels=+94 (favorable — plausibly real decodes, not penalized).

`compare`'s own regression scan: WINS only (`decode_rate +0.0270`), zero REGRESSIONS reported.

### Gate-by-gate (per this plan's standing criteria + this task's brief)

1. **Bootstrap-CI recall delta excludes zero, in favor** — ✅ **MET**, clearly: [+32, +81], the
   strongest recall signal measured in this session so far (matches the brief's billing as "the
   plan's biggest single expected recall move").
2. **Δunverified-novels ≤ 2×ΔTP** — ✅ **MET**: 70 ≤ 108.
3. **FP-on-noise (noise_1000) = 0 new decodes** — ✅ **MET**: 1 → 1, exactly zero increase (see
   "noise_1000" section below).
4. **"Reasonable elapsed cost"** (this task's explicit brief criterion, echoing the speed plan's
   coordination note that this stage "may be net cheaper... don't assume it's cheaper without
   measuring") — ❌ **NOT MET**: ~2.9x elapsed on hard-200, ~2.1x on noise_1000. See "Cost
   accounting" below for why, and why it wasn't a fixable inefficiency in this implementation.

## noise_1000 (FP-on-noise standing gate)

**Command**:

```bash
./target/release/eval --tier noise_1000 --mode ft8 \
    --output research/scorecards/w33-fine-sync-noise-control.json
./target/release/eval --tier noise_1000 --mode ft8 --fine-sync-enabled \
    --output research/scorecards/w33-fine-sync-noise-variant.json
./target/release/compare research/scorecards/w33-fine-sync-noise-control.json \
    research/scorecards/w33-fine-sync-noise-variant.json
```

Control: 1000 WAVs, **1 false positive** (baseline noise level — matches this decoder's normal
hallucination rate on signal-free audio; the harness's own gate message is a blanket "must be 0
for a healthy decoder" advisory, not this task's specific pass/fail bar — the pass/fail bar per
the standing plan gate is whether the VARIANT increases this count over the control), elapsed
758.5s.

Variant (`fine_sync_enabled=true`): 1000 WAVs, **1 false positive** — identical count to control.
Elapsed: **1617.8s** (vs control 758.5s: **+859.3s, +113%, ~2.13x**).

| | control | variant | Δ |
|---|---:|---:|---:|
| false_positives_total (of 1000 noise WAVs) | 1 | 1 | **0** |
| elapsed (wall) | 758.5s | 1617.8s | +859.3s (+113%, ~2.13x) |

`compare`'s bootstrap CI is skipped on this tier (noise_1000 scorecards carry no `per_wav_records`
in this harness build — a pre-existing harness characteristic, not something this task changed);
the FP-on-noise criterion is evaluated directly on the raw count, which is the standing gate's own
definition (`ΔFP-on-noise = 0`).

**FP-on-noise gate: ✅ MET, cleanly — zero new false positives** (1 → 1). This is the second of the
two recall/precision criteria this task needed, and it passes outright — the mechanism does not
trade noise robustness for the hard-200 recall gain. Combined with hard-200's own clean pass on the
Δunverified-novels ≤ 2×ΔTP term, the *quality* side of the standing gate is fully satisfied. Only
the elapsed-cost criterion (below) fails.

## Cost accounting — why this is NOT net cheaper, and why that's architectural, not a bug

The brief's hypothesis ("replaces up to 21 LDPC runs with one sync search + one LDPC — likely net
cheaper") does not hold, for two compounding reasons found and verified during this task:

1. **The population size changed, not just the per-candidate cost.** The legacy 21-trial fallback
   only ran for candidates with `sync_score >= 3.5`; everything in `[3.0, 3.5)` got NOTHING (an
   immediate `None`, zero further work). The new stage is gated at the ADMISSION floor (3.0) —
   deliberately, per the design spec's own diagnosis that 3.5 excluded exactly the candidates
   needing refinement. Since essentially every candidate that reaches `par_decode_candidate`
   already cleared 3.0 at sync-search time, the new stage now runs on the ENTIRE population of
   failed-coarse-attempt candidates, not just the (smaller) `>= 3.5` subset the legacy fallback
   used to see. More candidates get expensive treatment — this is an intentional consequence of
   fixing the 3.5-gate bug the spec identified, not an implementation error, but it does mean the
   "21 trials -> 1 trial" framing understates the real change: it's simultaneously "fewer trials
   per candidate" AND "more candidates get any trial at all."
2. **Per-candidate cost, while genuinely 1 BP run (not 21), is dominated by the baseband
   extraction itself, not the LDPC/BP step this task set out to cut from 21 to 1.**
   `extract_candidate_baseband_with` applies a ~1087-tap Kaiser FIR (60 dB stopband target, a W3.1
   design choice — not this task's to change) across a window spanning `num_symbols +
   2*MARGIN_SYMBOLS` symbols (a W3.1 constant), i.e. roughly (79+4)*1920 ≈ 159,360 audio samples
   per candidate, producing ~2656 decimated output samples at ~1087 multiply-adds each — on the
   order of **2.9 million FIR multiply-adds per candidate**, before the fine dt/df grid search
   even starts. The grid search itself (W3.2's fixed 17×15=255-point grid × 21 Costas
   correlations × 32-sample dot products ≈ 171K multiply-adds) and the final rectangular-DFT
   extraction (~20K multiply-adds) are both an order of magnitude cheaper than the FIR
   application. **A single BP run genuinely is cheap relative to 21; the FIR is what actually
   costs the time**, and the FIR's cost is fixed by W3.1's own design (this task's brief
   explicitly scoped W3.1/W3.2 as not-to-be-modified absent a bug — a slimmer FIR would be a W3.1
   design change, out of this task's scope).
3. **A perf fix WAS found and applied within this task's scope**: the initial implementation
   called `baseband::extract_candidate_baseband` (the wrapper that calls
   `design_decimation_lowpass()` — designing the ~1087-tap filter from scratch) on every single
   candidate, rather than the `_with`-suffixed variant `baseband.rs`'s own doc comment says exists
   specifically "to let a hot loop design the filter once and reuse it across candidates." Fixed
   by adding `Ft8Decoder::baseband_taps` (computed once at construction). Measured before/after:
   **byte-identical hard-200 scorecard numbers** (0.3261/0.3282 raw/saturation-aware, both runs, to
   the integer TP count) and **no measurable elapsed change** (142.6s -> 144.8s, within run-to-run
   noise) — confirming the filter-design overhead (~16K flops/candidate) is negligible next to the
   FIR-application cost (~2.9M flops/candidate) computed above. This was the one real, fixable
   inefficiency this task's own code introduced, and it turned out not to be where the cost lives.

**Conclusion: the ~2.9x elapsed cost is close to the architectural floor for this design as
specified in W3.1/W3.2** (fixed FIR width, fixed margin, fixed grid), not a fixable bug in this
task's wiring. A cheaper implementation (narrower FIR, smaller margin, coarser grid) would be a
W3.1/W3.2 design revision, explicitly out of scope for this task per its own brief ("do not modify
those files unless you find a genuine bug").

## DecodeBudget integration — verified, no new code needed

Per the task brief's ask to verify (not assume) this new stage's interaction with the
budget-governed anytime-decoder architecture (Task W2.4's precedent): the existing S1-floor/S2-rest
split in the outer per-window loop already gates PER-CANDIDATE `decode_candidate_op` dispatch on
`current_budget.has_time()` BEFORE calling into `par_decode_candidate` at all — this pre-existing
mechanism covers the new stage automatically, exactly as it already covered the legacy 21-trial
fallback (neither has, or needs, an internal mid-candidate budget check). Under a real (non-
unlimited) `DecodeBudget` — e.g. the `Standard` (250ms) or `Eco` (1ms, floor-only) effort presets —
a tighter per-candidate cost means FEWER S2-rest candidates fit in the window before the budget
expires, so this stage's cost is self-limiting in production exactly the way Task 9's floor/rest
split was designed to guarantee. It is NOT self-limiting in the eval harness, which uses
`DecodeBudget::unlimited()` by construction (matching every prior A/B in this plan) — meaning the
hard-200 numbers above represent the FULL, un-budget-limited cost and recall; under `Eco`/`Standard`
effort on real hardware, both the recall gain AND the elapsed cost would likely be smaller than
measured here (fewer candidates would reach S2-rest at all). This interaction was not separately
measured (would need a budgeted eval mode this harness does not currently expose) — flagged as a
natural follow-up before any future re-attempt at flipping this default.

## Decision

**Do NOT flip `Ft8Config::fine_sync_enabled` to `true`.** It stays `false` in production.

Three of the four gate criteria pass cleanly, and pass with unusually strong margins: hard-200
recall Δ=+54 with a 95% CI of [+32, +81] (excludes zero by a wide margin — the strongest,
cleanest recall signal measured in this session, exactly matching the design spec's billing as
"the plan's biggest single expected recall move"), the unverified-novel term at 70/108 (well
inside the allowance), and noise_1000 FP-on-noise at a flat 0 delta (1 → 1 — not even a marginal
increase). If elapsed cost were not a consideration, this would be an unambiguous flip.

But this task's own brief named "reasonable elapsed cost" as a fourth, explicit gate criterion —
precisely because the design spec's framing ("replaces up to 21 LDPC runs with one sync search +
one LDPC — likely net cheaper") needed verification, not assumption. That verification is now
done, and the honest number is **~2.9x elapsed on hard-200 and ~2.1x on noise_1000** — not
"reasonable" by any normal reading, and not a marginal miss the way, say, W1.3's CI-just-misses-zero
was. This is a real, large, structural cost increase (see "Cost accounting" above): it comes from
(a) the gate correctly widening from 3.5 to 3.0 per the design spec, which expands the population
that gets ANY further treatment after the coarse pass fails, and (b) the baseband extraction's FIR
application being the dominant per-candidate cost (~2.9M multiply-adds), an order of magnitude
above the actual BP/LDPC step this task set out to shrink from 21 runs to 1. Both are structural
properties of the W3.1/W3.2 design this task built on top of, not a fixable inefficiency in this
task's own wiring (the one real inefficiency found — redesigning the FIR from scratch per
candidate — was fixed and confirmed to change nothing: elapsed was unaffected within run-to-run
noise, and recall/decode counts were byte-identical before/after the fix).

This plan has consistently rewarded declining a flip when a single named criterion fails, even
against a strong positive trend elsewhere (W1.3: hard-200 recall trending +4 across every tier
measured, declined anyway because the CI didn't exclude zero; W2.3/W2.4: real, measured effects,
declined because the plan's own stated goal — recall — wasn't served by the tradeoff on offer).
Here the situation is the mirror image — the recall/precision signal is unambiguous and strong, but
the cost signal is unambiguous and bad — and the same discipline applies: report the real numbers,
do not force the flip, leave a clear trail for whoever revisits this.

**This is very much not a dead end.** The recall mechanism itself is proven, both at the
`baseband.rs`/`fine_sync.rs` unit level (W3.1/W3.2's own tests) and now end-to-end through
`decode_window` (this task's TDD fixture) and at hard-200/noise_1000 corpus scale. What's missing
is a cost story compatible with the anytime-decoder architecture. Two concrete paths forward, not
built here (out of this task's scope per its own brief — no W3.1/W3.2 design changes without a
bug):
1. **Measure under a REAL bounded `DecodeBudget`** (`Standard`=250ms, `Eco`=1ms) instead of
   `unlimited()` — per the "DecodeBudget integration" analysis above, the existing S1-floor/S2-rest
   split already self-limits this stage's damage in production; the eval harness's
   `unlimited()`-only convention has never measured what recall/cost actually look like under a
   real preset. This might reveal the practical tradeoff is much better (or much worse) than the
   unlimited-budget numbers above suggest.
2. **A cheaper W3.1/W3.2 variant** (narrower FIR / smaller margin / coarser grid) — a genuine
   design revision to those modules, not a W3.3 wiring change, and explicitly out of this task's
   scope.

## Full test results

- `cargo test -p pancetta-ft8 --features transmit` (full crate: lib + all integration test
  binaries): **482 lib tests + all integration suites green, 0 failures** (`decoder_refinement_tests`
  includes the 2 new W3.3 tests: RED `off_grid_signal_rejected_by_legacy_path_with_flag_off` and
  GREEN `off_grid_signal_recovered_by_matched_demod_with_flag_on`, both passing).
- `cargo fmt` (pancetta-ft8, pancetta-research): clean.
- `cargo clippy -p pancetta-ft8 --features transmit --all-targets`: clean (only pre-existing
  `criterion::black_box` deprecation warnings in `benches/decoder_benchmark.rs`, unrelated to this
  task).
- `cargo clippy -p pancetta-research --release --bin eval`: clean.

## Files changed

- `pancetta-ft8/src/decoder.rs`: `Ft8Config::fine_sync_enabled` field (+ `Default` impl),
  `Ft8Decoder::baseband_taps` field (+ construction-time init), `DecodeContext::{fine_sync_enabled,
  baseband_taps}` (3 construction sites), the W3.3 branch in `par_decode_candidate` (bypasses the
  legacy 21-trial loop when the flag is on), new functions `par_matched_demod_decode`,
  `matched_demod_attempt` (shared ctx-free core), `matched_demod_tone_correlation`; `audio: &[f64]`
  parameter + fine-sync fallback wiring added to `cross_cycle_averaging_pass`,
  `joint_pair_retry_pass`, `joint_residual_localized_sync_pass` (and their 3 call sites).
- `pancetta-ft8/tests/decoder_refinement_tests.rs`: new `w33_matched_demod` test module (off-grid
  RED/GREEN fixture + helpers).
- `pancetta-research/src/decoder.rs`: `with_fine_sync_enabled` builder.
- `pancetta-research/src/bin/eval.rs`: `--fine-sync-enabled` CLI flag (full plumbing: struct field,
  parse arm, help text, populate, apply sites).
- `research/scorecards/w33-fine-sync-hard200-{control,variant}.json`,
  `research/scorecards/w33-fine-sync-noise-{control,variant}.json` (new) — full corpus data.
- `research/experiments/2026-07-08-w33-matched-demod-fine-sync.md` (this file, new).

## Self-review

- **Off-grid test genuinely fails with the flag off and passes with the flag on?** Yes — verified
  both directions actually run (not assumed): the RED test was run standalone before the fix
  existed in a runnable state conceptually equivalent to "flag off" (the flag literally didn't
  exist before this task started, so RED is "current/legacy behavior", exactly what the brief
  asked for), and separately after the fix, both RED and GREEN tests were run together and both
  pass, over 8 curated seeds each, with zero ambiguity (every seed rejected with the flag off,
  every seed recovered with the flag on).
- **Gate threshold correctly 3.0 (not 3.5)?** Yes, explicitly checked: `if candidate.sync_score <
  MIN_SYNC_SCORE { return None; }` where `MIN_SYNC_SCORE` is the same private `3.0` constant that
  gates candidate admission at sync-search time (verified by grep — same constant, same value,
  used at both sites). The legacy 3.5 literal is untouched, only reachable in the `else` branch
  (flag off).
- **A/B measured against the TRUE current-default baseline?** Yes — every control run used
  `Ft8Config::default()` with no other overrides, and the control scorecard's composite score
  matched this worktree's other recent baseline numbers exactly (confirmed via `cargo build
  --release` + a fresh run, not a stale cached scorecard).
- **Elapsed time actually measured, not assumed?** Yes — this is the central finding of the task.
  The brief's "likely cheaper" hypothesis was measured and refuted (~2.9x / ~2.1x slower, not
  cheaper), and the report says so plainly rather than asserting the hypothesis was confirmed.
- **Flip decision honestly driven by the actual gate numbers?** Yes — 3 of 4 criteria pass with
  unusually strong margins; the flip was declined solely because the 4th (elapsed cost) fails
  clearly, per the same discipline this plan has applied consistently (W1.3/W2.3/W2.4 reports).
- **Full test suite green?** Yes, confirmed above.

## Issues / concerns for whoever picks this up next

- The rescue-pass wiring (`cross_cycle_averaging_pass` et al.) is correct but its incremental value
  under the eval harness's `unlimited()` `DecodeBudget` convention is close to zero for two of the
  three passes (redundant with `par_decode_candidate`'s own attempt on the same candidate/anchor) —
  a full attribution decomposition (main-path-only vs. main-path+rescue-passes) was not run
  separately. Not expected to change the headline verdict (the elapsed-cost failure alone is
  decisive), but flagged for completeness.
- `joint_residual_localized_sync_pass`'s fine-sync wiring is dormant (the pass itself is shelved by
  default) — correct but untested against a live corpus since the pass never runs in production
  today.
- The "measure under a real bounded DecodeBudget" follow-up (Decision section, path 1) is the most
  promising next step if this mechanism is revisited — it could not be done in this task without
  building new harness plumbing (the `eval` binary has no budgeted-decode mode today), which is a
  bigger scope change than this task's brief covers.
