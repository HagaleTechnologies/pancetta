# Algorithm spec: Three-way soft-decoder ensemble (single / pair / triple)

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — driver structure around lines 2640-2705
  (`one_iter1`), `c_soft_decode` around lines 1786-1914,
  `soft_decode_pairs` around lines 1949-2053, `soft_decode_triples`
  around lines 2056-2180. Companion: `c_convert_to_snr` at lines
  1427-1495 (the complex-bin normalizer all three variants share).
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

Pancetta's current soft demodulator produces a single 174-bit LLR
vector per candidate from magnitude-only per-symbol FFT bins. ft8mon
runs **three structurally different** LLR producers per candidate and
hands each one independently to LDPC + CRC. They differ in what
correlation pattern they exploit:

- **Single-symbol complex demod (`c_soft_decode`)** — per-symbol
  hypothesis test that uses **complex** bin values and a
  phase-coherence prior across a sliding window of `c_soft_win = 2`
  neighbor symbols. Each tone is scored by how well its phase and
  magnitude match the predicted phase/magnitude implied by surrounding
  "genuine-looking" symbols.
- **Two-symbol coherent pair (`soft_decode_pairs`)** — already specced
  in `spec-ft8mon-soft-decode-pairs.md`. Sums complex bins of two
  adjacent symbols, takes magnitude. Wins ~1.5-3 dB on slow-fading
  channels where phase is stable over 320 ms.
- **Three-symbol coherent triple (`soft_decode_triples`)** — sums
  complex bins of three adjacent symbols, takes magnitude. Wins
  another ~0.7-1.5 dB on **even slower** fading, costs 512 tone
  combinations per stride instead of 64.

The three decoders are **alternative pathways, not fused**. The first
one whose LDPC + CRC validates wins; further variants are skipped.
This pattern is the load-bearing structural insight: rather than
trying to fuse multiple LLR vectors (which is statistically subtle —
correlated noise across variants makes weighted averaging worse than
either alone in many regimes), ft8mon treats each variant as an
independent decoder that gets one shot at LDPC, and lets the 14-bit
CRC stop false positives.

## Algorithm description (PROSE ONLY)

### Inputs (shared across all three variants)
- A 79×8 **complex** per-symbol FFT array `m79` (or `c79`) from the
  fine-sync stage. Phase must be preserved — magnitude-only inputs
  cannot drive any of the three variants.
- Costas symbol positions `{0..6, 36..42, 72..78}` and Costas pattern
  `{3, 1, 4, 0, 6, 5, 2}` (known a priori).
- Gray-code un-map table `{0, 1, 3, 2, 6, 4, 5, 7}` (note ft8mon's
  layout — confirm against pancetta's existing map).
- A choice of `problt_how_noise` and `problt_how_sig` for the Bayes
  conversion (see "Numerical constants" below; the existing
  `soft_decode_pairs` spec covers the `Stats` machinery).

### Outputs (each variant)
- A 174-element `f32` LLR vector, with the standard sign convention
  (positive = bit 0, negative = bit 1).

### Driver (`one_iter1`)
1. Extract `m79` from the candidate's 200-sps audio at `(best_hz, best_off)`.
2. Optionally run fine-sync refinement (`do_fine_hz`, `do_fine_off`)
   and re-extract `m79`.
3. **If `soft_ones` is enabled**, call either `soft_decode` (the
   simpler magnitude-only path, `soft_ones == 1`) or `c_soft_decode`
   (the complex phase-aware path, `soft_ones == 2`). Pass the
   resulting `ll174` to `try_decode` with `use_osd = 1`. If LDPC + CRC
   validates, return.
4. **If `soft_pairs` is enabled**, call `soft_decode_pairs`. Pass the
   resulting `p174` to `try_decode` with `use_osd = 1`. If validates,
   return. (Side effect: if `soft_ones == 0`, copy `p174` into
   `ll174` to seed the hint loop's "starting LLR vector".)
5. **If `soft_triples` is enabled**, call `soft_decode_triples`. Pass
   to `try_decode` with `use_osd = 1`. If validates, return.
6. Fall through to the `use_hints` loop (separate spec) using whichever
   `ll174` was last available.

### Variant 1: `c_soft_decode` (complex single-symbol with phase prior)

The key idea: for each (symbol, tone) bin, the per-bin LLR is *not*
just "is this bin large?". It is "does this bin agree in phase and
magnitude with what the surrounding `c_soft_win` symbols predict for
this tone?". A genuine tone has tightly clustered phase + magnitude
across adjacent symbols (channel is approximately flat over 80 ms × 5
symbols = 400 ms); a noise spike does not.

#### Steps
1. **Per-symbol normalize** via `c_convert_to_snr` (complex variant of
   `convert_to_snr` — see SNR spec). Result: per-symbol bins divided
   by a windowed-Blackman-smoothed noise estimate so that the
   magnitude scale is stable across the 79 symbols.
2. **Identify the "max tone" at each symbol position** `maxes[i]`:
   - For Costas symbols (`i < 7`, `36..42`, `72..78`), `maxes[i]` is
     the complex bin at the **known** Costas tone (no guessing).
   - For data symbols, `maxes[i]` is the strongest-magnitude tone (a
     best-guess at the true tone). This is intentionally noisy at
     low SNR; the surrounding-symbol smoothing absorbs that.
3. **Score every (symbol, tone) bin** as the negative of:
   - A `c_soft_weight` (default 7) multiplied by the bin's own
     magnitude (favors strong evidence for that bin's hypothesis).
   - Minus the sum over `k` in `[i - c_soft_win, i + c_soft_win]`
     (skipping `k == i`) of the **complex distance**
     `|maxes[k] - c79[i][j]|` — i.e. how far that bin's phasor is
     from the surrounding-symbol-predicted phasor.
   - Divided by `n` (number of neighbors actually used, clamped at
     buffer edges).
   - Negated so that higher score = more likely true.

   Net effect: a bin that is both bright and *in phase with its
   neighbors* gets the highest score; a bin that is bright but
   phase-incoherent with surrounding symbols gets a much lower score.
4. **Make Bayes statistics** (`make_stats`) over the resulting 79×8
   score array — distribution of "best tone per symbol" goes in
   `bests`, distribution of all scores goes in `all`.
5. **Un-gray-code** the rows (`un_gray_code_r`).
6. **For each of 58 data symbols × 3 bits = 174 bit positions**, find
   the best-scoring tone consistent with bit = 0 (from the 4 tones
   whose gray-decoded value has that bit = 0) and the best-scoring
   tone consistent with bit = 1. Convert to LLR via the standard
   Bayes formula (same one used by pair/triple).

The phase-coherence prior is what makes this variant genuinely
different from the magnitude-only `soft_decode`. It works well
specifically when the channel is approximately flat over ~400 ms but
the noise floor is not flat — i.e. when a wideband noise pulse covers
several FFT bins on one symbol but doesn't repeat. The phase-incoherent
noise pulse fails the "matches surrounding-symbol phase" test even
when it is strong.

### Variant 2: `soft_decode_pairs`

Covered in `spec-ft8mon-soft-decode-pairs.md`. Summary: stride 2 over
data symbols, 64 pair combinations per stride, complex vector sum and
magnitude, per-bit max-evidence-for-zero / max-evidence-for-one,
Bayes-convert to LLR.

### Variant 3: `soft_decode_triples`

#### Steps
1. **Normalize** via `c_convert_to_snr`.
2. **Iterate over symbol triples**: stride 3 through `si = 0..79`. At
   each stride, the triple `(si, si+1, si+2)` spans 9 LDPC bits (3
   per symbol). If `si+1` or `si+2` falls past 79, omit them from the
   sum (degenerate triple at the tail).
3. **Enumerate 8³ = 512 tone combinations** `(s1, s2, s3)`. For each:
   - Compute `csum = m79[si][s1] + m79[si+1][s2] + m79[si+2][s3]`.
   - Take magnitude `x = |csum|`.
   - Append `x` to the `all` statistics distribution.
   - For each of the three symbols and each of the 3 bits in its
     gray-decoded tone, update `bitinfo[bitind].one` or
     `bitinfo[bitind].zero` to be the max of its previous value and
     `x` — exactly the same per-bit best-evidence structure as the
     pair variant.
4. **Stash the "true Costas correlation"** as a representative of the
   "signal-present" hypothesis in `bests`:
   - At `si = 0, 36, 72` (Costas blocks starting), record the
     magnitude of the `(s1=3, s2=1, s3=4)` corner of the 512-corr
     table — the Costas pattern's first three tones.
   - At `si = 3, 39, 75` (Costas blocks continuing), record the
     `(s1=0, s2=6, s3=5)` corner — the next three tones.
   - For other strides, record the maximum of the 512-corr table
     (best-guess at the true tone triple).
5. **Bayes-convert** each `bitinfo[i].zero` / `bitinfo[i].one` pair
   into an LLR using the same `bests`/`all` distributions as the
   pair variant.

The triple structure is **8× more expensive** than pair (512 vs 64
combinations per stride) and visits roughly **2/3 the number of
strides** (`79/3 ≈ 26` vs `79/2 ≈ 39`). Net cost: ~5× pair, ~30×
single-symbol.

The triple variant pays off only when the channel is **even slower**
than the pair assumption — i.e. phase stable over 3 × 160 ms = 480 ms.
This is the regime of true slow-fading HF channels; on a fast-fading
multipath channel the triple sum may destructively interfere worse
than the pair sum. ft8mon's response is to run all three variants and
let CRC sort it out.

### Ensemble structure (the key insight)

The variants are **OR**-combined at the LDPC + CRC stage, not
LLR-fused. The flow is:

```text
extract m79 → 
  c_soft_decode → ll174_a → LDPC → CRC → emit if valid
                                       ↓
  soft_decode_pairs → p174_b → LDPC → CRC → emit if valid
                                       ↓
  soft_decode_triples → p174_c → LDPC → CRC → emit if valid
                                       ↓
  use_hints → (apply each hint, LDPC, CRC)  → emit if valid
                                       ↓
                                  give up
```

Whichever decoder converges first wins; CRC filters false positives
across the whole chain. This is structurally an **ensemble of weak
learners** in the Boosting sense: each variant has a different bias
(magnitude-only / phase-coherent / pair-coherent / triple-coherent /
hint-clamped) so they make different decoding errors. The CRC + LDPC
combination is the meta-classifier that accepts the first variant
whose answer survives.

### Numerical constants (facts, not expression)
- `c_soft_win = 2` — half-window of neighbor symbols used in the
  phase-coherence prior for the single-symbol variant. Window length
  = `2 × c_soft_win + 1 = 5` symbols (~400 ms).
- `c_soft_weight = 7` — how strongly the own-bin magnitude is weighted
  vs the surrounding-phase distance. Higher = more like a pure
  magnitude decoder; lower = more like a pure phase-coherence decoder.
- `soft_ones = 2` — use `c_soft_decode` (1 would select the simpler
  magnitude-only `soft_decode`).
- `soft_pairs = 1` — pair variant enabled.
- `soft_triples = 1` — triple variant enabled.
- Stride: 2 for pairs (29 windows), 3 for triples (~26 windows).
- Combinations per window: 64 for pairs, 512 for triples.
- `bayes_how = 1` — selects the Bayes posterior formula in the shared
  `bayes()` helper.

### Edge cases
- **Tail handling for triples** — when `si + 1 >= 79` or `si + 2 >= 79`,
  the corresponding tone is simply omitted from `csum` (the source
  literally skips the `+=`). This means the last partial triple gets
  scored with degraded SNR.
- **Costas-block straddling triples** — when a stride 3 sweep crosses
  a Costas block, ft8mon's source as written does not specifically
  skip Costas symbols in the *sum* (they still contribute their
  known-tone magnitude), only in the *bit-output* loop. The pair and
  triple sums therefore include Costas tones when they fall inside
  the stride — which is acceptable because Costas tones are
  predictable (high magnitude at the known tone) and contribute
  consistently across all 64/512 combinations, washing out in the
  per-bit max.
- **`maxes[i]` for noise-only symbols** — if all 8 tones are noise,
  `maxes[i]` is just the loudest noise bin. The surrounding-symbol
  smoothing in `c_soft_decode` keeps this from blowing up the LLR
  vector — a single noise-spike symbol cannot dominate because it
  must agree with neighbors.
- **Variant order matters** — `c_soft_decode` is the cheapest (one
  pass, no combinatorial sweep) and runs first. `soft_decode_pairs`
  is next (29 windows × 64 = 1856 ops). `soft_decode_triples` is
  last (~26 windows × 512 = ~13.3k ops). The order is
  cost-ascending so the cheap path runs first.

## Conflict with pancetta's existing mechanisms

Pancetta's current soft demod is the **magnitude-only single-symbol**
path (equivalent to ft8mon's `soft_decode` with `soft_ones = 1`).
Adding the three-way ensemble is **purely additive**:
- The complex single-symbol path requires the same complex `m79` that
  the pair variant requires. If the pair spec
  (`spec-ft8mon-soft-decode-pairs.md`) is implemented first, the
  complex `m79` is already plumbed and adding `c_soft_decode` is a
  ~150 LOC port.
- The triple variant requires nothing new (same complex `m79`) but
  adds the heaviest per-candidate cost in the entire decode pipeline.
  It should be gated on tier — disabled on Slow, optional on Moderate,
  default on for Fast.

The 5-symbol phase-coherence prior in `c_soft_decode` is the
genuinely new mechanism vs the already-specced pair variant. It
specifically targets a different regime — single-impulsive noise
spikes — than the slow-fading scenario the pair variant targets.

False-positive risk: each variant adds independent LDPC + CRC
attempts. CRC is 14 bits → 1 in 16384 false-acceptance per attempt.
Worst case (3 variants + 50 hints + 100 candidates × 6 passes) gives
~80k attempts per slot, raising the cumulative FP rate from ~0 to
~0.5%. The existing hb-058 / hb-062 / hb-103 FP filters absorb this
without recalibration.

Interaction with hb-217 (RR73 fix): the RR73 fix lives in the
parser, downstream of LDPC. All three variants benefit from it
equally — no special handling.

## Estimated Rust port effort
- `c_soft_decode`: ~150-200 LOC. Single-pass nested loop; the only
  novel piece vs the existing single-symbol path is the surrounding-
  window complex-distance accumulator.
- `soft_decode_triples`: ~250-300 LOC. Cleanly parallel to the pair
  variant once that's in place; the 8³ triple loop is the hot path
  and benefits from SIMD if pancetta has it.
- Driver wiring in the decoder: ~50 LOC to chain the three variants
  in the cost-ascending order with early-exit on success.
- 2-3 sessions: (S1) `c_soft_decode` with a unit test against a
  synthetic single-impulse noise scenario; (S2) `soft_decode_triples`
  with a unit test against a synthetic 480 ms slow-fade scenario;
  (S3) ensemble wiring + hard-200 eval, especially the weak-SNR and
  capture-effect buckets.

## Implementation notes for the implementer thread

- Variant order is non-negotiable: cheap → expensive. Profile
  pancetta's existing single-symbol path and place `c_soft_decode`
  immediately before it; the existing magnitude-only path can stay
  as a "fast fallback" or be retired once `c_soft_decode` is
  validated.
- Share the **complex `m79`** across all three variants. Materialize
  it once per candidate at the top of `one_iter1`-equivalent and
  pass `&m79` (or `&c79`) down.
- The `c_convert_to_snr` normalizer is shared infrastructure — it
  is the same Blackman-window per-symbol noise estimate used by the
  magnitude version. See the SNR spec
  (`spec-ft8mon-snr-windowed-blackman.md`) for the details.
- The `Stats` / `problt` machinery is shared with the pair spec.
  Don't re-implement; reuse.
- Tier-gating recommendation:
  - **Fast tier**: all three variants enabled, hint loop enabled.
  - **Moderate tier**: single + pair enabled, triple disabled,
    hints disabled.
  - **Slow tier**: single (magnitude-only) only, no pair / triple /
    hints. (Matches the existing `max_decode_passes=1, osd_depth=1`
    Slow-tier preset philosophy.)
- Watch for **NaN propagation** in `c_soft_decode`: the
  surrounding-window complex distance is computed against `maxes[k]`
  which is a complex number; if any `m79[k][·]` row contains zero
  bins (degenerate FFT input), `maxes[k]` could be zero and the
  complex distance becomes meaningless. Clamp with an epsilon at
  the input.
- Eval target: hard-200 buckets where the magnitude-only path is
  known to underperform — specifically the weak-SNR (≤ -19 dB)
  bucket where the pair / triple variants are designed to win, and
  the slot-edge negative-dt bucket where the surrounding-symbol
  phase-coherence prior of `c_soft_decode` should specifically help
  by absorbing the per-symbol time-misalignment as a constant phase
  rotation. Headroom estimate: each variant adds ~3-7% recall on
  its target bucket; ensemble total ~10-15%.

## Cross-references
- `spec-ft8mon-soft-decode-pairs.md` — pair variant in detail.
- `spec-ft8mon-snr-windowed-blackman.md` — `c_convert_to_snr` shared
  normalizer.
- `spec-ft8mon-use-hints.md` — the hint loop runs *after* this
  ensemble.
