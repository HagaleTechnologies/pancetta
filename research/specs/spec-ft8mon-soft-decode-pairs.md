# Algorithm spec: Soft LLR producer via two-symbol coherent pair correlation

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` approximately lines 1949-2053 (function
  `soft_decode_pairs`)
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

The single-symbol soft demodulator that pancetta currently uses (and that
ft8mon also retains) treats each symbol as an independent 8-tone
hypothesis test using only that symbol's FFT-bin magnitudes. On
slow-fading channels — i.e. channels where the channel coefficient is
approximately constant over two consecutive symbols — adding the complex
FFT outputs of two adjacent symbols **coherently** (vector sum, phase
preserved) before taking magnitude integrates signal but only sqrt(2)
times the noise, yielding roughly 1.5–3 dB of additional SNR on those
two-symbol windows. The literature claim is well established for
slow-fading FSK; ft8mon implements it as an independent decoding
pathway: produce a complete 174-bit LLR vector from pair correlations
alone, hand it to LDPC + CRC, and accept the result if the codeword
validates. Pancetta's hb-217 batch identified a large residual coverage
gap on weak signals that this technique targets.

## Algorithm description (PROSE ONLY)

### Inputs
- A 79 × 8 array of **complex** FFT bins, denoted `m79[symbol_index][tone]`,
  one per symbol time × tone hypothesis. These are the same complex
  bins that come out of the per-symbol FFT step during fine sync; phase
  must be preserved (no magnitude-only reduction has occurred).
- The Costas-sync symbol positions (the three blocks at 0–6, 36–42,
  72–78) are known a priori and must be excluded from the data-bit
  extraction.

### Outputs
- A 174-element `f32` array of log-likelihood ratios, one per LDPC
  codeword bit. Sign convention: positive favors bit = 0, negative
  favors bit = 1 (the same convention used by the existing single-symbol
  soft demod and the LDPC stage).

### Steps

1. **Iterate over symbol pairs.** Step `si` through the 58 data-symbol
   indices in strides of two (skipping the Costas blocks). At each
   stride, the pair `(si, si+1)` carries 6 data bits — three bits per
   symbol — but the algorithm exploits the joint two-symbol structure
   rather than decoding bit-by-bit per symbol.

2. **Form the 64 pair sums.** For each `(s1, s2)` pair with `s1` and
   `s2` each in `0..8`, compute the complex vector sum
   `csum = m79[si][s1] + m79[si+1][s2]` and its magnitude `|csum|`.
   This produces a 64-element table of coherent-pair magnitudes.

3. **Per-bit accumulators.** For each of the 6 LDPC bits that the pair
   spans (bits `b0`-`b2` come from the gray-decoded value of `s1`; bits
   `b3`-`b5` from the gray-decoded value of `s2`), maintain two
   running maxima:
   - `best_for_zero[b]` = the largest `|csum|` over all 64 pair
     combinations consistent with this bit equal to 0.
   - `best_for_one[b]`  = the largest `|csum|` over all 64 pair
     combinations consistent with this bit equal to 1.
   At the end of the 64-iteration sweep these two scalars represent the
   strongest pair-evidence in favor of each bit value.

4. **Gather global statistics for the Bayes conversion.** Two empirical
   distributions are estimated **on-the-fly** from this slot only (no
   pre-trained tables):
   - `bests` — a distribution of "best magnitude seen at the correct
     hypothesis" values, populated by appending every `best_for_*` that
     wins.
   - `all`   — a distribution of all observed `|csum|` values across the
     entire pair sweep, used as the noise/null reference.
   Both distributions are stored as sorted arrays so that a magnitude
   can be turned into a cumulative-probability value
   `P(observe ≤ x | hypothesis)` by simple table lookup (the
   ft8mon `problt()` method).

5. **Bayes conversion from magnitudes to LLR.** For each bit `b`, with
   `m0 = best_for_zero[b]` and `m1 = best_for_one[b]`:
   - Compute `p0 = P(observe ≥ m0 | signal-present)` from the `bests`
     distribution, and `q0 = P(observe ≥ m0 | noise)` from the `all`
     distribution.
   - Compute `p1`, `q1` symmetrically with `m1`.
   - Form posterior odds `odds = (p0 * q1) / (p1 * q0)` — i.e. the
     ratio of "probability that the hypothesis bit=0 explains the
     observation" over "probability that bit=1 explains it" — then take
     `ln(odds)` to get the LLR.
   - Sign convention: positive when m0 > m1 (bit=0 favored), negative
     otherwise.

6. **Skip Costas positions.** During the pair sweep, when `si` would
   straddle or land on a Costas-block symbol, advance past that block.
   The Costas symbols carry known patterns, not data bits, and must
   not be folded into the LLR array.

7. **Output the 174-LLR vector** in LDPC bit-index order, ready to hand
   to the LDPC + CRC stage.

### How this is *used* by the calling code

`soft_decode_pairs` is one of several independent demodulators tried by
the per-candidate decode driver (`one_iter1` in ft8mon). The driver
calls the single-symbol soft demod first, attempts LDPC + CRC; on
failure, calls the pair-based demod, attempts LDPC + CRC; on failure,
the triple-based demod. Each demodulator produces its own complete
174-LLR vector. **There is no averaging or fusion between them** —
they are alternative pathways and the first one whose LDPC output
passes CRC wins.

### Numerical constants (facts, not expression)
- 64 pair combinations per two-symbol window (`8 × 8`).
- 6 LDPC bits derived per pair window (3 per symbol).
- 58 data symbol indices (79 total − 21 Costas).
- Stride of 2 means the pair sweep visits 29 windows.
- No pre-trained probability table — distributions are slot-local.

### Edge cases
- **The last data symbol is unpaired** if the odd/even arithmetic
  leaves a dangling final symbol. ft8mon's structure naturally handles
  this because data symbols come in three blocks of equal length
  (7-data, 14-data, 14-data layouts surrounding Costas blocks); confirm
  layout in the implementation rather than hand-coding parities.
- **Costas-straddling pairs** must be skipped (do not pair a data symbol
  with a Costas symbol).
- **Empty `bests` distribution at startup of the bit sweep** — the first
  bit conversion happens before any `best_for_*` has been observed.
  ft8mon's `problt()` returns a sane default when the distribution is
  empty; pancetta's port should clamp `p` to a small epsilon to avoid
  `log(0)`.
- **Gray code unmapping** — symbol indices 0..7 must be unmapped through
  the FT8 gray code before extracting individual bits. Pancetta already
  has the gray map; use it.
- **No coherence assumption beyond two symbols** — the algorithm
  assumes phase is approximately stable over two adjacent FT8 symbols
  (320 ms total). On fast-fading channels this assumption fails and the
  pair sum can destructively interfere; that's why the algorithm is run
  in parallel with the single-symbol path rather than replacing it.

## Conflict with pancetta's existing mechanisms

Pancetta's current soft demod (single-symbol) produces a 174-LLR vector
fed to the OSD/BP stage. The pair-decode pathway is **additive**, not
replacement: it produces a second 174-LLR vector that is tried
independently. The natural integration point is the per-candidate
decode driver — wherever the current code constructs a 174-LLR vector,
construct a second one via pair decode, run LDPC + CRC on both, accept
either valid codeword.

Important: this needs the **complex** per-symbol FFT bins. If pancetta
currently discards phase after the per-symbol FFT (taking magnitudes
into a `[f32; 79][8]`), the per-symbol FFT path must be amended to
retain (or alternatively re-compute) the complex bins for this codepath.
Check `pancetta-ft8/src/decoder/` for where `m79`-equivalent is
materialized; if it is `[f32; 79][8]`, refactor to keep both magnitude
and complex copies, or recompute complex bins on demand for the
candidates that single-symbol demod failed on.

Interaction with hb-217 RR73-fix and hb-103: pair decode is a coverage
add, not a precision add. Any extra TPs come at the cost of additional
FP risk — gate behind the existing CRC validation (no relaxation) and
keep the existing FP filters (hb-058, hb-062, hb-103) active on the
emitted decodes. Empirically pair-decode FPs are extremely rare because
LDPC + 14-bit CRC have a vanishing false-acceptance rate; the
gating cost should be near zero.

## Estimated Rust port effort
- ~150–250 LOC in `pancetta-ft8/src/decoder/soft_decode.rs` (new file or
  extension of existing single-symbol path).
- Possibly ~50 LOC of refactoring in the per-symbol FFT path to retain
  complex bins.
- 1–2 sessions: (S1) port the pair-sum + Bayes + LLR construction, with
  a unit test against a synthetic two-symbol pair where ground truth is
  known; (S2) wire into the per-candidate driver, eval on hard-200,
  measure coverage delta.

## Implementation notes for the implementer thread

- The `Stats`/`problt()` machinery in ft8mon is just "sort the
  magnitudes, binary-search for the cumulative-probability value." A
  Rust analog is `Vec<f32>` sorted once, with a `partition_point()`
  lookup — `O(log n)`. No external crate needed.
- For `bests` and `all`: gather every pair magnitude during the sweep,
  then sort once before the LLR conversion pass. This is two passes
  through the 64×29 = 1856-element data — trivial cost.
- A clean abstraction is `fn pair_llr(m79_complex: &[[Complex<f32>; 8]; 79])
  -> [f32; 174]`, mirroring the signature of the existing
  single-symbol soft demod.
- The Bayes step is numerically sensitive at the tails of the empirical
  distributions. Clamp `p` and `q` to `[eps, 1-eps]` with `eps = 1e-6`
  before taking the log. ft8mon does something equivalent in `problt`.
- **Do not change the LDPC + CRC stage**. Pair decode is purely a
  different *input* to the existing decoder.
- Order of operations in the per-candidate driver: try single-symbol
  first (cheap, current baseline); on LDPC failure, try pair decode;
  on second failure, optionally try the triple-symbol path (also in
  ft8mon, similar pattern with stride 3) — note that the triple path is
  expensive (512 combinations) and may not pay off; profile before
  enabling.
- Watch the wall-clock budget on Moderate / Slow tiers; gate behind a
  `pair_decode_enabled` config flag and default on for Fast only.
- Eval: hard-200 corpus, especially the weak-SNR bucket (≤ -19 dB) and
  the slot-edge negative-dt bucket (currently 48.3% recall per hb-217
  notes).
