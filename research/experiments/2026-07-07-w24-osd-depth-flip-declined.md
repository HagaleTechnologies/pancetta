# OSD depth flip Some(0) -> Some(2) — A/B against the TRUE production baseline — DECLINED (Workstream 2, Task W2.4, Flip 1)

**Date**: 2026-07-07/08
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: [A/B] — **DECLINED**. Production default (`osd_depth: Some(0)`) is UNCHANGED.

## What this is

W2.1-W2.3 built and calibrated a signal-domain acceptance gate for OSD's order-1/2/3
escalation (soft_distance <= 0.0976), fixed OSD to select the best-by-distance CRC-valid
candidate instead of the first, and confirmed the default input-LLR source (BpPosterior).
All three tasks measured their changes at a RESEARCH-ONLY `osd_depth: Some(2)` override —
**none of them ever flipped production**, so none of them ran the plan's full standing gate
(recall CI, FP-on-noise hard gate, novel/TP ratio, elapsed) against the TRUE current
production baseline (`osd_depth: Some(0)`, i.e. order-0-only). This task (W2.4) is the first
to attempt that flip, and is therefore the first to run that comparison.

## Prerequisite work (landed regardless of this decision, see separate commits)

1. **DecodeBudget integration**: OSD's escalation ladder (order-1 -> order-2 -> order-3/npre2)
   now checkpoints against a `DecodeBudget` at each order boundary
   (`OsdDecoder::decode_with_features_scored_budgeted`), threaded from the coordinator's
   per-window budget through `DecodeContext`/`LdpcDecoder::with_budget`. Under
   `DecodeBudget::unlimited()` (every existing caller, including this task's own eval runs)
   this is a byte-identical no-op — proven by a dedicated unit test
   (`expired_budget_blocks_escalation_past_order_zero`) and by the fresh `compare` run below
   showing the ONLY config diff between baseline and variant is `osd_depth` itself.
2. **`npre2_residual_signature` bug fix**: see the separate experiment log
   `2026-07-07-w24-npre2-residual-signature-fix.md`. Independent of this decision (npre2
   stays disabled by default either way).

## Methodology

Reused the exact `eval`/`compare` pipeline this whole plan's other [A/B] tasks use (per
`research/README.md`), rather than only the `acceptance_calibration` script W2.1-W2.3 used —
that script measures OSD's own internal acceptance-scored rows, which is NOT the same
population as `Scorecard`'s `false_positives_total` (the actual post-pipeline decoded-message
count the plan's hard gate is defined against). Ran FOUR fresh eval invocations, all on the
current code state (this task's DecodeBudget-integration + npre2-fix changes already applied,
confirmed a no-op for `osd_depth: Some(0)` since neither touches that path):

```
cargo build --release -p pancetta-research --bin eval --bin compare
./target/release/eval --tier curated-hard-200 --mode ft8 --osd-depth 0 \
  --output research/scorecards/w24_flip1_hard200_baseline.json
./target/release/eval --tier curated-hard-200 --mode ft8 --osd-depth 2 \
  --output research/scorecards/w24_flip1_hard200_variant.json
./target/release/eval --tier noise_1000 --mode ft8 --osd-depth 0 \
  --output research/scorecards/w24_flip1_noise_baseline.json
./target/release/eval --tier noise_1000 --mode ft8 --osd-depth 2 \
  --output research/scorecards/w24_flip1_noise_variant.json
./target/release/compare research/scorecards/w24_flip1_hard200_baseline.json research/scorecards/w24_flip1_hard200_variant.json
./target/release/compare research/scorecards/w24_flip1_noise_baseline.json research/scorecards/w24_flip1_noise_variant.json
```

Corpus: full `curated-hard-200` (200 WAVs) + full `noise_1000` (1000 WAVs), same manifests
every other task in this plan uses. `--osd-depth 0` reproduces the literal current production
default (`Ft8Config::default().osd_depth == Some(0)`); `--osd-depth 2` is the candidate flip.
Everything else is `eval`'s (== production's) default config — the `compare` CONFIG DIFF
output (below) confirms `osd_depth` is the ONLY field that differs between the two runs.

Elapsed: hard-200 baseline 55.4s, hard-200 variant 89.1s, noise_1000 baseline 845.1s (~14.1
min), noise_1000 variant 961.0s (~16.0 min). Total wall time ~33 minutes.

## Results

### hard_200 (recall)

```
A: w24_flip1_hard200_baseline.json (score 0.3126)
B: w24_flip1_hard200_variant.json  (score 0.3138 +0.0012)

WINS:
  curated-hard-200      decode_rate   0.6252 -> 0.6277  (+0.0025)

REGRESSIONS:
  (none)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200   rec Δ=+5   (95% CI [+1.0, +10.0])  — significant
  curated-hard-200   novel Δ=+14 (95% CI [+7.0, +22.0]) — significant

CONFIG DIFF:
  decoder.decoder.osd_depth   0 -> 2
```

### noise_1000 (false positives — the plan's hard gate)

```
A: w24_flip1_noise_baseline.json (osd_depth=0): false_positives_total = 1  (1/1000 WAVs)
B: w24_flip1_noise_variant.json  (osd_depth=2): false_positives_total = 12 (11/1000 WAVs)

############################################################
# HARD GATE FAILURE — FALSE POSITIVES INCREASED ON NOISE TIER
# ANY increase disqualifies this change. See design spec §2 (D0).
############################################################
  noise_1000   false_positives_total   1 -> 12  (+11)
  noise_1000   noise_files_decoded     1 -> 11  (+10)
```

(`compare`'s own hard-gate detector — wired by an earlier task in this plan per
CLAUDE.md — fires automatically and prints this banner; not something this task's report is
asserting on its own initiative.)

### Elapsed (single-window wall time, informational — not itself pass/fail)

Using `pancetta-ft8/examples/profile_decode.rs` (already-committed harness,
`ABL_OSD_DEPTH` env var added this task for this measurement) on the profiling fixture
(`tests/fixtures/wav/wsjt/210703_133430.wav`, multi-thread):

```
osd_depth=0 (baseline): 141.94 ms/window, 120 msgs/15 iters (8 msgs/window, unchanged either way)
osd_depth=2 (variant):  253.15 ms/window, 120 msgs/15 iters
```

+78% single-window wall time on this fixture (which happens to show zero net message-count
change on THIS particular easy WAV — the corpus-level TP gain W2.1-W2.3 measured is
concentrated in genuinely marginal signals, not this one). This is a real, substantial cost:
under a `Standard` effort-preset budget (250ms, per the decoder-speed-overhaul plan's
Phase-3 presets), a window like this alone would consume essentially the entire budget,
leaving little room for S2+ (rest-candidate) processing on Moderate-tier hardware. The
DecodeBudget checkpointing landed as a prerequisite (see above) means this cost is at least
now correctly bounded by the operator's configured budget rather than an unconditional
addition — but it does not make the cost free, and it was never claimed to.

## Standing gate evaluation

| Criterion | Result | Verdict |
|---|---|---|
| Bootstrap-CI recall delta (hard-200) excludes zero, in favor | rec Δ=+5, CI [+1.0, +10.0] | **PASS** |
| Unverified-novel increase <= 2x verified-TP increase | novel Δ=+14 vs 2x5=10 allowed (2.8x) | **FAIL** |
| FP-on-noise = 0 new decodes | 1 -> 12 (+11), 1 -> 11 WAVs affected | **FAIL (hard gate)** |
| Elapsed hard gate | +78% single-window wall time on the profiling fixture (real, substantial, but boundable via DecodeBudget) | informational, not itself disqualifying, but corroborates the FP finding — this is a genuinely more expensive, less-precise mode |

Two of the four standing-gate criteria fail, one of them (`false_positives_total` on
noise_1000) an EXPLICIT, automated hard gate ("ANY increase disqualifies this change") wired
by an earlier task in this same plan specifically to catch exactly this scenario.

## Reconciling this against W2.1-W2.3's own narrative

W2.1 measured noise_1000 FPs collapsing 835 -> 12 (a 98.6% reduction) and treated 12 residual
FPs as a strong, if incompletely decomposed, win. That comparison was **pre-fix-OSD-2 vs
post-fix-OSD-2** (i.e. "how much did the W2.2 best-by-distance selection help, given OSD-2 is
already running") — it was never a comparison against the actual `osd_depth: Some(0)`
production baseline, which this task now measures directly for the first time and finds to be
much lower (1, not 0, but nowhere near 12). W2.2's own report explicitly flagged this exact
gap as an open, unresolved caveat ("Neither residual-FP decomposition... done — appropriately
flagged as a follow-up for whenever `osd_depth` is raised in production (i.e. relevant to
W2.4 next)"). This task closes that gap and the answer is: the 12 residual FPs, while a huge
improvement over the OLD first-CRC-accept mechanism's 835, are still an **11-decode net
regression** relative to what production actually does today. The W2.1-W2.3 acceptance-gate
threshold (`soft_distance <= 0.0976`, calibrated for <=1% FDR on a DEFINITIVE TP/FP population)
was never designed to drive the noise-tier absolute FP count to zero — it trades a bounded
FDR against a MUCH bigger implicit trial volume (up to 121,485 order-3 trials per BP-failed
candidate) than order-0 ever attempts, so some residual noise-tier false-accepts were always
going to survive even a well-calibrated gate. That tradeoff is not acceptable against this
plan's own explicit hard gate.

## Decision

**Flip 1 is DECLINED.** `Ft8Config::default().osd_depth` stays `Some(0)`. No production
behavior changes as a result of this task. Per the task brief's own guidance ("If it doesn't
pass: report DONE_WITH_CONCERNS with full data, do NOT force the flip"), this is reported
honestly rather than rationalized past the hard-gate failure — the recall gain (+5 verified
TP) is real and positive, but it is bought at a disqualifying cost (+11 new noise-tier
false positives, a 2.8x novel/verified-TP ratio, and a real ~78% single-window latency
increase on top).

Since Flip 1 did not land, **Flip 2 (`osd_depth: Some(3)` + npre2) was NOT evaluated** — the
task brief explicitly gates Flip 2's A/B on Flip 1 landing. The npre2_residual_signature bug
fix itself was still completed as an independent correctness fix (see the companion log) since
it stands on its own merits regardless of this decision.

## Files

- `research/scorecards/w24_flip1_hard200_baseline.json`, `w24_flip1_hard200_variant.json`
- `research/scorecards/w24_flip1_noise_baseline.json`, `w24_flip1_noise_variant.json`
- `research/scorecards/acceptance_calibration_w24_flip1.csv` (the acceptance_calibration
  reproduction run, confirming byte-for-byte the same 1256 TP / 4970 novel / 12 FP / 0.1902
  threshold W2.2 and W2.3 both reported, on the current code state post-DecodeBudget-
  integration — i.e. confirms that integration is a genuine no-op under
  `DecodeBudget::unlimited()`)

## Follow-up (not built here, flagged honestly)

The plan's own gating language treats FP-on-noise as an automated hard gate but doesn't
currently have a lever to trade a FEW more noise FPs for a recall gain when the recall gain
is real (the `Δrecall`/`ΔFP` ratio here — +5 verified TP for +11 noise FP — is a judgment call
this task is not authorized to make; the brief says decline on gate failure, not weigh
tradeoffs). If a future task wants to revisit this, the natural next lever is tightening
`max_soft_distance` further specifically to kill the noise-tier residual (which W2.1 already
flagged has a soft, overlapping boundary — noise minimum 0.0242 sits inside the verified-TP
range), rather than re-attempting this exact flip unchanged.
