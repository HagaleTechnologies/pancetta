# Algorithm spec: JS8Call-Improved LLR whitening (noise normalisation)

## Source attribution

- Origin: JS8Call-Improved (https://github.com/JS8Call-improved/JS8Call-improved)
- File path (for traceability, NOT to be quoted): `JS8_Mode/whitening_processor.h`
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

The LDPC decoder works best when LLRs are on a comparable scale across
bit positions. Real FT8 / JS8 audio rarely provides that: the noise
floor varies by frequency (band edge vs. middle) and by time (QRN
bursts, neighbouring strong stations bleeding into nearby tones). If
the symbol demapper hands raw magnitude-derived LLRs to LDPC, some
bits end up dominated by their local-noise scale rather than by their
actual reliability.

The "whitening" pass estimates noise *separately* per-tone and per-
symbol, normalises the LLRs by the geometric mean of those two
estimates, optionally erases the bits that fall below an erasure
threshold after normalisation, and finally standardises the surviving
LLRs by their global variance. The output is a comparable, decoder-
friendly LLR stream.

The name "whitening" refers to noise whitening in the LLR
distribution (giving the LLR vector a roughly unit-variance
appearance for the decoder), not to spectral whitening of the audio.

## Algorithm description (PROSE ONLY)

### Inputs

- A symbol-magnitude matrix of shape `8 × ND` (rows are the eight
  GFSK tones; columns are the ND data-symbol positions; sync symbols
  are excluded by the caller).
- An array of winning tone indices, one per symbol position (`ND`
  entries), each in `[0, 8)`. These are the demapper's hard
  decisions: "the loudest tone in this symbol slot".
- An erasure threshold (scalar). May be disabled via an env-var-style
  knob.
- A debug-print flag (not part of the algorithm; safe to ignore in
  the port).

### Outputs

A result structure carrying:

- `llr0`, `llr1`: two normalised LLR vectors, each length `3 × ND`
  (three bits per 8-FSK symbol). The two are produced from
  different "winner-minus-runner-up" pair statistics; LDPC sees both
  and uses their better-conditioned one (or sums them as a follow-on
  refinement).
- A whitening-applied flag and an erasure-applied flag (so the
  downstream stages know which path the LLRs came from).
- A count of erased LLRs.
- Pre-normalisation and post-normalisation magnitude statistics for
  telemetry.

### Steps

1. **Per-tone noise estimation**. For each of the 8 tone rows,
   compute a *median* of the magnitudes across all ND symbol positions
   where that tone is *not* the winner. Median (not mean) because
   medians are robust to a small number of strong contaminants. Call
   this vector `n_tone[0..8]`.
2. **Per-symbol noise estimation**. For each of the ND symbol slots,
   compute a *median* over the 7 non-winning tone magnitudes. Call
   this `n_symbol[0..ND]`.
3. **Divisive normalisation**. For each `(tone, symbol)` position
   that contributes to LLR formation, divide the magnitude by the
   geometric mean of `n_tone[tone]` and `n_symbol[symbol]`:
   `mag_norm = mag / sqrt(n_tone × n_symbol)`. This pulls both rows
   and columns of the matrix toward a unit-noise scale.
4. **LLR formation**. From the normalised magnitudes, produce two LLR
   vectors using the standard winner-minus-runner-up family of
   approximations (or whatever the host demapper uses; the
   normalisation step is the load-bearing change, not the LLR
   formula). The "two LLR estimates" (`llr0`, `llr1`) come from
   running the LLR formula against the *first* and *second* runners-up
   independently. Both are returned.
5. **Optional erasure**. For each LLR whose absolute value is below
   the erasure threshold (post-normalisation), force it to zero.
   Increment the erased counter. May be disabled.
6. **Variance standardisation**. Compute the variance of the
   surviving (non-zero) LLR values. Divide every LLR by the square
   root of that variance. This gives the LLR distribution unit
   variance, which is the regime LDPC's check-node updates are tuned
   for.
7. Return both LLR vectors plus the flags and counters.

### Numerical constants (facts, not expression)

- Tone count: 8 (8-GFSK).
- LLR vector length: `3 × ND` (three bits per symbol).
- Per-tone median is over `ND` positions (minus the count of those
  where the tone was a winner; in practice ND minus a small fraction
  ≈ ND/8).
- Per-symbol median is over 7 magnitudes per symbol.
- The erasure threshold is a small fraction of the post-
  normalisation LLR scale; the JS8Call-Improved source exposes it as
  a tunable env var. Start at ~0.5 for the implementer port.
- The variance standardisation uses the *sample* variance, not the
  population variance — but with `3 × ND ≈ 174` LLRs in flight the
  difference is negligible.

### Edge cases

- All-zero LLRs after erasure: catastrophic for LDPC. Cap the
  erasure rate; if more than, say, 50% of LLRs are erased, skip the
  erasure step and let LDPC see the full normalised set. (The
  implementer should make this conditional on a configurable max-
  erasure-fraction parameter.)
- Single dominant interferer on one tone: the median estimator for
  that tone row is robust against the interferer's symbol positions,
  but if the interferer occupies most of the slot, the per-tone
  median becomes large and the normalisation under-amplifies that
  tone's true winners. Acceptable behaviour: those symbols' LLRs
  will be small and the LDPC decoder will lean on parity to fill
  them, which is the right outcome.
- Whitening-disabled path (env knob): all of step 3 is skipped; LLRs
  are formed from raw magnitudes; step 6 still runs to give LDPC a
  consistent scale.
- Numerical zero in the denominator (a tone with no observed
  magnitude): use a small floor (e.g., `1e-6`) on `n_tone` and
  `n_symbol` to avoid division-by-zero.

## Conflict with pancetta's existing mechanisms

- Pancetta-ft8 currently uses a single LLR vector (not a pair) and a
  simpler global noise normalisation. The whitening pass would
  replace the global normalisation with a per-tone × per-symbol
  median-based one, and add the second LLR vector.
- The LDPC consumer must learn to take a pair (or to combine the
  pair before feeding LDPC). The implementer thread should validate
  on the hard-200 corpus that `llr0 + llr1` (or whichever combination
  the JS8Call-Improved decoder uses) outperforms either alone.
- Compatible with multi-pass SIC: the whitening step runs per
  candidate per pass; SIC operates on the residual audio, not on the
  LLRs.
- Stacks with the LDPC feedback refinement (separate spec): whitening
  shapes the input distribution, feedback refines based on the
  intermediate decode. Different lever, same downstream stage.

## Estimated Rust port effort

- ~180 LOC including the median computation (use a partial-sort or
  `quickselect` for O(n) median), the geometric-mean step, the
  optional erasure pass, the variance standardisation, and tests.
- 1–2 implementer sessions.

## Implementation notes for the implementer thread

- New module: `pancetta-ft8/src/decoder/whitening.rs`.
- Public API:
  - `struct WhiteningOutput { llr0: Vec<f32>, llr1: Vec<f32>,
    whitening_applied: bool, erasure_applied: bool, erased_count:
    usize, stats: WhiteningStats }`
  - `fn whiten_llrs(mags: &[[f32; 8]], winners: &[u8], cfg:
    &WhiteningConfig) -> WhiteningOutput`
- `WhiteningConfig`:
  - `enabled` (default false until hb-bench validates).
  - `erasure_threshold` (default 0.5, in normalised-LLR units).
  - `max_erasure_fraction` (default 0.5).
  - `noise_floor` (default 1e-6, divisor floor).
- Median computation: use the `pdqselect` crate or write a small
  median-of-medians; either way, do it in-place on a scratch buffer
  to avoid per-symbol allocations.
- Where to insert: in `decoder.rs`, between symbol-magnitude
  extraction and LDPC entry. Currently the LLR vector flows
  straight from magnitudes; reroute through `whiten_llrs` when
  enabled.
- Telemetry: log `WhiteningStats { erased_count, pre_mag_p50,
  pre_mag_p95, post_mag_p50, post_mag_p95 }` per candidate. Surface
  via the research scorecard so we can A/B against shelf.
- Bench gate: new hypothesis bank entry. Suggest `hb-222` or
  next-free. Expected effect: improved recall on band-edge signals
  (where local noise is non-uniform) and on slots with one strong
  station bleeding into neighbours.
