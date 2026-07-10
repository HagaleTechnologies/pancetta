# Acceptance-metric calibration (Workstream 2, Task W2.1)

**Date**: 2026-07-07
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: BIT-EXACT task — the acceptance metric is computed and logged on every reachable
CRC-valid decode; nothing in the decode pipeline reads it to accept/reject anything yet. This
log records the metric's implementation, the calibration run, and the resulting threshold
recommendation for later gating tasks (W2.2-W2.5).

## What this is

Design spec `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md` §4 (D2) roots
OSD's current disablement (`osd_depth: Some(0)`) in a real gap: OSD accepts the **first**
CRC-14-passing candidate out of up to 121,485 flip patterns with **no distance/acceptance
metric at all**. At CRC-14's 2⁻¹⁴ collision rate, false passes are a statistical certainty at
that trial volume — Batch 73 measured ~7,000 spurious FPs for zero net TP when OSD ran
unguarded. This task builds the signal-domain acceptance metric that will let OSD return
safely (W2.2+): an LLR-domain weighted soft distance to the channel's own hard decisions, a
hard-error count, and an optional coherent re-encode correlation.

## Implementation

New module `pancetta-ft8/src/acceptance.rs`:

```rust
pub struct AcceptanceScore {
    pub soft_distance: f32,   // sum(|llr_i| where sign mismatch) / sum(|llr_i|)
    pub hard_errors: u16,     // count of hard-decision disagreements over 174 bits
    pub coherence: Option<f32>,
}
pub fn score(codeword: &BitSlice, channel_llrs: &[f32; 174]) -> AcceptanceScore
```

- **Hard-decision convention** matches `LdpcDecoder::llrs_to_bits` exactly: `llr < 0.0 ⟹ bit = 1`.
- **`soft_distance`** is the WSJT-X-mainline-OSD-style weighted soft distance (clean-room ref
  `research/specs/spec-wsjtx-mainline-osd174.md`): `Σ|llr_i|` over mismatching bits, divided by
  `Σ|llr_i|` over all 174 bits. Clamped to `[0, 1]`; degenerate all-zero-magnitude input returns
  `0.0` rather than dividing by ~zero.
- **`coherence`** is always `None` from `score()` itself (no spectrogram access in a pure LLR/bit
  function). A `normalize_coherence(raw, num_deltas)` helper exists to map
  `known_coherence_score`'s raw `(-2*num_deltas, 0]` output to `0..=1`, but **no call site wires
  it in this task** — see "Scope decisions" below.
- **`score_from_slice`**: convenience wrapper for the common `&[f32]`/`Vec<f32>` shape at call
  sites (vs. the brief's fixed `&[f32; 174]`); returns `None` on a length mismatch instead of
  panicking.

TDD (RED confirmed before GREEN — see task report for the exact stub/revert sequence): 9 unit
tests in `acceptance.rs`, including the required distinguishing case
(`soft_distance_disagrees_with_naive_hard_error_ordering`): a single bit flipped at `|llr|=100`
(hard_errors=1) yields `soft_distance ≈ 0.366`, while twenty bits flipped at `|llr|=0.01` each
(hard_errors=20) yields `soft_distance ≈ 0.0013` — soft_distance correctly ranks the single
confidently-wrong bit as far MORE suspicious than twenty weak disagreements, which a bare
hard-error count could not distinguish (it would rank 1 < 20 the other way).

### Wiring: `DecodedMessage.acceptance: Option<AcceptanceScore>`

Added to `pancetta-ft8/src/message.rs`; both constructors (`new`, `from_ft8lib`) and the one
field-literal test-scaffolding site default it to `None`.

Every CRC-valid native-decode finalization site in `decoder.rs` was traced and wired (9 sites,
not the brief's simplified "BP/OSD/AP" — the real pipeline has many more due to the multi-pass
rescue mechanisms layered on over time):

| Site | Channel LLRs used |
|---|---|
| `try_ldpc_with_ap` (sequential, AP0-AP4) | `base_llrs` — pre-AP-injection |
| `par_try_ldpc_with_ap` (parallel twin) | `base_llrs` — pre-AP-injection |
| `par_try_ldpc_with_recent_only` | `base_llrs` — pre-AP-injection |
| `par_decode_candidate` standard trial (+ BICM-ID rescue branch) | `llrs` fed to `decode_soft`/rescue |
| `par_decode_candidate` fine-FFT fallback (21-trial) | `llrs` fed to `decode_soft` |
| `cross_cycle_averaging_pass` | `llrs` (averaged-candidate) |
| `coherent_subtract_and_repass` | `llrs` (residual candidate) |
| `joint_pair_retry_pass` | `llrs` (residual candidate) |
| `joint_residual_localized_sync_pass` | `llrs` (localized-sync candidate) |

**AP paths deliberately score against `base_llrs` (pre-injection), not the post-injection `llrs`
fed to BP/OSD.** AP injection is a decoder-internal bias — a prior baked in specifically to help
that candidate's codeword pass CRC — so scoring against the post-injection LLRs would be
self-fulfilling at exactly the bit positions AP biased. `base_llrs` is the actual received-signal
evidence, independent of any decoder-side steering. This is the same "channel, not posteriors"
principle the brief calls out, extended one step further for the AP-specific case.

**Left `acceptance: None` (by design, not oversight):**
- `try_cross_sequence_decodes` and `a7_cross_correlation_pass` — these are template
  cross-correlation matches (`a7::best_template_score` against pre-encoded candidate texts), not
  CRC-verified LDPC decodes at all. There is no codeword to score against channel LLRs; the a7
  "codeword" IS the template's, by construction (see the existing comment at that call site).
- Hand-built test scaffolding (`decoder.rs` unit tests constructing `DecodedMessage` directly).
- ft8_lib FFI decodes (`DecodedMessage::from_ft8lib`) — no native channel-LLR array exists for
  those.

### Scope decision: `coherence` left unwired in production call sites

The struct field is `Option<f32>` specifically so partial coverage is a valid, honest state —
this mirrors the existing `confidence_features` precedent in the same struct (also genuinely
partial: populated at `try_ldpc_with_ap` but `None` at most of the other 8 sites above). Wiring
`known_coherence_score` requires spectrogram + candidate `(freq_bin, freq_sub, time_step)` +
decoded tone_symbols all in scope together, which is available at every site above but adds
real per-site complexity (the raw score has to be normalized against the actual `num_deltas`
counted, which `known_coherence_score` doesn't expose back to the caller today). Given the
task's bit-exact/telemetry-only scope and the time budget, this task ships `soft_distance` +
`hard_errors` fully wired (the two channel/LLR-domain fields the brief's `score()` signature
actually computes) and leaves `coherence` for a follow-up — it is optional in the struct, no
downstream consumer expects it yet, and the calibration below did not need it to reach a
usable threshold.

## Verification: bit-exact, decode counts unchanged

- `cargo test --workspace --features transmit`: **0 failed**, full run (see task report for
  per-crate breakdown). `pancetta-ft8` lib: 456 passed standalone (`-p pancetta-ft8 --features
  transmit --lib`; 458 under full-workspace feature unification — the +2 is Cargo enabling an
  additional optional feature transitively via workspace unification, not a code-path change;
  same test names, zero failures either way).
- `pancetta/tests/loopback_qso.rs` (14 tests, all pass unchanged) and the `pancetta-research`
  `research-eval`-gated corpus tests (`synth_corpus_decodes_at_comfortable_snr`,
  `committed_truth_json_parses_and_covers_all_fixtures`, chrono-replay, tier-slot tests) all
  pass unchanged — these assert specific decoded message content/counts, so any accidental
  behavior change from this task's wiring would have shown up as a failure here.
- The wiring is additive-only by construction: every site sets `.acceptance` on a
  `DecodedMessage` AFTER all existing accept/reject gates (confidence floor, suspicion score,
  AP-injection-survival, CRC) have already decided to keep the decode. Nothing added a new
  `continue`/`return None`/early-exit anywhere.

## Calibration run

**Command:**
```
cargo build --release -p pancetta-research --bin acceptance_calibration
./target/release/acceptance_calibration \
  --output research/scorecards/acceptance_calibration.csv \
  --target-fdr 0.01
```
(New binary: `pancetta-research/src/bin/acceptance_calibration.rs`. Also added
`soft_distance`/`hard_errors`/`coherence` fields to the research `Decode` view struct so the
harness can see them — `pancetta_ft8::acceptance::AcceptanceScore` isn't otherwise exposed
outside the crate's own `DecodedMessage`.)

**Config**: `Ft8Decoder::with_default_config().with_osd_depth(Some(2))` — a **local override on
the research wrapper only**. `pancetta_ft8::Ft8Config::default()` (production) is untouched and
still ships `osd_depth: Some(0)`.

**Corpus**: full `hard_200` (200 real operator-recording WAVs,
`research/corpus/curated/ft8/hard_200.manifest.json`) + full `noise_1000` (1000 pure-noise WAVs
+ 30% birdie interference, Workstream 0's Task W0.1 corpus,
`research/corpus/curated/noise/noise_1000.manifest.json`). No reduction was needed — full
corpus completed in reasonable time (see below), so the "smaller corpus" fallback in the task
brief's escalation guidance wasn't invoked.

**Elapsed**: hard_200 in 119.8 s (200 files), noise_1000 in 1188.5 s (1000 files) — **total
~21.8 minutes**. Noise files are markedly more expensive per-file at `osd_depth=2` than
real-signal files (BP almost never converges on pure noise, so OSD always runs its full trial
budget; on hard_200, many candidates converge in BP alone and never reach OSD at all) —
consistent with the design doc's framing of OSD cost.

**Ground truth (`is_verified`, the brief's `is_jt9_verified`)**: for `hard_200`, a decode's exact
message text appearing in that WAV's cached jt9 baseline
(`research/baselines/ft8/<sha256>.json`); for `noise_1000`, always `false` (no signal exists in
that corpus by construction — any decode is definitionally a false positive).

### A methodological correction made during this run

The naive approach — treat every jt9-unverified hard_200 decode as a false positive for FDR
purposes — produces a **useless result**: `hard_200` yielded 6472 acceptance-scored decodes,
only 1250 (19.3%) jt9-verified; the other 5222 (80.7%) are "novel" (jt9 didn't independently
confirm them). Under the naive treatment, **no threshold achieves FDR ≤ 1%** at all (see the
CSV / task report for the full naive sweep) — the metric would look useless.

This is a measurement artifact, not a metric failure: `hard_200` is curated to be
**operationally hard** (marginal/weak signals), and jt9 itself has a well-documented recall gap
on exactly this kind of corpus — `eval.rs::run_curated_tier` already has its own "novel decode"
classifier for this same ambiguity (a novel decode is "unconfirmed", not "wrong"). Lumping every
hard_200-novel row in as a false positive would make the acceptance metric look artificially
much worse than it is.

So this run computes **two** sweeps:
1. **NAIVE** (all 7307 rows; hard_200-novel counted as FP) — reported for transparency, but
   acknowledged as a pessimistic, unusable upper bound.
2. **DEFINITIVE** (2085 rows: `noise_1000`'s 835 unambiguous FPs + `hard_200`'s 1250
   jt9-verified unambiguous TPs; the 5222 hard_200-novel rows are **excluded**, not
   force-labeled either way) — this is the basis for the threshold recommendation below.

## Full distributions

| Population | n | `soft_distance` min / median / mean / max | `hard_errors` min / median / mean / max |
|---|---|---|---|
| hard_200, jt9-verified (definitive TP) | 1250 | 0.0 / 0.0084 / 0.0155 / 0.0761 | 0 / 7 / 8.13 / 37 |
| hard_200, novel/unverified (ambiguous, excluded from FDR) | 5222 | 0.0 / 0.0111 / 0.0231 / 0.3300 | 0 / 8 / 10.19 / 64 |
| noise_1000 (definitive FP, by construction) | 835 | 0.0242 / 0.1606 / 0.1639 / 0.3214 | 15 / 45 / 45.27 / 65 |

`coherence` is empty for all 7307 rows (never populated — see "Scope decision" above; the CSV
column exists and is correctly empty, not a bug).

Notably, the **ambiguous hard_200-novel population's distribution sits much closer to the
verified-TP population than to the noise-FP population** (median soft_distance 0.0111 vs.
0.0084 for TP vs. 0.1606 for FP; median hard_errors 8 vs. 7 for TP vs. 45 for FP) — this is
independent, self-consistent evidence that most of those "novel" decodes are real signals jt9's
own recall gap missed, not hallucinations, which is exactly what curating `hard_200` for
difficulty would predict.

## FDR sweep (definitive subset) and chosen threshold

Accept-if-`soft_distance <= t`, walking `t` upward through every distinct observed value:

| threshold t | n_accepted | n_false_positive | FDR |
|---|---|---|---|
| 0.0828 | 1252 | 2 | 0.16% |
| 0.0951 | 1259 | 9 | 0.71% |
| **0.0976** | **1262** | **12** | **0.95%** |
| 0.0999 | 1263 | 13 | 1.03% |
| 0.1016 | 1265 | 15 | 1.19% |

**Chosen threshold: `soft_distance <= 0.0976` at target FDR ≤ 1%.**

At this threshold, on the definitive-subset data:
- **All 1250/1250 (100%) of jt9-verified true positives are accepted** — zero recall loss on
  the confirmed-real-signal population in this sample.
- **12/835 (1.4%) of noise-corpus false positives sneak through** — i.e. this threshold rejects
  **823/835 (98.6%)** of the guaranteed hallucinations in the noise tier.
- Applying the same threshold to the *ambiguous* hard_200-novel population would accept
  4967/5222 (95.1%) of them — consistent with the "these are mostly real, jt9-missed decodes"
  reading above, not a red flag (if they were mostly noise-hallucinations, we'd expect them to
  cluster near the noise-FP distribution and get rejected at a similar rate to `noise_1000`,
  which is not what happens).

This is a strong, self-consistent separation for a first calibration pass, and validates the
core premise of design decision D2: a signal-domain acceptance check on channel LLRs can
distinguish real decodes from CRC-14-coincidence noise passes even at `osd_depth=2`, where
today's ungated OSD would (per the design doc's Batch 73 finding) flood the output with FPs.

## Caveats and follow-ups for W2.2+

- **This is a first-pass calibration, not a validated gate.** N=835 FP samples and N=1250 TP
  samples is a reasonable start but not exhaustive; W2.2+ should re-verify against a larger/held-out
  slice before wiring `0.0976` (or whatever the re-verified number is) as an actual accept/reject
  gate.
- **`coherence` is unused in this calibration** (always empty) — a future session could wire it
  at the highest-value site(s) (`par_decode_candidate`'s standard trial, the actual production
  hot path) and re-run this same harness to see whether it adds separation beyond `soft_distance`
  alone, per D2's "(b) optional coherent re-encode correlation" bullet.
- **The naive (all-rows) sweep is intentionally not the recommendation** — see "methodological
  correction" above. Anyone re-running this analysis should use the definitive-subset
  methodology (`noise_1000` FP + hard_200-jt9-verified TP only), not the raw CSV blindly.
- **`hard_errors` alone tracks `soft_distance` closely** in this data (both separate cleanly) —
  W2.2 should check whether a combined rule adds anything over `soft_distance` alone before
  building extra gating complexity around both fields.
- CSV retained at `research/scorecards/acceptance_calibration.csv` (7307 rows,
  `tier,wav_hash,soft_distance,hard_errors,coherence,is_verified`) for anyone who wants to
  re-derive thresholds differently.
