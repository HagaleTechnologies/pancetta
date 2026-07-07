# Padé fast_atanh (F1) — decoder-speed-overhaul Task 5: piecewise SHIPS, raw Padé SHELVED

**Date**: 2026-07-06
**Branch**: `worktree-decoder-speed-overhaul` (commit 69190760 at measurement time)
**Status**: GATE PASSED (piecewise variant) — `Ft8Config::pade_atanh` default flipped to `true` in a follow-up commit. The raw (unconditional) Padé variant FAILED the gate on recall and is not shipped.

## What this was

Decoder-speed-overhaul plan Task 5 (F1, `[A/B]`): swap `fast_atanh`'s exact
`0.5·ln((1+x)/(1-x))` in the LDPC sum-product check-node update
(`2·atanh(∏ tanh(v/2))`) for ft8_lib's Padé rational approximant
(`vendor/ft8_lib/ft8/ldpc.c`, MIT) — division/multiply only, no `ln` call,
on the hottest BP inner loop. Gated behind `Ft8Config::pade_atanh` (default
`false`) per the plan's [A/B] discipline: implement off, validate on the
research-harness gate corpus, only then flip the default.

## Harness setup note

This worktree's `research/baselines/ft8/` cache (gitignored, machine-local
per `.gitignore` — not committed) was empty on first run, so the initial
`eval` invocation scored `truth_decodes_total: 0` for both arms (a
no-op comparison — not a real gate). Populated it by rsyncing the plain
`<sha>.json` jt9-baseline files from the primary checkout
(`/Users/thagale/Code/pancetta/research/baselines/ft8/`) into this
worktree's `research/baselines/ft8/` (48 of the 200 hard-200 WAVs have a
cached jt9 baseline in this environment — the same partial coverage the
primary checkout has; both arms below read the identical 48-WAV truth
subset, so the A/B comparison is apples-to-apples even though absolute
recall percentages are relative to that partial ground truth).

## Commands run (real CLI, `--help`-verified before use)

```bash
# Control (production defaults, pade_atanh implicitly false)
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/pade-atanh-control.json

# Variant 1: raw Padé everywhere
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --pade-atanh \
    --output research/scorecards/pade-atanh-variant.json

# (after implementing the piecewise fallback, re-run variant with the
#  SAME --pade-atanh flag — now routes through fast_atanh_pade_piecewise)
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --pade-atanh \
    --output research/scorecards/pade-atanh-variant.json

cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/pade-atanh-control.json research/scorecards/pade-atanh-variant.json
```

`--pade-atanh` and the `DecoderUnderTest::with_pade_atanh` builder are new
plumbing added in this task (`pancetta-research/src/bin/eval.rs`,
`pancetta-research/src/decoder.rs`) mirroring the existing `--layered-bp`
pattern — the eval CLI had no generic escape hatch for an arbitrary
`Ft8Config` bool, each flag is wired by hand.

## Results

### Attempt 1: raw Padé (`fast_atanh_pade`) unconditionally — FAILED the gate

| | control | variant (raw Padé) |
|---|---:|---:|
| composite (saturation-aware) | 0.3041 | 0.3011 |
| decode_rate | 0.6277 | 0.6217 (flagged REGRESSION, -0.0060) |
| elapsed_seconds | 58.3 | 46.8 (-19.7%) |

Bootstrap CI (n=1000, seed 0xb007):
- `rec` Δ=**-12**, 95% CI **[-20, -5]** — **significant regression**
- `novel` Δ=-51, 95% CI [-67, -37] — significant improvement (fewer FPs)

Gate criterion "recall within CI of baseline" **failed** — the CI excludes
zero and the whole interval is negative. This is the accuracy/stability
tradeoff the `fast_atanh_pade` doc comment already flagged: the Padé
saturates near |x|→1 (bounded ≈2.28) vs the exact ln form's ≈8.4 at the
practical BP clamp, and a real (if minority) fraction of BP check-node
products in this corpus land close enough to that boundary that the lost
confidence measurably costs converged codewords.

### Fallback: piecewise (Padé for |x| ≤ 0.95, exact ln form above) — PASSED the gate

Per the plan's explicit fallback instruction. Implemented
`fast_atanh_pade_piecewise` (`pancetta-ft8/src/decoder.rs`) and re-ran the
identical `--pade-atanh` invocation (now routing through the piecewise
function instead of the raw Padé):

| | control | variant (piecewise) |
|---|---:|---:|
| composite (saturation-aware) | 0.3041 | 0.3041 (identical) |
| decode_rate | 0.6277 | 0.6277 (no regression flagged) |
| elapsed_seconds | 58.3 | 49.0 (**-16.0%**) |

Bootstrap CI (n=1000, seed 0xb007):
- `rec` Δ=**+0**, 95% CI **[-4, +4]** — **not significant** (recall within CI of baseline)
- `novel` Δ=-11, 95% CI [-22, -2] — significant improvement (fewer FPs; ΔFP ≤ 2×ΔTP trivially holds since ΔTP=0 and ΔFP is negative)

All three gate criteria met: recall within CI of baseline, ΔFP ≤ 2×ΔTP,
elapsed improved (-16.0% wall on the hard-200 tier's full pipeline,
consistent with — smaller than, because the fallback branch pays the exact
`ln` call on the rare near-saturation edges — the -26.7% single-thread
`profile_decode` fixture measurement below).

## profile_decode benchmark (informational only, not the gate)

Single-thread (`RAYON_NUM_THREADS=1`), `pancetta-ft8/tests/fixtures/wav/wsjt/210703_133430.wav`,
10 iterations, production `Ft8Config` defaults otherwise:

| pade_atanh | ms/window | decode count |
|---|---:|---:|
| false (baseline) | 252.10 | 8 |
| true (raw Padé, measured before the piecewise fallback existed) | 184.79 | 8 |

**-26.7%**, decode count unchanged on this single fixture — this measured
the raw Padé's speed (piecewise wasn't re-benched here since the harness
gate above, not this fixture, is the binding measurement per the plan;
the piecewise fallback only adds cost on the rare |x| > 0.95 edges, so the
single-thread win is expected to be close to this number in practice).

## Decision

Ship `fast_atanh_pade_piecewise` as the `pade_atanh = true` implementation
(NOT the raw Padé — that variant is dead code reachable only by internal
call from the piecewise wrapper, not independently selectable). Flip
`Ft8Config::default().pade_atanh` to `true` in a follow-up commit citing
this journal.

## Counters

- Ships: `Ft8Config::pade_atanh` (default flipped true, this batch),
  `fast_atanh_pade` + `fast_atanh_pade_piecewise` (decoder.rs),
  `LdpcDecoder::with_pade_atanh`, `DecodeContext::pade_atanh`,
  `--pade-atanh` eval CLI flag + `DecoderUnderTest::with_pade_atanh`
  (research harness plumbing).
- Unit tests: 2 new (`pade_atanh_matches_ln_form_in_bp_operating_range`,
  `pade_atanh_piecewise_matches_ln_form_beyond_the_saturation_boundary`).
  Full `pancetta-ft8` suite green throughout (431+2 lib tests).
- Shelved: raw unconditional Padé (`fast_atanh_pade` called directly from
  BP) — measurably regresses recall on hard-200; kept as a private helper
  only, not exposed as a selectable mode.
- Scorecards: `research/scorecards/pade-atanh-control.json`,
  `research/scorecards/pade-atanh-variant.json` (the piecewise result —
  overwritten in place after the raw-Padé attempt was refuted; the raw
  numbers are recorded in this journal instead of a second scorecard file).
