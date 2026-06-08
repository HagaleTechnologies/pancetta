# Algorithm spec: JTDX between-cycle 2-tap audio smoothing

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File paths (for traceability, NOT to be quoted):
  - `lib/ft8_decode.f90` (the `ipass == 4` and `ipass == 7` branches
    at lines 192-213 that replace the working `dd8` buffer with a
    2-tap moving-average smoothed copy)
  - `lib/sync8.f90` (consumer: the next pass's spectrogram is built
    from the smoothed `dd8`, see lines 13-64)
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

JTDX runs up to nine "passes" of sync + decode per slot, grouped into
three "cycles" of three passes each (driven by `nft8cycles ∈ {1, 2,
3}`). Between cycle 1 and cycle 2, and again between cycle 2 and
cycle 3, JTDX replaces the in-memory audio buffer `dd8` with a 2-tap
moving-average smoothed copy of itself. The smoothed buffer is then
fed into the next cycle's sync detector and inner decoder. The
effect is a mild low-pass filter on the audio that suppresses high-
frequency noise components, making weak signals stand out
differently in the sync correlator.

This is a *low-cost recall multiplier*: it costs one pass through
180 000 floats (~720 KB) per cycle transition, and it makes the
second and third decode cycles operate on a different signal-noise
distribution than the first. Any signal that was just below
detection threshold in the original audio but stands above it in the
smoothed version is recovered in cycle 2; any signal blocked by a
specific noise transient may decode cleanly in cycle 3.

## Algorithm description (PROSE ONLY)

### Inputs

- `dd8`: the working audio buffer (real, 180 000 samples at 12 000
  sps = 15 s slot). Mutated in-place.
- `nft8cycles`: ∈ {1, 2, 3}. Maps to `npass ∈ {3, 6, 9}`.
- `ipass`: the current pass index.

### Outputs

- Side effect: `dd8` is mutated to a smoothed version of itself,
  and (in the 3-cycle case) the original audio is preserved in
  `dd8m` for later restoration.

### Steps

The pass loop reaches the transition logic at the *start* of pass 4
and pass 7. Between passes 3 and 4 (cycle 1 → cycle 2):

1. If `npass == 9` (3-cycle mode), allocate a save buffer `dd8m` of
   size 180 000 and copy the current `dd8` into it. This preserves
   the original (cycle-1) audio for later use.
2. Apply a 2-tap forward moving-average over `dd8` in-place:
   `dd8(i) = (dd8(i) + dd8(i+1)) / 2` for `i = 1..179999`.
3. The 180 000th sample is left unchanged (no `i+1` exists for it).
4. Continue with pass 4. From here through pass 6 (cycle 2),
   `sync8` and `ft8b` operate on the smoothed audio.

Between passes 6 and 7 (cycle 2 → cycle 3):

1. If `dd8m` was allocated (3-cycle mode), reconstruct `dd8` from
   `dd8m` using a 2-tap backward moving-average:
   `dd8(1) = dd8m(1)`, then for `i = 2..180000`:
   `dd8(i) = (dd8m(i-1) + dd8m(i)) / 2`.
2. Deallocate `dd8m`.
3. Continue with pass 7. From here through pass 9 (cycle 3),
   `sync8` and `ft8b` operate on the *backward*-smoothed original
   audio.

The transition is wrapped in OpenMP `barrier` + `single` directives
to ensure exactly one thread performs the mutation and all threads
wait for it before proceeding.

### Numerical constants (facts, not expression)

- Cycle-2 smoothing: forward 2-tap MA, `dd8(i) = (dd8(i) + dd8(i+1))
  / 2` for `i = 1..179999`. Sample 180 000 unchanged.
- Cycle-3 smoothing: backward 2-tap MA from the *preserved original*
  `dd8m`: `dd8(1) = dd8m(1)`, then `dd8(i) = (dd8m(i-1) + dd8m(i))
  / 2` for `i = 2..180000`.
- Buffer size: 180 000 samples (15 s × 12 000 sps).
- Sample type: single-precision float (`real`).
- Smoothing only occurs when `npass ≥ 6` (cycle 1 → 2 transition)
  and `npass == 9` (cycle 2 → 3 transition). Single-cycle mode
  (`npass = 3`) never smooths.

### Edge cases

- The 2-tap MA has a frequency response of `H(f) = cos(π f / fs)`
  where `fs = 12 000 sps`. At the FT8 audio band (300 - 3000 Hz),
  the attenuation is mild: about 0.03 dB at 300 Hz, 3 dB at 3000 Hz.
  Above ~6000 Hz (Nyquist), attenuation rises rapidly.
- The forward MA and backward MA are *not* commutative — applying
  both in sequence would result in a 3-tap symmetric kernel with
  centre weight 0.5 and edges 0.25 each. JTDX never applies both in
  sequence; it always reverts to the original audio (`dd8m`) before
  applying the backward MA.
- The +0.5-sample group delay of the forward MA shifts every signal
  in the slot by ~83 µs. The Costas sync window is ~62.5 ms wide
  (`tstep = 0.04 s` × ±2.5 s), so the group delay is negligible for
  sync detection. Symbol extraction is also tolerant — the symbol
  window is 1920 samples wide (160 ms).
- Edge samples (sample 1 in forward MA, sample 180 000 in backward
  MA) are handled by skipping the update (forward) or copying the
  preserved original (backward). The discontinuity is one sample
  wide and has no detectable downstream effect.
- The smoothing is **destructive** to the original `dd8` after
  cycle 1. The `dd8m` allocation in 3-cycle mode is the only way
  cycle 3 sees anything close to the original audio.

## Conflict with pancetta's existing mechanisms

Pancetta currently runs a single decode pass (`max_decode_passes = 1`
on Slow tier per hb-216; higher on Fast/Moderate). The pass loop
exists but does not implement between-cycle audio mutation.

The mechanism composes cleanly with pancetta's existing structure:

1. **Cheap to add** when `max_decode_passes > 1`: pancetta already
   has a multi-pass loop in `pancetta-ft8`'s decoder coarse-search
   section. Adding the smoothing requires only a single in-place
   MA at the boundary between pass-group invocations.
2. **Composes with subtract-and-re-decode (SIC)**: pancetta's
   existing SIC subtracts decoded signals from `dd8`. The smoothing
   is *applied after* SIC and before the next pass; the two are
   orthogonal.
3. **Composes with the 3-method magnitude sweep** (see
   `spec-jtdx-3method-sweep.md`): JTDX's 3-method sweep operates
   on the smoothed audio in cycles 2 and 3. The smoothing is a
   prerequisite for getting the most out of repeating the same
   metric three times across cycles — without the smoothing, passes
   1, 4, 7 would all see the same audio and produce nearly identical
   candidates.
4. **Mild risk of false signals from filter ringing**: a 2-tap MA
   has no ringing in the time domain (it's an FIR with two equal
   non-negative taps), so the FP risk is structurally minimal.

The most useful place to wire this is *between pancetta's pass-group
iterations* in the multi-pass scoped fast-path. On the Fast tier
where multi-pass is enabled, the smoothing is a free recall lift; on
the Slow tier where only one pass runs, it never fires.

## Estimated Rust port effort

- ~40 LOC to add the 2-tap forward MA and the save-and-restore
  backward MA in pancetta-ft8's decoder pass-loop. Use
  `slice::windows(2)` for the MA, careful to not double-touch any
  sample.
- ~20 LOC for the `Vec<f32>` save buffer (only allocated when
  3-cycle mode is configured).
- ~30 LOC of tests confirming the mutations are correct and the
  restoration round-trips (small drift is allowed at the order of
  1 sample / 180 000).
- 1 session.

Total: ~90 LOC, 1 session.

## Implementation notes for the implementer thread

- This is one of the cheapest mechanisms in this batch. Implement
  whenever pancetta's `max_decode_passes` exceeds 1.
- Use an in-place transformation. The temporary `dd8m` save buffer
  is only needed if pancetta supports 3-cycle mode; for 2-cycle
  mode, the destructive forward MA is enough.
- The mechanism doubles as a CPU-cost driver: in single-cycle mode
  it's a no-op, in 2-cycle mode it costs 180 000 multiply-and-add
  (~0.5 ms on modern CPUs), in 3-cycle mode it costs ~1 ms total.
  These costs are negligible compared to the sync detector itself
  (which dominates per-pass cost).
- Add a config knob `audio_smoothing_between_cycles: bool` defaulting
  to true. The reason to allow disabling is to scorecard the recall
  delta cleanly — compare on/off across hard-200 to confirm the lift.
- Critical: the mechanism's recall lift comes from the *combination*
  with the multi-pass + multi-magnitude-metric pipeline. Shipping the
  smoothing without the multi-pass loop or without the 3-method
  sweep (`spec-jtdx-3method-sweep.md`) gives a much smaller lift.
- Cross-reference: this is part of a family of "perturb the input
  audio between passes" mechanisms. The 3-method sweep (RSS / power
  / L1) perturbs the magnitude metric. The smoothing perturbs the
  audio itself. The lreverse flag (passes 2, 5, 7) swaps forward and
  reversed-conjugate symbol matrices. Together they provide ~9
  structurally different "looks" at the same slot per cycle.
