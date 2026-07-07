# Costas half-loop disable (F5) — decoder-speed-overhaul Task 6: gate FAILED, default NOT flipped

**Date**: 2026-07-06
**Branch**: `worktree-decoder-speed-overhaul` (commit a7459d5e at measurement time)
**Status**: GATE FAILED. `Ft8Config::costas_half_loop_disabled` default stays `false`. No production
default was flipped by this task.

## What this was

Decoder-speed-overhaul plan Task 6 (F5, `[A/B]`): the plan's premise was that
"Batch 92 already argued redundancy at TIME_OSR ≥ 2" for the half-symbol
inner loop in `compute_costas_score_groups`, and that this task's A/B on
hard-200 would simply confirm that redundancy and let the default flip to
`true`.

Reading the actual Batch 92 record before running anything
(`pancetta-ft8/src/decoder.rs:661-680` doc comment,
`research/experiments/2026-06-12-batch-92.md`, hypothesis bank hb-251)
shows Batch 92 concluded the **opposite**: the half loop is provably
score-redundant (an executable plateau-identity assertion proves
`score(t0) == max(g(t0), g(t0+1))`), but disabling it is measurably
**harmful to recall** — on `raw_530_full` (200/2066 slots), Batch 92 measured
TP −64/−635 respectively, with the pre-registered ship test explicitly
**failing** on ΔTP. The field's own doc comment says "Keep this **false**"
and explains the mechanism: with NMS off (production default), the
plateau emits two sync candidates 960 samples apart per strong signal, and
the ±720-sample fine timing search only jointly covers the true alignment
through the *pair* — disabling the half loop removes that free
adjacent-alignment retry.

This task re-ran the A/B on the plan's actual gate corpus (hard-200, not
Batch 92's `raw_530_full`) to check whether that conclusion still holds
before deciding whether to flip the default. It does.

## Harness plumbing added

No `--costas-half-loop-disabled` eval flag existed yet (Batch 92 used
one-off examples: `pancetta-research/examples/batch92_costas_half_loop.rs`,
`batch88_dt_audit.rs --half-loop-off`). Added the standard eval-CLI
plumbing mirroring the `--pade-atanh` pattern from Task 5:

- `DecoderUnderTest::with_costas_half_loop_disabled` (`pancetta-research/src/decoder.rs`)
- `--costas-half-loop-disabled` flag + `Args::costas_half_loop_disabled` (`pancetta-research/src/bin/eval.rs`)

## Commands run (real CLI, `--help`-verified before use)

```bash
# Control (production defaults, costas_half_loop_disabled implicitly false)
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/costas-half-loop-control.json

# Variant: costas_half_loop_disabled = true
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --costas-half-loop-disabled \
    --output research/scorecards/costas-half-loop-variant.json

cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/costas-half-loop-control.json research/scorecards/costas-half-loop-variant.json
```

## Results

| | control | variant (`costas_half_loop_disabled = true`) |
|---|---:|---:|
| composite (raw) | 0.3138 | 0.2994 |
| composite (saturation-aware) | 0.3041 | 0.2897 |
| decode_rate | 0.6277 | 0.5987 (flagged REGRESSION, −0.0290) |
| elapsed_seconds | 48.8 | 44.6 (−8.6%) |

Bootstrap CI (n=1000, seed 0xb007):
- `rec` Δ=**−58**, 95% CI **[−80, −39]** — **significant regression**, entirely negative
- `novel` Δ=−103, 95% CI [−158, −41] — significant improvement (fewer FPs), but this does not
  rescue the gate — the plan's ΔFP ≤ 2×ΔTP criterion is about bounding acceptable *added* FPs per
  *gained* TP; here TPs are *lost*, not gained, so the criterion doesn't apply and can't offset the
  recall loss.

Gate criterion "recall within CI of baseline" **failed** — the CI excludes zero and the entire
interval is negative, consistent with (and slightly larger in relative terms than) Batch 92's
original `raw_530_full` measurement (−64 TP / 200 slots there vs −58 rec / 200 WAVs here — same
order of magnitude, different corpus, same mechanism).

## Decision

**Do not flip the default.** `Ft8Config::costas_half_loop_disabled` stays `false` in
`pancetta-ft8/src/decoder.rs:1252`; no source changes to the decoder's default or the pinned
`test_costas_half_loop_disabled_plateau_identity_and_sharpening` test (both already assert the
`false` default and remain accurate). The plan's Task 6 premise — that Batch 92 "argued
redundancy" in a way that supported flipping the default — conflated "score-redundant" with
"safe to disable"; Batch 92's own verdict already said the opposite ("Pre-registered ship test
failed on ΔTP → `costas_half_loop_disabled` stays default-OFF"). This task's hard-200 A/B
independently reconfirms that verdict; nothing here contradicts or supersedes hb-251
(SHELVED-MEASURED).

Kept as harness infrastructure (not reverted, mirrors the `--pade-atanh` precedent of keeping the
plumbing regardless of the A/B outcome): the new `--costas-half-loop-disabled` eval flag and
`with_costas_half_loop_disabled` builder, so a future TIME_OSR change or NMS-on configuration can
be re-A/B'd without re-adding plumbing.

## Counters

- Ships: none (no default flip; no pinned-test changes).
- Harness plumbing: `--costas-half-loop-disabled` eval CLI flag,
  `DecoderUnderTest::with_costas_half_loop_disabled` (`pancetta-research/src/decoder.rs`,
  `pancetta-research/src/bin/eval.rs`).
- Reconfirms: hb-251 (SHELVED-MEASURED, Batch 92) — no change to its status.
- Scorecards: `research/scorecards/costas-half-loop-control.json`,
  `research/scorecards/costas-half-loop-variant.json` (untracked, same convention as
  `pade-atanh-{control,variant}.json` in Task 5).
