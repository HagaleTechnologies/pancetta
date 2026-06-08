# Algorithm spec: Per-symbol windowed-Blackman SNR normalization

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — `convert_to_snr()` around lines 1349-1421
  (magnitude version), `c_convert_to_snr()` around lines 1423-1495
  (complex version). `blackman()` window helper at lines 127-137.
  Constants `snr_win = 7`, `snr_how = 3` at lines 50-51.
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

FT8 signals propagate through HF channels that have time-varying gain
(slow fading on the seconds scale, plus impulsive noise bursts).
Treating raw per-symbol FFT bin magnitudes as the soft-demod input
implicitly assumes a flat channel — when the channel actually fades
20 dB over the 12.6-second slot, the LLR vector has badly
miscalibrated magnitudes (large at the loud end, small at the faded
end) which the LDPC Bayesian decoder then mishandles.

`convert_to_snr` flattens this by normalizing every (symbol, tone)
bin by an **estimated per-symbol-time noise level**, smoothed across
a 15-symbol Blackman-weighted window. The result: a synthetic SNR
quantity that has unit median across the message, removing the
slow-fading envelope but preserving the per-symbol contrast between
the true tone and the seven noise tones.

The per-symbol noise estimator (`snr_how`) supports six variants;
the default `snr_how = 3` uses the **weakest tone** at each symbol
position as the noise estimate. The intuition: in an 8-FSK system,
even if there is no signal at this symbol time, eight bins of pure
noise will have a wide range of magnitudes — the weakest of the
eight is a robust estimate of the noise floor (it's not pulled up
by any genuine signal in any tone). The Blackman smoothing across
neighboring symbols then averages out per-symbol noise fluctuations.

## Algorithm description (PROSE ONLY)

### Inputs (magnitude variant `convert_to_snr`)
- A 79×8 array `m79[symbol_index][tone]` of real-valued FFT bin
  magnitudes from per-symbol FFTs.

### Inputs (complex variant `c_convert_to_snr`)
- Same shape but complex bins. The complex variant takes
  `std::abs(m79[si][bi])` to compute the per-symbol noise estimate
  and then divides the *complex* bins by the smoothed noise scalar,
  preserving phase.

### Outputs
- An array of the same shape with each bin divided by the
  per-symbol smoothed noise estimate. The complex variant returns
  complex bins; the magnitude variant returns real magnitudes.

### Steps

1. **Per-symbol raw noise estimate.** For each `si` in `0..79`, copy
   the 8 bin magnitudes into a working vector `v[0..8]`. If
   `snr_how != 1`, sort `v` ascending so `v[0]` is the weakest and
   `v[7]` the strongest. Then pick the noise estimate `mm[si]`
   according to `snr_how`:
   - `snr_how = 0`: median of the 8 tones, `(v[3] + v[4]) / 2`.
   - `snr_how = 1`: arithmetic mean, `sum / 8`. (No sort — uses
     `sum` accumulated during the copy.)
   - `snr_how = 2`: mean of the 7 weakest tones (drops the strongest,
     which is likely the true tone), `(v[0]+v[1]+...+v[6]) / 7`.
   - `snr_how = 3`: weakest tone, `v[0]`. (Default.) This is the
     most-noise-only-likely tone — if any of the 8 bins is genuine
     signal, `v[0]` is unlikely to be it.
   - `snr_how = 4`: strongest tone, `v[7]`. (Inverts the logic;
     useful as a sanity-check baseline.)
   - `snr_how = 5`: second-strongest tone, `v[6]`. (Compromise
     between median and weakest.)
   - Other values: `mm[si] = 1.0` (no normalization).

2. **Build a Blackman smoothing window** of length `2 × snr_win + 1`
   (with `snr_win = 7`, length 15). The Blackman window is
   `0.42 - 0.5 × cos(2π k / n) + 0.08 × cos(4π k / n)` for `k = 0..n-1`.
   It is not normalized to unit sum — the absolute scale of the
   normalization is irrelevant since every bin is divided by the
   same scalar at each symbol position.

3. **Apply the smoother symbol-by-symbol.** For each `si` in
   `0..79`:
   - Accumulate `sum = Σ mm[clamp(dd, 0, 78)] × winwin[wi]` for
     `dd` in `si - snr_win .. si + snr_win` (15 values), where
     `wi = dd - (si - snr_win)`.
   - The clamping handles symbol-array boundaries: at `si < snr_win`
     the missing left tail is replaced with `mm[0]`; at
     `si > 78 - snr_win` the missing right tail is replaced with
     `mm[78]`. (Reflection or mirroring would also work; ft8mon's
     choice is edge-clamping.)
   - For each tone `bi` in `0..8`, write
     `n79[si][bi] = m79[si][bi] / sum`. The complex variant does
     `n79[si][bi] = m79[si][bi] / sum` with `sum` real — phase is
     preserved.

4. **Disable behavior**: if `snr_how < 0` or `snr_win < 0` at config
   time, the function returns the input unchanged. This is the
   "raw bin" baseline for comparison.

### Numerical constants (facts, not expression)
- `snr_win = 7` — half-window in symbols. Total window length
  `2 × 7 + 1 = 15` symbols ≈ 2.4 seconds (HF slow-fade time scale).
- `snr_how = 3` — weakest-tone noise estimator (default).
- Blackman window coefficients: 0.42, 0.5, 0.08 (standard 3-term
  Blackman).
- Window not normalized to unit sum (irrelevant scale).
- Edge handling: clamp (extend with `mm[0]` / `mm[78]`).

### Edge cases
- **Smoothing window length must be odd** (`2 × snr_win + 1`).
  `snr_win = 0` reduces to a trivial 1-sample window
  (`winwin = [1.0]`) — i.e. no smoothing, just per-symbol weakest-
  tone normalization. Useful for fast-fading regimes.
- **Zero noise estimate** — if all 8 tones are zero at some symbol
  (degenerate FFT input), `mm[si] = 0` and the smoothed sum could
  approach zero in a region of all-zero symbols. This would
  divide-by-zero. Pancetta's port should add an epsilon floor
  (`max(sum, EPS)`) to handle this.
- **Edge clamping artifacts** — clamping with `mm[0]` and `mm[78]`
  means the first 7 and last 7 symbols are normalized by a sum
  biased toward the endpoint's noise level. Acceptable because
  the Costas blocks (symbols 0-6 and 72-78) anchor those edges
  with high-confidence known tones; the noise estimate there is
  well-behaved.
- **Sort-vs-no-sort branch** — `snr_how = 1` (mean) does not need
  the sort and takes the no-sort branch. Other values sort. Don't
  optimize away the sort unconditionally.
- **Phase preservation in complex variant** — division by a real
  scalar preserves phase. Make sure the implementation doesn't
  accidentally convert to magnitude before dividing.

## Conflict with pancetta's existing mechanisms

Pancetta's current soft demod (per CLAUDE.md and Batch 30 results)
appears to use raw per-symbol FFT magnitudes without this
normalization. The `make_stats` / `bayes` machinery in pancetta
should already handle absolute-magnitude variation across messages
to some extent — it builds per-message distributions of "best tone"
and "all tones" magnitudes — but the per-symbol-within-a-message
variation (slow fading inside one 12.6-second slot) is not absorbed
by that machinery. The 15-symbol smoothed normalization is
specifically aimed at within-message fading.

Interaction with the three-soft-decoder-ensemble spec: all three
variants (single complex, pair, triple) call this same normalizer
at their entry. It is **shared infrastructure**, not per-variant.
Implementing it once benefits all three.

Interaction with the rate-reduction spec: this normalizer operates
on the per-symbol FFT output, which is downstream of the rate
reduction. No interaction.

The `snr_how = 3` (weakest tone) default is interesting from a
pancetta perspective because at the corpus level, the weakest tone
in an 8-FSK FFT is a quite stable estimator (8-tone min order
statistic of Rayleigh-distributed magnitudes is well-characterized).
Comparing to `snr_how = 2` (mean of 7 weakest) is a useful eval —
weakest is robust to FFT-window-leakage spikes; mean-of-7 is
robust to outlier-low magnitudes.

The Blackman window choice (vs Hamming or rectangular) reflects
ft8mon's empirical preference. The window has good frequency-
domain sidelobe rejection (-58 dB) which translates here as
"a strong burst in one symbol doesn't bleed into the noise
estimate of distant symbols". Rectangular smoothing would let
single-symbol bursts bias the noise estimate across 15 symbols.

## Estimated Rust port effort
- ~100-150 LOC in `pancetta-ft8/src/decoder/` (new file
  `snr_normalize.rs` or as part of an existing soft-demod module).
- 1 session: port + unit test against a synthetic slow-fading signal
  (e.g. a constant tone with a 20-dB-per-12-second linear amplitude
  ramp) showing that normalized magnitudes are approximately flat
  across the message.

## Implementation notes for the implementer thread

- The Blackman window can be precomputed once at startup (it's
  length 15 with `snr_win = 7`). No per-slot allocation.
- The per-symbol noise estimator should be a small enum / match,
  one variant per `snr_how` value (0-5). Make the default
  (`Weakest = 3`) and supply the others for eval comparison.
- The complex-variant division-by-real-scalar should use
  `Complex::scale(1.0 / sum)` or equivalent — avoid going through
  magnitude.
- Add a "raw bins" no-op path (`snr_how < 0` or `snr_win < 0`) so
  A/B comparisons are easy.
- Tier interaction: this normalizer is **always on** in ft8mon and
  has no compute downside (it's O(79 × 8 × 15) = O(10000) trivial
  multiplies per candidate, dwarfed by the FFT cost). Enable for
  all tiers.
- Eval target: in addition to overall hard-200 recall, look at the
  "fading-marked" subset (any signal whose first-7-symbol and
  last-7-symbol mean magnitudes differ by >10 dB). Expected lift on
  that subset: 3-5 percentage points.
- This is the input to the soft-demod statistics builder. Implement
  before any of the three soft-decoder variants depend on it.
- Compose this with the spec-wsjtr-sync-norm.md spec? They're
  conceptually similar (both normalize per-symbol noise) but
  operate at different stages — wsjtr's `sync-norm` normalizes
  sync scores, this spec normalizes soft-demod input. Land both
  independently.
