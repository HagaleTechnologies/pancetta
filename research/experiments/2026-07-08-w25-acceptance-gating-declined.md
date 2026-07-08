# Replace blunt post-CRC gates with the acceptance metric — A/B against the TRUE production baseline — DECLINED (Workstream 2, Task W2.5)

**Date**: 2026-07-08
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: [A/B] — **DECLINED**. `Ft8Config::default().acceptance_gating_enabled` stays `false`.

## What this is

W2.1 built a signal-domain acceptance metric (`AcceptanceScore.soft_distance`, `acceptance::score()`,
threshold `soft_distance <= 0.0976` AND `hard_errors <= 37`, calibrated at FDR<=1% —
`research/experiments/2026-07-07-acceptance-calibration.md`). W2.2-W2.4 used it to gate OSD's own
order-1/2/3 candidate search. This task (W2.5) targets a DIFFERENT gate: the BP/AP decode paths'
blunt post-CRC confidence floors (`MIN_DECODE_CONFIDENCE = 0.41` in the non-AP path,
`MIN_AP_CONFIDENCE = 0.55` in the AP1-4/recent-only path), which reject any CRC-valid decode below
a sync-score-derived confidence threshold regardless of how clean the actual codeword match is. The
idea: a CRC-valid decode at low sync score (the sync search itself admits candidates down to
`MIN_SYNC_SCORE = 3.0`, well below the `MIN_DECODE_CONFIDENCE` floor's implied sync ~4.92) but a
genuinely tight acceptance match is probably real and shouldn't be blindly rejected.

Per the prior task's (W2.4) finding — measuring against the TRUE current-default baseline matters,
not a comparison between two already-modified variants — this task ran the full standing-gate
methodology from the start.

## Current gate structure (corrects the task brief's stale line numbers)

The brief's stale snapshot cited a serial `try_ap_decode`/`decoder.rs:7186-7201`/`4649` structure and
assumed W1.6's deletion of the dead `decode_candidate` twin left "one gate-change site." Investigated
directly against the current code (re-grepped, not trusted from the brief):

- **`try_ap_decode` / `try_ldpc_with_ap`** (self-methods on `Ft8Decoder`, serial path) are **fully
  dead code** — no call site anywhere in `decoder.rs`, `pancetta-ft8/tests/`, or
  `pancetta-research/examples/` calls them (confirmed by exhaustive grep). They compile without a
  `dead_code` warning only because `pancetta-ft8/src/lib.rs` carries a crate-wide
  `#![allow(dead_code, unused_imports)]`. **Not touched by this task** — modifying genuinely
  unreachable code would add untested surface for zero production effect.
- **The actual live gate structure is 4 sites, not 1 or 2**, split across two code path families:
  1. `par_decode_candidate` (non-AP / "AP0-equivalent" path) — **two** internal gate sites: the
     coarse spectrogram-based trial (with the BICM-ID rescue) and the fine-FFT fallback trial. Both
     use `MIN_DECODE_CONFIDENCE = 0.41`.
  2. `par_try_ldpc_with_ap` (AP1-4) — one gate site, `MIN_AP_CONFIDENCE = 0.55` (relaxed to
     `MIN_DECODE_CONFIDENCE = 0.41` when the existing a8 template-match fires).
  3. `par_try_ldpc_with_recent_only` (AP2-class, my_call-less/hb-043 recent-caller path) — one gate
     site, `MIN_AP_CONFIDENCE = 0.55`, no existing a8-style relaxation.

  W1.6 DID unify the serial/parallel *non-AP* twins into the single `par_decode_candidate` — that
  part of the brief's premise was correct — but it did not collapse the AP-path gates, and the
  non-AP path itself was always two sites (coarse + fine-FFT fallback), not one.

## Implementation

Added `Ft8Config::acceptance_gating_enabled: bool` (default `false`, plain struct field — not a
`pancetta-config::ConfigSection`, confirmed no `merge_with` needed) and a `DecodeContext` field of
the same name threaded through the one production ctx-construction site. A shared private helper,
`passes_acceptance_gate(enabled: bool, score: Option<AcceptanceScore>) -> bool`, encapsulates the
W2.1 calibration bar (`soft_distance <= 0.0976 && hard_errors <= 37`) and is consulted at all 4 live
sites:

- **Non-AP (`par_decode_candidate`, both trials)**: when a decode cleanly passes acceptance, the
  `MIN_DECODE_CONFIDENCE` floor AND the suspicion-scrutiny check are both skipped entirely —
  accepted regardless of sync score. A decode that does NOT cleanly pass acceptance falls through to
  the unchanged legacy floor + suspicion gate.
- **AP1-4 (`par_try_ldpc_with_ap`) and recent-only (`par_try_ldpc_with_recent_only`)**: a clean
  acceptance pass (scored against `base_llrs`, the PRE-injection channel evidence — independent of
  the AP bias itself) relaxes the floor from `MIN_AP_CONFIDENCE` down to `MIN_DECODE_CONFIDENCE` (NOT
  a full drop — AP injection still biases the LDPC prior, so the baseline floor stays as a backstop)
  and skips suspicion scrutiny, mirroring the existing a8 template-match precedent already in
  `par_try_ldpc_with_ap`.

`acceptance_gating_enabled == false` forces `passes_acceptance_gate` to always return `false`
regardless of the score, so every touched site is byte-identical to pre-W2.5 behavior when the flag
is off (verified by the noise-tier and hard-200 config-diff below showing `acceptance_gating_enabled`
as the ONLY differing field, and hard-200's baseline run reproducing this task's own
`decode_rate`/`novel_decodes` unchanged from `main.json`'s regression-flags computation).

Wired into the research eval harness: `pancetta-research::decoder::Ft8Decoder::with_acceptance_gating_enabled(bool)`
builder + `eval`'s `--acceptance-gating`/`--no-acceptance-gating` CLI flags (mirrors the existing
`--llr-whitening`/`--no-llr-whitening` pattern).

## TDD evidence

Three tests in `pancetta-ft8/src/decoder.rs::w2_5_acceptance_gating_tests` (all currently passing —
GREEN post-implementation; the flag-off test doubles as the "confirm current/pre-W2.5 behavior"
sanity check since that code path is provably unchanged):

1. **`low_sync_true_signal_rejected_with_gating_disabled`**: a REAL, clean (full-amplitude,
   noiseless) encoded+modulated "CQ K5ARH EM10" signal, located via the actual Costas sync search
   (asserted `sync_score > 6.0` naturally — a genuine strong signal), then its candidate's
   `sync_score` field alone is overridden to `4.0` (confidence 0.333, inside the design doc's
   targeted 3.0-4.92 sync band) before calling `par_decode_candidate` directly. Only the
   `sync_score` FIELD is synthetic — `freq_bin`/`time_step`/`freq_sub` are the real coordinates, so
   BP/CRC genuinely run over the real signal. Models a real signal whose Costas-preamble correlation
   happens to be weak while its data-bearing symbols stay clean. With `acceptance_gating_enabled:
   false`, asserts the decode is `None` — confirms the legacy blunt-floor rejection is preserved.
2. **`low_sync_true_signal_accepted_with_gating_enabled`**: same construction, flag `true` — asserts
   the decode is `Some`, text matches, and `acceptance.soft_distance <= 0.0976` (a genuinely clean
   match, not a hand-waved pass).
3. **`low_sync_bad_acceptance_still_rejected`** (negative case): reuses the exact weight-4 CRC-14
   kernel element (`[0, 3, 37, 66]` on the all-zero payload) already proven in `osd.rs`'s
   `test_osd_rejects_untrustworthy_order1_collision_and_finds_truth_at_order3` to build TWO
   independently CRC-14-valid LDPC codewords sharing the same CRC bits. Channel LLRs are constructed
   to confidently match codeword A; codeword B (the CRC-14-collision partner) is scored against them
   via the real `acceptance::score` function and shown to have `soft_distance` far above 0.0976 (a
   genuinely bad match, not typed-in). Confirms `passes_acceptance_gate(true, Some(bad_score)) ==
   false` — the exact predicate all 4 production gate sites consult.

**Scope note on test 3** (documented in-code and here, not glossed over): this is a metric+gate-level
test, not a full rayon-pipeline integration like tests 1-2. Forcing BP itself to *converge* to a
specific wrong-but-CRC-valid codeword through the full decode pipeline isn't practically
constructible — that would require either a genuine CRC-14 noise collision (BP essentially never
produces one on random/noise data; convergence needs real supporting evidence, unlike OSD's
order-limited exhaustive trial search) or AP-bias steering (already covered by the separate, existing
`ap_injection_survived` check this task doesn't touch). What's tested directly is exactly what every
one of this task's 4 call sites actually consults.

Full `pancetta-ft8` suite: 467 unit tests green (up from 464 baseline + 3 new), all integration test
binaries green, `cargo fmt`/`clippy` clean on touched files.

## A/B methodology

Reused the exact `eval`/`compare` pipeline (per `research/README.md` and W2.4's precedent), measuring
against the TRUE current production default (`acceptance_gating_enabled: false`), not a comparison
between two already-modified variants:

```
cargo build --release -p pancetta-research --bin eval --bin compare
./target/release/eval --tier curated-hard-200 --mode ft8 --no-acceptance-gating \
  --output research/scorecards/w25_hard200_baseline.json
./target/release/eval --tier curated-hard-200 --mode ft8 --acceptance-gating \
  --output research/scorecards/w25_hard200_variant.json
./target/release/eval --tier noise_1000 --mode ft8 --no-acceptance-gating \
  --output research/scorecards/w25_noise_baseline.json
./target/release/eval --tier noise_1000 --mode ft8 --acceptance-gating \
  --output research/scorecards/w25_noise_variant.json
./target/release/compare research/scorecards/w25_hard200_baseline.json research/scorecards/w25_hard200_variant.json
./target/release/compare research/scorecards/w25_noise_baseline.json research/scorecards/w25_noise_variant.json
```

Corpus: full `curated-hard-200` (200 WAVs) + full `noise_1000` (1000 WAVs) — the same manifests every
other task in this plan uses. Everything else is `eval`'s (== production's) default config; `compare`'s
CONFIG DIFF output confirms `acceptance_gating_enabled` is the ONLY field that differs between the two
runs in both comparisons.

Elapsed: hard-200 baseline 50.8s, hard-200 variant 50.4s (no measurable cost — `acceptance::score`
is a cheap linear scan over 174 floats, and it was already being computed unconditionally
pre-W2.5, just after the gate instead of before). noise_1000 baseline 768.5s (~12.8 min), noise_1000
variant 777.7s (~13.0 min). Total wall time ~28 minutes.

## Results

### hard_200 (recall) — ZERO measured effect

```
A: w25_hard200_baseline.json (score 0.3126)
B: w25_hard200_variant.json  (score 0.3126 +0.0000)

REGRESSIONS:
  (none)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200   rec Δ=+0   (95% CI [+0.0, +0.0])   — NOT significant
  curated-hard-200   novel Δ=+0 (95% CI [+0.0, +0.0])   — NOT significant

CONFIG DIFF:
  decoder.decoder.acceptance_gating_enabled   false -> true
```

Direct scorecard comparison (not just the composite score) confirms this is not just "no *net*
change" but **bit-for-bit identical decode output**: `decode_rate` 0.6251874062968515 ==
0.6251874062968515, `truth_decodes_recovered` 1251 == 1251, `novel_decodes` 4956 == 4956
(`novels_verified` 3161 == 3161, `novels_unverified` 1795 == 1795) — not a single decode differs
between baseline and variant on the full curated-hard-200 corpus.

**Why zero, not the expected sync-3.0-to-4.92-band gain**: the sync search does admit candidates down
to `MIN_SYNC_SCORE = 3.0` (well below the floor's implied sync ~4.92 confidence threshold), and those
candidates DO reach BP/CRC in `par_decode_candidate` — the gate only fires post-CRC. On this 200-WAV
corpus, evidently no candidate in that low-sync band that reaches a genuine CRC pass also has a clean
acceptance score (or, more likely given the noise-tier evidence below, none reach CRC pass there at
all with REAL signal support) — the noise-tier result below proves the mechanism is live and firing
somewhere, just not on any real hard-200 signal.

### noise_1000 (false positives — the plan's hard gate) — REGRESSION

```
A: w25_noise_baseline.json (acceptance_gating_enabled=false): false_positives_total = 1  (1/1000 WAVs)
B: w25_noise_variant.json  (acceptance_gating_enabled=true):  false_positives_total = 10 (9/1000 WAVs)

############################################################
# HARD GATE FAILURE — FALSE POSITIVES INCREASED ON NOISE TIER
# ANY increase disqualifies this change. See design spec §2 (D0).
############################################################
  noise_1000   false_positives_total   1 -> 10  (+9)
  noise_1000   noise_files_decoded     1 -> 9   (+8)
```

(`compare`'s own automated hard-gate detector fires this banner; not something asserted by this
report on its own initiative.)

## Standing gate evaluation

| Criterion | Result | Verdict |
|---|---|---|
| Bootstrap-CI recall delta (hard-200) excludes zero, in favor | rec Δ=+0, CI [+0.0, +0.0] — literally zero, not even a non-significant positive trend | **FAIL** |
| Unverified-novel increase <= 2x verified-TP increase | Δverified-TP=0, Δunverified-novel=0 — vacuously satisfied, but irrelevant given zero gain | N/A |
| FP-on-noise = 0 new decodes | 1 -> 10 (+9), 1 -> 9 WAVs affected | **FAIL (hard gate)** |
| Elapsed hard gate | No measurable cost on either tier | PASS (moot given the above) |

## Decision

**DECLINED.** `Ft8Config::default().acceptance_gating_enabled` stays `false`. No production behavior
changes as a result of this task.

This is an even more clear-cut decline than W2.4's (which at least found a real +5 TP recall gain to
weigh against its FP regression): here there is **zero recall benefit on hard-200 and a real,
unambiguous cost on noise_1000** (+9 false positives, a 10x increase over baseline). There is no
tradeoff to weigh — the change is pure downside on the measured corpora. Per the task brief's own
guidance ("Flip default to true ONLY on pass... If it fails, do NOT force the flip") and mirroring
W2.4's precedent of declining on a real FP regression, this is reported honestly rather than
rationalized past two failing standing-gate criteria.

Hard-1000 (the plan's escalation path for flips claiming <1% effects) was not additionally run: the
hard-200 result is not a small positive effect that might need a larger corpus to resolve — it is
exactly zero, and the FP-on-noise hard gate already independently disqualifies the change regardless
of what a larger recall corpus might show.

## Files

- `research/scorecards/w25_hard200_baseline.json`, `w25_hard200_variant.json`
- `research/scorecards/w25_noise_baseline.json`, `w25_noise_variant.json`

## Follow-up (not built here, flagged honestly)

The zero-effect result on hard-200 suggests the premise motivating this task ("candidates the sync
search admits at 3.0-4.92 but the floor re-rejects, that would otherwise cleanly pass CRC+acceptance")
may not hold in practice on this corpus — a real signal weak enough to score sync<4.92 plausibly
rarely produces a clean enough overall LLR field to both pass CRC AND clear the tight 0.0976
acceptance bar. If a future task wants to revisit the underlying idea, the natural next step is a
diagnostic run (dumping every low-sync CRC-valid candidate's acceptance score, pass or fail, similar
to the W2.1 calibration methodology) to characterize how often the target scenario even occurs on a
larger/different corpus, rather than re-attempting this exact flip unchanged.
