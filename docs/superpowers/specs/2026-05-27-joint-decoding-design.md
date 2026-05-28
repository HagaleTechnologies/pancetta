# Joint multi-candidate decoding — design spec

**Status:** proposed (design before implementation; batch 13 iter 7)
**Hypothesis:** hb-086 (NEW this batch)
**Author:** research harness, 2026-05-27
**Estimated effort:** 3-5 sessions

## Why this is the next-tier lever

After hb-079 (coherent iterative-subtract multi-pass) graduated at
N=3 (composite +0.009212 + +0.000935 from hb-080), the post-hb-079
follow-up surface is closed:

- hb-081 MRC subtract — regresses −170 hard-200 (under-subtract blocks
  multipass).
- hb-082 residual sync threshold — 0 effect (not binding).
- hb-085 cross-cycle on residual — structurally redundant, shelved
  before implementation.

The remaining recall wall has a clear shape. From main.json's
hard-200 `per_wav_top_failures`:

| metric | value |
|---|---:|
| top-20 worst WAVs total truth | 1214 |
| top-20 recovered | 518 |
| top-20 missed | **696** (60% miss rate on these WAVs) |
| top-20 share of all hard-200 misses | **17%** |
| top-20 WAVs all have jt9 = truth | yes (WSJT-X recovers all) |

These 20 WAVs share a profile: **very dense** (50-70 truths each,
roughly 1 signal per 6-9 audio Hz of spectrum), pancetta recovers
~40-50%, WSJT-X gets all. They are the canonical "busy band" case.
hb-079's coherent subtract works one signal at a time and hits a
fundamental limit: **mutually masking signal pairs** where neither
decodes first cannot be subtracted to help the other, and dense
multi-station interference compounds.

This is joint multi-candidate decoding territory.

## Mechanism

**Pair / cluster detection.** From the sync_candidates list, find
candidates whose frequency bins are within ~25 Hz (4 FFT bins) AND
whose time-step alignment overlaps such that one signal's tone leakage
affects the other's symbol windows. Estimate the "mutual interference
strength" as a scalar from the overlap geometry + relative sync scores.

**Joint LLR computation.** For each pair (A, B) of close candidates:
- Extract complex symbols for both at their respective (t0, f0) positions.
- Each pair's symbol bins overlap partially (in time, in adjacent freq
  bins). The complex bin at any (t, f) is the sum of A's and B's signal
  contributions plus noise.
- The joint inference: given the codebooks (LDPC code structure), the
  ML estimate of (A's codeword, B's codeword) maximises the joint
  likelihood. Practically: alternate between estimating A's codeword
  (treating B's current estimate as known interference, subtracting it
  coherently before extracting A's LLRs) and B's codeword.
- This is **interference cancellation** (decision-feedback) at the
  LLR level. Three iterations typically converge.

**Variants to test:**

1. **Hard-decision joint (simplest):** decode A as currently (treating
   B as noise), if successful subtract A's coherent contribution from
   B's extraction window, decode B from the cleaner residual. Then
   maybe iterate. This is just hb-079 but applied *intra-window* to
   pair candidates rather than across passes.
2. **Soft joint (canonical):** maintain probabilistic estimates of
   both A and B, iterate LLR updates with mutual subtraction at each
   step. Higher CPU; closer to optimal.
3. **Triplet+:** extend to clusters of 3+. Diminishing returns; only
   if pair variant graduates.

## Build sequence

1. **Diagnostic first** (instrument to confirm pair-density on
   hard-200): for each missed truth in the top-20 WAVs, identify the
   *nearest* recovered decode in (freq, time) and check whether they
   could form a "joint decoding pair." If yes for most misses, the
   mechanism is right-sized. If most misses are isolated (no nearby
   decode), the wall is something else (sync? CRC?) and we pivot.
2. **Hard-decision joint pair variant** (V1). Implement intra-window:
   in the rayon decode loop, for each successful decode, identify
   nearby pending candidates, coherent-subtract from their extraction,
   re-attempt their LDPC. Bounded change to current pipeline.
3. **A/B on hard-200 top-20.** This subset is the natural target —
   if V1 wins there, it'll win on the broader corpus too. If V1 wins,
   full hard-200 + full 5-tier.
4. **Decision.** Graduate V1 OR pivot to soft variant (V2) if V1's
   recall is below the diagnostic ceiling.
5. **Future:** triplet+ if V1 graduates cleanly.

## Architecture-fit (mr-007 lens)

- ✅ **Direct attack on the measured residual wall** (top-20 WAVs,
  17% of all hard-200 misses).
- ✅ **Reuses hb-079's coherent subtract primitives** (complex
  spectrogram, rotor estimation, ML projection) — infrastructure
  payback continues.
- ✅ **The FP filter handles novel pressure**, as it has for every
  prior structural lever — pair decoding will add some FPs from
  spurious "interference cancellation" coincidences; the filter
  catches those.
- ⚠️ **CPU cost matters.** Joint decoding adds per-pair work. With
  ~30-60 candidates per WAV and ~1-3 nearby pairs each, the per-WAV
  overhead is bounded but real. Budget: aim for <30% wall-clock
  increase vs current production.
- ⚠️ **Variance risk.** Joint decoding has many design choices (pair
  selection, iteration count, subtraction order); some will shelve.
  Plan for 2-3 sessions including dead-ends.

## Open questions

- **Pair-selection criterion.** Strict frequency proximity (±25 Hz)
  vs energy-overlap (any nearby strong tone). Start with the simpler.
- **Whether to extend hb-079's multi-pass loop with the pair variant**
  or run a separate pair-decoding pass before the multi-pass loop.
  Probably the latter — pair decoding finds masked pairs in pass-1,
  then iterative subtract finds tertiary-masked.
- **Soft vs hard decision joint.** Hard is one session; soft is two.
  Probably try hard first, graduate or fail, then revisit.

## Eval plan

- **Diagnostic step**: per-WAV residual pair-density check. Quantify
  expected ceiling on joint decoding.
- **A/B**: hard-200 with/without joint pair (V1). Watch hard-1000 +
  full 5-tier if win.
- **Success criteria**: +30+ hard-200 recovered with manageable novel
  cost (filter absorbs). Composite +0.0015 or better.
- **Kill criteria**: <10 hard-200 recovered (mechanism doesn't fit
  the pair structure of this corpus), OR composite-negative from
  novel pressure even after filter.

## Risk-bounded scope

If the diagnostic step (step 1) shows fewer than ~30% of top-20
misses have a nearby recovered-decode-pair structure, the joint
decoding mechanism doesn't match the corpus's wall and we shelve
without implementing. That's the kill switch — it makes the project
bounded.
