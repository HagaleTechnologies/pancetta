# Algorithm spec: sync_bc partial Costas metric

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- File path (traceability only, NOT quoted): `crates/jt9r/src/sync.rs`, function pair `sync_abc` / `sync_bc` invoked inside `sync_at`
- Companion doc: `docs/jt9r.md` (architecture summary)
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

FT8 frames carry three identical 7-symbol Costas synchronization arrays at symbol
offsets 0, 36, and 72 of a 79-symbol burst. A "full" sync metric correlates the
spectrogram against all three arrays simultaneously. The problem: when a signal
arrives early enough that the first Costas array falls outside the recorded
audio window (the negative-dt slot-edge case — the operator started recording
slightly late, or the remote transmitter started slightly early relative to the
local clock), the first 7 symbols contribute either silence, noise, or
out-of-window garbage to the full metric, drowning the signal. The same goes for
audio that is heavily clipped or AGC-disturbed at the leading edge.

The `sync_bc` partial Costas metric is a fallback that scores using only the
*second* and *third* Costas arrays. When the leading edge is missing, the
partial metric is materially larger than the full metric, and a `max(full,
partial)` selection rescues the candidate. When the leading edge is present and
healthy, the full metric dominates and nothing changes. The mechanism is
non-destructive: it never *replaces* a healthy full-metric decision, only
*backstops* a degraded one.

In pancetta terms, the headroom is the negative-dt slot-edge bucket called out
in the MEMORY: "dt: slot-edge (negative dt) at 48.3% recall (1376 truths)".

## Algorithm description (PROSE ONLY — no code)

### Inputs

- A complex/magnitude spectrogram grid `S[t, f]` already built from the audio
  window, where `t` indexes spectrogram frames spaced by `NSTEP = 480` audio
  samples (40 ms at 12 kHz), and `f` indexes frequency bins at the
  spectrogram's resolution. The audio sample rate is 12 kHz; NSPS (samples per
  symbol) is 1920, NFFT is 3840 (i.e. 2×NSPS, so two spectrogram frames per
  symbol).
- A lag offset `lag` (signed integer count of spectrogram frames; the sync
  search sweeps this from `-62` to `+62` relative to the nominal sync position,
  spanning ~±2.5 seconds).
- A base frequency bin index `f0`.
- The 7-tone FT8 Costas pattern, a constant permutation of tone indices in the
  range 0..7.

### Outputs

- A single non-negative scalar sync metric for the `(lag, f0)` pair. Higher is
  better.

### Steps

The sync routine computes **two** ratios over the same spectrogram region and
returns the larger of the two.

1. Locate the three Costas array regions in the spectrogram. Each array spans
   7 consecutive symbol slots:
   - Block A: symbol slots 0 through 6 (frames at the nominal start).
   - Block B: symbol slots 36 through 42 (mid-burst).
   - Block C: symbol slots 72 through 78 (end of burst).
   Each symbol slot corresponds to 2 spectrogram frames (because NFFT = 2×NSPS).
   The frame address for the k-th symbol of block X is the nominal start
   plus `lag` plus `2 * (symbol_offset + k)`.

2. For each block X in {A, B, C}, compute two power sums by walking the 7
   Costas-permutation tones:
   - `signal_X`: sum across the 7 Costas symbols of the spectrogram bin power
     at the *correct* tone bin (i.e. the tone the Costas pattern says should
     be present at that symbol slot). The "correct" frequency bin is
     `f0 + tone_index_for_this_slot * bins_per_tone`. Each tone is one tone
     spacing apart (about 6.25 Hz; equivalently, the bin spacing of the
     NFFT=3840 spectrogram).
   - `total_X`: sum across the same 7 symbols of the spectrogram bin power
     summed across *all 8 candidate tone bins* at that symbol (i.e. the
     denominator surveys the full 8-tone alphabet width, not just the assigned
     tone).

3. Compute the noise-floor estimate per block as `(total_X - signal_X) / 6`.
   The divisor 6 is "8 alphabet tones minus the 1 signal tone, minus the 1
   correctly-weighted half-bin self-bias" — empirically a per-bin average over
   the alphabet excluding the signal tone, normalized to be on the same scale
   as `signal_X / 7`. (Treat 6 as a fact: it is the constant the source uses
   and it makes the full-metric and partial-metric comparable.)

4. **Full metric `sync_abc`**:
   - Numerator: `signal_A + signal_B + signal_C`.
   - Denominator: `(noise_A + noise_B + noise_C)` using the per-block
     noise-floor estimate from step 3 (so:
     `((total_A + total_B + total_C) - (signal_A + signal_B + signal_C)) / 6`).
   - If the denominator is strictly positive, the metric is numerator divided
     by denominator. Otherwise the metric is defined as zero.

5. **Partial metric `sync_bc`**:
   - Numerator: `signal_B + signal_C`. Block A is *not* used.
   - Denominator: `((total_B + total_C) - (signal_B + signal_C)) / 6`.
   - If the denominator is strictly positive, the metric is numerator divided
     by denominator. Otherwise the metric is defined as zero.

6. **Selection rule**: the sync routine returns `max(sync_abc, sync_bc)`.
   No explicit gating condition such as "if block A is out of window, use
   partial" is required: the max() naturally selects whichever ratio is larger.
   When block A contains real signal, including it in both numerator and
   denominator strictly improves the full ratio over the partial ratio in
   expectation. When block A contains only noise (the slot-edge case), it
   inflates the denominator more than the numerator and the partial ratio
   wins.

### Numerical constants (facts, not expression)

- Costas array length: 7 symbols.
- FT8 burst length: 79 symbols total.
- Costas block offsets within the burst (in symbols): 0, 36, 72.
- Samples per symbol (NSPS): 1920 (at 12 kHz, this is 160 ms per symbol).
- FFT length (NFFT): 3840 (covering 2 symbols, so symbol-level addressing in
  the spectrogram uses a stride of 2 frames per symbol).
- Spectrogram step (NSTEP): 480 audio samples (40 ms per frame; 4 frames per
  symbol of step, 2 of which lie within any single symbol's NFFT window).
- Lag sweep range: `[-62, +62]` spectrogram frames around the nominal sync
  position (~±2.5 s).
- Noise-floor normalization divisor: 6 (one less than the 7 non-signal
  alphabet tones).
- Sample rate: 12 kHz.

### Edge cases

- **Denominator non-positive**: both metric branches return 0 if the
  noise-floor estimate is non-positive. This prevents NaN/Inf from a
  near-saturated bin and causes the candidate to be naturally rejected by the
  downstream `sync_min` threshold.
- **Out-of-window block addressing**: if the lag and block offset would
  address a spectrogram frame outside the recorded window, the source treats
  that as zero contribution (the spectrogram is effectively zero-padded for
  addressing past the boundary). The `sync_bc` fallback is the principled
  response to *exactly this* situation for block A; for block C falling off
  the end, no symmetric fallback exists in the source, but the burst-end case
  is rarer because slot-late audio is usually padded by the next slot's
  capture.
- **NaN / non-finite metric**: the downstream threshold check rejects
  non-finite values.

## Conflict with pancetta's existing mechanisms

Pancetta's sync stage lives in `pancetta-ft8/src/decoder.rs` and uses the
ft8_lib-derived sync metric: it correlates the spectrogram against all three
Costas arrays at once and treats the score as a single ratio. There is no
partial-Costas fallback today. The existing metric is "block-A required" — when
block A is missing or corrupted, the score collapses and the candidate is
dropped well before LDPC ever sees it.

The hb-091 scoped-fast-path work (already shipped) cuts wall-clock cost in the
sync hot loop but does **not** change the metric definition. The hb-216 hardware
tier classifier flips presets but does not add a partial metric. So the partial
metric is a genuinely new mechanism for pancetta.

Possible conflicts to think through:

1. **Costas array indexing**: pancetta's Costas pattern is the same FT8
   permutation as wsjtr's (both follow the FT8 protocol spec), so no
   re-derivation is needed. Verify the in-code constant matches before wiring.
2. **Bin-power vs. magnitude**: confirm whether pancetta's spectrogram stores
   linear power, magnitude, or log-magnitude. The wsjtr ratios are over
   **linear power**. If pancetta currently sums log-magnitudes, the ratio
   semantics break — implementer must add an exp() or switch to linear sums
   for this metric specifically.
3. **False-positive risk**: by accepting a strictly *less* informed metric
   (sync_bc uses 14 symbols instead of 21), the FP rate could rise. Mitigation
   is already in place in pancetta: hb-062 (callsign-trust), hb-058
   (/R-suffix filter), hb-103 (content score) sit downstream. The expectation
   from MEMORY's slot-edge bucket is that the *recall* gain at negative dt is
   large enough to be worth the marginal FP exposure, especially because the
   hb-062 / hb-103 stack is already conservative.
4. **Interaction with hb-115/hb-100 capture-effect work**: orthogonal — that
   work changes how *adjacent* signals interact, this work changes what
   metric we use when the leading edge of the *current* signal is missing.

## Estimated Rust port effort

- ~60–120 LOC in `pancetta-ft8/src/decoder.rs` (or wherever the sync metric
  is computed). Most of the existing sync routine can be left alone; we add a
  second accumulator pair `(signal_bc, total_bc)` alongside the existing
  three-block sums, compute the partial ratio, and return the max.
- 1 session for the implementation + unit tests.
- 1 session for eval on hard-200 with the slot-edge bucket isolated.
- Total: 1–2 iter sessions.

## Implementation notes for the implementer thread

- The natural splice point is wherever pancetta's sync routine currently
  accumulates `(signal, total)` for the three Costas blocks. Add a parallel
  accumulator pair restricted to blocks B and C, divide by the same `/6`
  noise-floor convention, and return `max(full_ratio, partial_ratio)`. Do not
  add a gating heuristic; let max() do the work.
- The candidate output downstream consumes a *single* sync score and a
  threshold; no contract change is needed there.
- For the unit test, build a synthetic FT8 burst, zero out symbols 0..6
  (block A), confirm that the partial metric stays meaningful while the full
  metric collapses, and confirm the returned metric equals the partial branch.
  A second test should confirm a clean burst returns the full branch.
- Place the new metric behind no feature flag for the first eval; if hard-200
  shows a FP regression on the non-slot-edge bucket, gate behind a config
  toggle and ship to slot-edge-only via a dt-prior. Initial expectation
  (matching MEMORY's hypothesis bank) is that the win on slot-edge is clean
  enough to ship unconditionally.
- Citation hygiene: this mechanism originates in wsjtr (clean-room read by
  the reader thread); cite as `pancetta-invented` only if the implementer
  arrives at the same algorithm independently — otherwise label
  `wsjtr-inspired` in the journal entry.
