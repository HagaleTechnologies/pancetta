# BP escalation ladder (S3) — decoder-speed-overhaul Task 10: implemented, GATE MARGINAL — default stays OFF

**Date**: 2026-07-06
**Branch**: `worktree-decoder-speed-overhaul`
**Status**: Continuation-equivalence PROVEN bit-exact (unit tests). A/B gate on
curated-hard-200: recall not significantly regressed, but elapsed improvement
is real yet far short of "strongly improved" (~5%, not the double-digit wins
of Tasks 5/7). Root-caused why (see below). `Ft8Config::escalation_enabled`
stays `false` by default — **not flipped**, per the task's explicit
instruction to report data rather than force a flip on an ambiguous gate.

## What this was

Decoder-speed-overhaul Task 10 (S3, `[A/B]`): a third candidate-decode tier.
Task 9 split candidates into an always-decoded "floor" and a budget-gated
"rest"; this task makes the *iteration budget itself* two-tiered. When
`escalation_enabled`, the S1/S2 candidate passes run BP at a shallow
`floor_iters` (25) instead of the full `ldpc_iterations`/`deep_iters` (100).
A candidate that fails to converge at 25 iterations, but isn't hopelessly far
off (`parity_errors <= escalation_parity_max` unsatisfied checks of 83), gets
its BP state (`c2v` + running posteriors — literally everything the layered
schedule carries between iterations) saved and **continued** to 100
iterations rather than re-decoded from scratch.

## Step 1-2: the continuation-equivalence proof (TDD)

The core mathematical claim: running layered BP for 25 iterations then
continuing for 75 more must be **bit-identical** to running it flat for 100
iterations, because the layered schedule's entire inter-iteration state is
`(c2v, running posteriors)` — nothing else survives across iterations.

**RED** (confirmed by temporarily renaming the two new methods and
re-running): `bp_continuation_equals_flat_run` and
`bp_continuation_matches_flat_run_when_it_converges` fail to compile —
`error[E0599]: no method named 'take_continuation'/'belief_propagation_continue'
found for struct 'decoder::LdpcDecoder'`.

**GREEN**: implemented by factoring the layered per-iteration body into
`LdpcDecoder::layered_bp_sweep` (called identically by both the flat run and
the continuation), then:
- `belief_propagation_with_features_capturing(llrs, want_continuation: bool)`
  — the renamed/extended original `belief_propagation_with_features`, which
  now optionally exports `(c2v, total)` as `LayeredBpState` on failure (zero
  extra cost when `want_continuation = false` — the existing callers all
  pass `false` via the now-thin `belief_propagation_with_features` wrapper).
- `BpContinuation` struct + `LdpcDecoder::belief_propagation_continue(&mut
  cont, additional_iters) -> ([f32;174], Option<Box<[[f32;174];25]>>, (u8,
  f32))` (the plan's exact required signature).
- Both `bp_continuation_equals_flat_run` (garbage LLRs that never converge —
  the real escalation-failure shape) and a companion test on the
  eventually-converges path pass **bit-exactly** (`assert_eq!`, not
  approximate).

Adapted from the plan brief's illustrative `BpContinuation` shape: this task's
wiring escalates *inline* within `par_decode_candidate` (see Step 3 scope
below), so the brief's `candidate: CostasCandidate` field (needed for
out-of-context re-emission by a deferred global stage) isn't required — the
calling context already has the candidate in scope. Added `floor_iters_done:
usize` (not in the brief) so `belief_propagation_continue` numbers
`iters_used` and trajectory rows correctly relative to a flat run in the
general case (`floor_iters != 25`); with the shipped default (`floor_iters ==
25 == trajectory depth`) this degenerates to a no-op passthrough.

## Step 3: wiring — scoped to the standard (non-AP, non-fine-timing) trial

`par_decode_candidate`'s "standard attempt" (spectrogram-based, 2 `freq_sub`
trials) now takes an `Option<&LdpcDecoder>` deep-decoder parameter and, when
`ctx.escalation_enabled`, calls the new
`LdpcDecoder::decode_soft_with_escalation(llrs, deep, escalation_parity_max)`
in place of `ldpc.decode_soft(llrs)`. That method reuses
`LdpcDecoder::post_bp_pipeline` (the CRC-adjacent OSD/neural-OSD/feedback-
refinement/research-capture logic, extracted from `decode_soft_with_features`
so it isn't duplicated) for **both** the floor attempt and the escalated
(deep) attempt — genuinely the same post-BP path, not a parallel copy.

**Deliberate scope limit** (flagged per the task's "STOP if this turns into a
large refactor" guidance): escalation only touches the standard freq_sub-
trial path. The fine-timing FFT fallback (21 trials/candidate) and the AP0-
failed AP-injection path (`par_try_ap_decode` + its 3 helper functions) are
**not** wired to escalation — reusing their message-assembly tails would have
required a much larger refactor (return-type changes rippling through
`map_init`/`par_chunks`/candidate-dump instrumentation) for what both those
paths already treat as an expensive, rarely-reached fallback. `ldpc_init`
gains a 5th tuple slot (`Option<LdpcDecoder>`, the deep decoder, built only
when `escalation_enabled`) alongside the existing low/mid/high adaptive-
iteration decoders; the shallow floor decoders replace `ldpc_iterations` with
`floor_iters` when escalation is on (taking precedence over the orthogonal,
also-off-by-default `adaptive_ldpc_iters` — an unmeasured combination).

**Regression invariant**: `escalation_enabled = false` (default) never
constructs a deep decoder and never calls the new methods — `par_decode_
candidate`'s standard trial is exactly `ldpc.decode_soft(&llrs)`, byte-
identical to pre-Task-10. Full `cargo test -p pancetta-ft8 --features
transmit --lib` — 437/437 passing (435 pre-existing + 2 new), confirming no
regression.

## Step 4: measurement — `escalation_parity_max` histogram

Instrumented `LdpcDecoder::decode_soft_with_escalation` (env-var-gated
`escalation_instrument`, mirrors the existing `bicm_id_instrument` pattern —
zero cost when `PANCETTA_ESCALATION_INSTRUMENT_FILE` is unset) to log
`<parity_errors> <ok|fail>` for every candidate escalated from floor to deep.
Ran with the ladder forced wide open (`--escalation-parity-max 83`, i.e.
escalate every floor failure) on curated-hard-200:

```bash
PANCETTA_ESCALATION_INSTRUMENT_FILE=/tmp/pancetta-escalation-instrument.log \
  ./target/release/eval --tier curated-hard-200 --mode ft8 \
  --escalation-enabled --floor-iters 25 --deep-iters 100 --escalation-parity-max 83 \
  --output research/scorecards/escalation-measurement.json
```

44,912 floor-failures escalated; 171 went on to succeed by 100 iterations
(CRC-passing). Cumulative distribution of parity-error count at floor among
those 171 successes:

| parity_errors <= | successes captured | cumulative % |
|---:|---:|---:|
| 10 | 76 | 44.4% |
| 15 | 141 | 82.5% |
| 20 | 161 | 94.2% |
| 24 | 167 | 97.7% |
| 25 | 168 | 98.3% |
| **30** | **170** | **99.4%** |
| 37 | 171 | 100.0% |

**`escalation_parity_max = 30`** is the smallest threshold clearing the
">=99%" bar — not 25, a natural but wrong first guess (floor_iters is also
25, but that's a coincidence of notation, not a reason the thresholds should
match). Shipped as the new default (`Ft8Config::escalation_parity_max = 30`,
overriding the `24` placeholder used during initial scaffolding).

## Step 5: A/B gate — recall holds, but elapsed win is real yet weak

```bash
./target/release/eval --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/escalation-ladder-control.json
./target/release/eval --tier curated-hard-200 --mode ft8 --escalation-enabled \
    --output research/scorecards/escalation-ladder-variant.json
./target/release/compare research/scorecards/escalation-ladder-control.json \
    research/scorecards/escalation-ladder-variant.json
```

| | control (flat 100) | variant (floor 25 / deep 100, escalation_parity_max=30) |
|---|---:|---:|
| composite (saturation-aware) | 0.3021 | 0.3019 |
| decode_rate | 0.6237 | 0.6232 |
| wall time (repeated runs) | 54.8-55.0s | 52.2-52.4s (**-4.9%**) |

Bootstrap CI (n=1000, seed 0xb007):
- `rec` Δ=-1, 95% CI **[-3, 0]** — not significant (upper bound is exactly 0,
  a borderline pass)
- `novel` Δ=+2, 95% CI [0, +5] — not significant

Recall gate: **passes** (not significant). Elapsed gate: **-4.9%, real but
not "strongly improved"** (Tasks 5/7 landed -16% and better). Investigated
why with two follow-up runs:

1. **Root cause — BP iteration count is a small fraction of total decode
   cost on this corpus.** Ran `--escalation-parity-max 0` (pure `floor_iters
   =25`, escalation never triggers — the theoretical *ceiling* on savings
   from cutting iterations alone): **52.0s**, essentially identical to the
   escalation_parity_max=30 variant's 52.2-52.4s. Cutting BP's iteration cap
   4x (100→25) with *zero* escalation buys the same ~5% this task's shipped
   config buys — the escalation ladder itself contributes almost nothing
   incremental on top of the floor cut. This means BP's own iteration loop
   is simply not the dominant cost in this pipeline: Costas sync candidate
   search across the whole passband, per-candidate spectrogram/FFT symbol
   extraction (2 freq_sub trials, up to 21 fine-timing FFT trials when
   `sync_score >= 3.5`), and message parsing dwarf the ~kilobyte-scale
   LDPC(174,91) sum-product update (83 checks, degree <= 7) even at 100
   iterations. BP already exits early on convergence for the common
   (converges-quickly) case, so the 100-vs-25 cap only ever mattered for
   candidates that fail outright — and even fully eliminating their wasted
   iterations barely moves the needle.
2. **Root cause — the recall-preserving threshold captures almost all
   failures anyway, not just the true near-misses.** Of the same 44,912
   floor-failures logged above, **44,814 (99.8%) have parity_errors <= 30** —
   the same threshold chosen to keep 99% of eventual *successes*. There is no
   clean separation between "near-miss, will succeed" and "hopeless" by
   parity-error-count-at-25 alone on this corpus: most candidates, real or
   noise, land in a broad 5-30 unsatisfied-check band at iteration 25.
   Any threshold tight enough to meaningfully cut the escalated set (e.g.
   `<=10`, capturing only 44% of true successes) still escalates ~13,191/
   44,912 (29%) of all failures — a real recall/speed tradeoff exists, but
   there is no free lunch at the "keep 99% of recoveries" operating point
   this task's Step 4 methodology targets.

## Decision

**Do not flip `escalation_enabled` to `true`.** The implementation is
correct (bit-exact continuation, proven byte-identical when off, zero
regressions across the full test suite) and the measured elapsed change is a
genuine small improvement, not a regression — but it falls well short of
"strongly improved," and the investigation above shows *why* further
threshold tuning won't fix it: the ceiling on savings from this mechanism
alone (pure floor cut, no escalation) is the same ~5% the shipped config
already gets. Reported per the task's explicit instruction: "if it fails,
investigate... if the gate fails, report DONE_WITH_CONCERNS with the data
rather than forcing a flip" — this task doesn't have an obvious fallback
variant the way Task 5 had piecewise-Padé.

**Follow-up flagged, not built**: if BP iteration cost is not the bottleneck,
the bigger win in this candidate-decode pipeline is more likely in the
Costas sync / spectrogram-extraction / fine-timing-FFT layer (already
partially addressed by prior Phase-1 tasks: F3 spectrogram flattening, F4
f32 real-FFT, F5 Costas half-loop). The escalation ladder machinery
(`BpContinuation`, `belief_propagation_continue`, `decode_soft_with_
escalation`) is shipped, tested, and available (default off) — an operator
or a future task could still enable it for whatever modest win it does offer,
once combined with a bigger structural change elsewhere, but it is not, on
its own, the double-digit win this phase's other [A/B] tasks delivered.

## Counters

- Ships (default OFF): `Ft8Config::{escalation_enabled, floor_iters
  (consumed for the first time), deep_iters, escalation_parity_max}`;
  `LdpcDecoder::{layered_bp_sweep, belief_propagation_with_features_
  capturing, belief_propagation_continue, decode_soft_with_escalation,
  post_bp_pipeline (extracted, no behavior change)}`; `LayeredBpState`,
  `BpContinuation`, `merge_trajectory` (decoder.rs); `escalation_instrument`
  (env-var-gated measurement helper, mirrors `bicm_id_instrument`);
  `par_decode_candidate`'s new `deep: Option<&LdpcDecoder>` parameter
  (standard-trial path only); research-harness `--escalation-enabled
  /--floor-iters/--deep-iters/--escalation-parity-max` flags +
  `DecoderUnderTest::with_{escalation_enabled,floor_iters,deep_iters,
  escalation_parity_max}` (pancetta-research).
- Unit tests: 2 new (`bp_continuation_equals_flat_run` — the never-converges
  case; `bp_continuation_matches_flat_run_when_it_converges` — the
  eventually-converges case), both bit-exact (`assert_eq!`, no tolerance).
  Full `pancetta-ft8` suite: 437/437 lib tests green (435 pre-existing + 2
  new), zero regressions.
- Scorecards: `research/scorecards/escalation-ladder-{control,variant}.json`.
  The wide-open measurement run and the escalation_parity_max=0 ceiling probe
  were investigation-only and not committed as scorecard files (their
  numbers are recorded in this journal instead, per the pade-atanh journal's
  precedent).
