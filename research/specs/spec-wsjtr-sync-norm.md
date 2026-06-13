# Algorithm spec: 40th-percentile sync normalization

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- File path (traceability only, NOT quoted): `crates/jt9r/src/sync.rs`,
  percentile-baseline computation applied to the per-bin sync arrays
  (referred to in the source as `red` and `red2`) prior to thresholding.
- Companion doc: `docs/jt9r.md` (architecture summary, "Normalization Strategy"
  section)
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

The raw sync metric (whether full or partial Costas; see the sister spec
`spec-wsjtr-sync-bc.md`) is a *power ratio* whose absolute value depends on the
ambient noise level in the recorded audio, on AGC behaviour, on band conditions,
and on the spectrogram's frequency resolution. A fixed absolute threshold
(e.g. "sync > 1.2") therefore produces wildly different recall in a quiet
nighttime band vs. a noisy daytime band, and is brittle to gain changes in the
receive chain.

The 40th-percentile normalization solves this by dividing every per-bin sync
metric by an *empirical noise-floor reference* drawn from the bottom half of
the same metric distribution observed in the same audio window. The 40th
percentile of the per-bin sync maxima is, in expectation, dominated by bins
that contain only noise (because true signal bins make up a small minority of
the spectrum). Dividing by this baseline yields a "metric relative to ambient
noise" number that is comparable across recordings, bands, and times. After
normalization, the threshold `sync_min` (default 1.2) has a stable physical
meaning: a candidate must score at least 20% above the noisy baseline.

The technique is a form of CFAR (Constant False Alarm Rate) detection adapted
for the FT8 sync stage: the threshold is not a hard absolute, it is a
percentile of the live-observed distribution.

## Algorithm description (PROSE ONLY — no code)

### Inputs

- `red[ia..=ib]`: a vector of per-frequency-bin sync metrics, where each entry
  is the *maximum* sync score for that frequency bin taken over all lags
  swept (i.e. `red[i] = max_{lag in [-62, +62]} sync_at(i, lag)`). The full
  Costas sync metric is used here. Range `[ia, ib]` is the frequency search
  window (in spectrogram bin indices).
- `red2[ia..=ib]`: a parallel vector of per-bin sync metrics for the partial
  Costas variant (`sync_bc`). Same indexing convention.

### Outputs

- The same two vectors, normalized in-place: every entry has been divided by
  its respective percentile baseline. After normalization, both vectors are
  unit-less ratios where `1.0` means "at the 40th-percentile noise floor",
  and values above the threshold `sync_min` are candidate signals.

### Steps

1. Build a working copy of `red[ia..=ib]` (so that sorting does not destroy
   the original frequency-indexed order).
2. Sort the working copy ascending. After sorting, low-power bins (mostly
   noise) are at the front, high-power bins (signal candidates) are at the
   back.
3. Compute the percentile index: `idx = round(0.40 * length)`, clamp to a
   minimum of 1, then subtract 1 to convert to a zero-based array index. For
   a search range of, say, 200 bins, this yields the sorted entry at index
   79 — the value below which 40% of all bins' maxima lie.
4. Read the percentile baseline value from the sorted working copy at that
   index. Call it `base_red`.
5. If `base_red` is strictly positive and finite, divide every entry of the
   original `red[ia..=ib]` by `base_red` in place. If `base_red` is
   non-positive or non-finite, the normalization is skipped (and downstream
   thresholding will reject everything because nothing will exceed the
   threshold — this is the correct conservative behaviour for an all-zeros
   audio buffer).
6. Repeat steps 1–5 for `red2[ia..=ib]` independently, computing its own
   percentile baseline `base_red2` and dividing in place.

After step 6, both vectors are on the "ratio to noise floor" scale.

### Thresholding (immediately after normalization)

A frequency bin `i` is accepted as a candidate when **either**
`red[i] >= sync_min` **or** `red2[i] >= sync_min`, where `sync_min = 1.2`
by default. (The `red2` path is what surfaces slot-edge signals; see
`spec-wsjtr-sync-bc.md`.) The wsjtr source treats this as a single accept
condition on `max(red, red2)` because the two vectors are post-normalization
on the same scale.

Accepted bins are then ranked (typically by `max(red, red2)` descending) and
the top entries fed to the refinement stage (see
`spec-wsjtr-grid-refinement.md`). The candidate list is capped at 1000 entries
*before* refinement.

### Numerical constants (facts, not expression)

- Percentile fraction: 0.40 (40th percentile).
- Index rounding: standard `round` (round-half-away-from-zero is fine; the
  difference vs. round-half-to-even is one bin out of hundreds and is in the
  noise).
- Minimum clamp on the index: 1 (so the smallest legal selection is the second
  smallest entry; `idx - 1 = 0` after the clamp-then-subtract).
- Default `sync_min` threshold: 1.2 (applied uniformly; see "What the code
  does NOT do" below).
- Candidate cap: 1000 entries before refinement.

### Edge cases

- **All-zero or empty `red` vector**: the percentile baseline is zero or
  undefined; normalization is skipped and no candidates pass the threshold.
  Correct conservative behaviour.
- **Single-bin search range** (`ia == ib`): the sort is trivial, the index
  is 0, and the percentile baseline equals the single value — the
  normalization divides the single value by itself, producing 1.0, which is
  below `sync_min`, so no candidate is reported. This is acceptable;
  single-bin searches do not happen in practice.
- **Saturation / DC offset in audio**: a single bin with extreme power does
  not poison the percentile baseline because the percentile is bounded by
  rank, not by sum. This is the whole point.
- **Very narrow signal cluster**: if more than 60% of bins genuinely contain
  signal (e.g. a contest with many concurrent stations packed into a narrow
  audio range), the 40th percentile is itself elevated, and the threshold
  becomes more conservative. This is a known tradeoff of percentile CFAR; the
  fix in such corpora is to widen `ia..=ib` so the percentile sees more
  noise-only bins.

### What the code does NOT do

The companion doc summary suggested a "depth-dependent threshold scaling"
(`depth=1` → 1.0×, `depth=2` → 0.9×, `depth=3` → 0.83×) — the reader thread
confirmed by direct source read that **no such scaling exists** in
`crates/jt9r/src/sync.rs`. The `sync_min` constant is applied uniformly.
Implementers should not chase the depth-scaling claim from the doc summary.

Similarly, the doc summary suggested a `(0.04s, 4 Hz)` post-refinement
deduplication; the reader thread confirmed **no proximity-based dedup**
exists in the sync candidate path. The only dedup is the post-decode message
text dedup. Implementers should not add a proximity-dedup step on the basis
of the doc summary alone.

## Conflict with pancetta's existing mechanisms

Pancetta's current sync thresholding (in `pancetta-ft8/src/decoder.rs`) uses
a different normalization scheme inherited from ft8_lib: a fixed-window noise
estimate computed from a fraction of the spectrogram. This is also a form of
adaptive noise estimation, but it operates on a different statistic (a smoothed
mean or median over time-and-frequency neighbourhoods) and produces a different
distributional reference.

Conflicts to consider:

1. **Threshold semantics shift**: pancetta's `sync_min` (if any) is calibrated
   against the *existing* normalization. Swapping in the wsjtr percentile
   normalization invalidates the current threshold. The implementer must
   either retune `sync_min` against hard-200 or run both normalizations and
   accept the union (more conservative on FPs, larger recall).
2. **Interaction with hb-156 (lid_of_band)**: the lid_of_band manifest
   measures recall at SNR ≤ -19 dB, and percentile normalization can help
   weak signals (where the absolute sync ratio is small but the
   relative-to-floor ratio is still meaningful). Likely complementary.
3. **Interaction with hb-058 (/R-suffix), hb-062, hb-103**: all of those sit
   downstream of sync acceptance and are unaffected by the change.
4. **The `red2` vector requires `sync_bc`**: this spec assumes the
   `spec-wsjtr-sync-bc.md` mechanism is also implemented. If only one of the
   two ships, this spec degrades gracefully: just normalize `red` and ignore
   the `red2` path. The percentile mechanism is independently valuable.
5. **Frequency search range `ia..=ib`**: pancetta's existing decoder uses a
   configurable audio passband (e.g. 200–3000 Hz). The percentile must be
   computed over the *same* range that downstream candidate selection uses;
   computing it over a wider range would bias the floor low. The implementer
   should pass the existing passband bin range directly into the percentile
   step.

## Estimated Rust port effort

- ~40–80 LOC in the sync-thresholding portion of `pancetta-ft8/src/decoder.rs`.
  Trivial sort + index pick + in-place divide. Two tiny helpers; no new
  module needed.
- 1 unit test confirming the percentile index calculation against a hand-rolled
  sorted array of known length.
- 1 unit test confirming all-zero input is gracefully ignored.
- 1 session implementation + tests.
- 1 session for hard-200 retune of `sync_min` and AUC comparison vs. existing
  normalization.
- Total: 1–2 iter sessions.

## Implementation notes for the implementer thread

- Sort a *copy* of the per-bin metric vector; never sort the in-place vector
  that downstream code indexes by frequency bin.
- Use `f32::partial_cmp().unwrap_or(Equal)` to dodge NaN-induced sort panics;
  any NaN entries should be treated as if they were zero (sort to the front).
- The percentile index calculation in plain Rust:
  `let idx = ((0.40_f32 * (len as f32)).round() as usize).max(1) - 1;`
  Treat this formula as derived; the spec only fixes the fraction (0.40), the
  clamp (≥1), and the zero-base subtraction (`- 1`).
- Skip the divide when `base <= 0.0 || !base.is_finite()`. Do not panic, do
  not divide by epsilon. Downstream threshold will correctly reject all
  candidates.
- Two independent baselines: one for the full-Costas vector, one for the
  partial-Costas vector. Do not share. The partial-Costas distribution has a
  different scale because it sums over fewer symbols.
- Pancetta should keep its existing noise-floor estimate available for one
  iter cycle as a fallback; toggle the percentile path behind a `bool` config
  for the first eval. Once hard-200 confirms recall does not regress, remove
  the fallback.
- Do **not** chase the doc summary's depth-dependent scaling or
  proximity-dedup suggestions; both were inferred-not-actual (confirmed by
  direct source read).
