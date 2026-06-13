# Algorithm spec: Three-stage sync cascade (coarse → second → third)

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — `coarse()` around lines 585-628 + driver in
  `go()` around lines 864-930; `search_both()` around lines 1130-1158
  (second stage); `search_both_known()` around lines 1160-1214 (third
  stage); call sites `one_iter` around lines 2380-2409 and
  `try_decode` around lines 2948-2967.
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

ft8mon's sync is a three-stage cascade with progressively finer
resolution and progressively more expensive per-candidate work:

1. **Coarse** — wide sweep over the entire band × wide time window,
   bin-aligned (and optionally sub-bin per the sub-bin Costas spec).
   Per-candidate cost is one Costas correlation; per-slot count is
   thousands of candidates. Yields a sorted list of strongest
   `(hz, off)` candidates.
2. **Second** (`search_both`) — narrow refinement around each coarse
   candidate's `(hz, off)`. Per-candidate cost is one nested
   `hz_n × off_n` search (default `8 × 10 = 80` correlation
   evaluations). Yields a small list (default 3) of refined
   candidates per coarse hit.
3. **Third** (`search_both_known`) — *post-decode* refinement using
   the known 79-symbol sequence from the LDPC output. Run after a
   candidate decodes successfully, this stage uses the *true* symbol
   sequence (not a guess) to maximize correlation, giving the
   tightest possible `(hz, off)` for subtraction.

The three stages exploit progressively more information: coarse has
only the Costas pattern (21 symbols of known tones); second adds
nothing new but refines the search resolution; third adds the
remaining 58 data symbols' known tones because LDPC has already
identified them. Each stage's resolution is approximately one order of
magnitude finer than the previous (coarse: 6.25 Hz × 160 ms; second:
~0.875 Hz × 16 ms; third: ~0.083 Hz × ~1 sample).

The structure is not optional — without the third stage, subtraction
residual quality drops dramatically because the Costas-only
refinement leaves residual `(hz, off)` errors that prevent clean
cancellation.

## Algorithm description (PROSE ONLY)

### Stage 1: Coarse sync (`coarse()` driven from `go()`)

#### Inputs
- The full slot's per-symbol FFT array `bins[symbol_index][tone_bin]`.
- Symbol-index search range `[si0, si1]` (the wide
  `tminus`/`tplus` window from the related spec).
- Optional sub-bin oversampling factors `coarse_hz_n`, `coarse_off_n`
  (per the sub-bin Costas spec).

#### Steps
1. For each frequency bin `bi` in `[min_bin, max_bin)`:
   - For each symbol index `si` in `[si0, si1)`:
     - Compute `one_coarse_strength(bins, bi, si)` — the Costas
       correlation at this bin and offset.
   - Sort the per-bin offset list descending by strength.
   - Retain the top `ncoarse` offsets per bin (default 1), with the
     constraint that retained offsets must be at least
     `ncoarse_blocks` symbol-times apart (default 1).
2. Pool the per-bin retained candidates into one global list.
3. Sort the global list descending by strength.
4. Apply the `already[]` exclusion (decodes from earlier in this
   pass at the same `already_hz`-resolution frequency are skipped).
5. Hand the surviving sorted list to the per-candidate decode
   pipeline.

Output resolution: 6.25 Hz × 160 ms (bin × symbol). With sub-bin
oversampling: 1.5625 Hz × 40 ms.

### Stage 2: Second-stage refinement (`search_both()`)

#### Inputs
- The 200-sps `samples200` buffer for one candidate, frequency-
  shifted to put the candidate near 25 Hz.
- The candidate's coarse `(hz0, off0)` (here `hz0 ≈ 25`,
  `off0` from the coarse sweep mapped into 200-sps).

#### Steps
1. Loop over `hz` from `25 - second_hz_win` to `25 + second_hz_win`
   in `2 × second_hz_win / second_hz_n` increments. With
   `second_hz_win = 3.5 Hz` and `second_hz_n = 8`, this gives
   `hz_inc = 0.875 Hz` and 9 frequency probes.
2. At each `hz`, call `search_time_fine(samples200, off0 ± off_win, hz, off_inc, &str)`:
   - Sweep `off` in `2 × off_win / off_n` increments. With
     `second_off_win = 0.5` (symbol-times = 16 samples at 200 sps)
     and `second_off_n = 10`, this gives `off_inc ≈ 3 samples` and
     11 time probes.
   - At each `(hz, off)`, compute Costas correlation strength.
   - Return the `(off, strength)` of the maximum.
3. Collect every `(hz, off, strength)` triple into a `Strength`
   vector.
4. Sort descending by strength.
5. Hand the top `second_count = 3` candidates to `one_iter1`.

Output resolution per candidate: ~0.875 Hz × 16 ms (or finer if
`second_hz_n` / `second_off_n` are tuned up).

### Stage 3: Third-stage post-decode refinement (`search_both_known()`)

#### Inputs
- The full-rate (12 kHz) `samples_` buffer.
- The candidate's `(best_hz, best_off)` after second stage and
  successful decode.
- The 79-symbol `re79[]` sequence reconstructed by `recode()` from
  the LDPC output.

#### Steps
1. Compute a single FFT of `samples_` once (reused across the
   frequency sweep).
2. Loop over `hz` from `best_hz - third_hz_win` to
   `best_hz + third_hz_win` in `third_hz_n` increments. With
   `third_hz_win = 0.25 Hz` and `third_hz_n = 3`, this gives
   `hz_inc = 0.25 Hz` (3 probes).
3. At each `hz`, call `search_time_fine_known(bins, rate, syms, off0 ± off_win, hz, gran, &str)`:
   - This is the variant that uses the **known** symbol sequence
     `re79[]` instead of just the Costas blocks. It applies a fine
     frequency shift via FFT bin rotation (see
     `fft_shift_f`-based helpers) to align the candidate to the
     nearest bin center, then sweeps `off` with `gran` step,
     evaluating `one_strength_known()` at each — which scores all
     79 symbols against their known tones, not just the 21 Costas
     symbols.
   - With `third_off_win = 0.075 × block` and `third_off_n = 4`,
     this is ~3 time probes at full sample rate.
4. Pick the `(hz, off)` of the maximum strength.

Output resolution per candidate: ~0.083 Hz × 1 sample.

### How the cascade is invoked

```text
go():
  coarse(): emit ~thousands of (hz, off) candidates per slot
  for each candidate (sorted by strength):
    one():
      down_v7_f(): downsample to 200 sps centered at 25 Hz
      one_iter():
        search_both(): refine to ~0.875 Hz × 16 ms; emit top 3
        for each (hz, off, strength):
          one_iter1():
            shift200(): center at exactly 25 Hz
            extract(): build m79
            fine(): symbol-to-symbol phase refinement (separate spec)
            soft demods → LDPC → CRC
            if decoded:
              try_decode():
                if do_third == 2: search_both_known() at full rate
                emit + subtract
                return success
            (else: try hints, then move to next second-stage candidate)
```

### Numerical constants (facts, not expression)

Stage 1 (coarse):
- `ncoarse = 1` — candidates retained per bin.
- `ncoarse_blocks = 1` — minimum spacing between retained candidates
  in the same bin.
- `coarse_strength_how = 6` — `signal / noise` strength metric.
- `already_hz = 27 Hz` — exclusion zone after a successful decode.

Stage 2:
- `second_hz_win = 3.5 Hz`, `second_hz_n = 8` → 0.875 Hz resolution,
  9 probes.
- `second_off_win = 0.5` symbol-times = 16 samples (200 sps),
  `second_off_n = 10` → ~3 samples = 15 ms resolution, 11 probes.
- `second_count = 3` — top candidates retained.
- `do_second = 1` — gate.

Stage 3:
- `third_hz_win = 0.25 Hz`, `third_hz_n = 3` → 0.25 Hz resolution,
  3 probes. (Note: with `third_hz_n = 3` and `third_hz_win = 0.25`,
  the inc is `2 × 0.25 / 2 = 0.25 Hz`.)
- `third_off_win = 0.075` symbol-fraction (~144 samples at 12 kHz,
  ~2.4 samples at 200 sps),
  `third_off_n = 4` → ~0.5 sample resolution at 12 kHz.
- `do_third = 2` — 0=off, 1=at 200 sps before emit, 2=at full rate
  before subtract.
- `known_strength_how = 7` — phase-aware strength metric (sums
  symbol-to-symbol phase delta magnitudes; see
  `one_strength_known`).
- `known_sparse = 1` — process every symbol (1 = no sparsification;
  higher values sub-sample for speed).

### Edge cases
- **Empty per-bin sweep** in coarse — if `sv.size() < 1` at any bin,
  the outer loop breaks. This can happen when the band edge clips
  candidates; benign.
- **`already_hz` collision** in coarse — multiple sub-bin candidates
  on the same signal get pruned to one. The `already[]` array is
  written **only when a decode succeeds**, not at coarse-sort time,
  so the second-strongest candidate at a slightly different sub-bin
  still gets tried if the strongest fails LDPC.
- **Third stage at full rate** is *significantly* more expensive
  than at 200 sps because the FFT size is 60× larger. Only run it
  for confirmed decodes (post-LDPC, post-CRC), where the cost is
  amortized over the bigger benefit: clean subtraction. The
  `do_third = 1` variant skips this and refines only at 200 sps.
- **Wrong-tone Costas in coarse** vs **correct symbols in third** —
  the third stage cannot improve a candidate that failed earlier
  stages because it requires the LDPC-decoded symbols. It is
  emphatically not a "rescue" stage for failed decodes.
- **Re-search past the decode** — `try_decode` calls
  `search_both_known` *after* LDPC + CRC succeed but *before*
  emitting and subtracting. This means the emitted `(hz, off)`
  reported to the callback is the third-stage refined value, not
  the second-stage value. Pancetta's downstream consumers should
  expect this precision in the emitted decode.
- **`do_third == 1` vs `2`** — option 1 refines in 200-sps space
  (cheap, ~0.5 Hz precision); option 2 refines in original-rate
  space (expensive, ~0.083 Hz precision). The expensive variant is
  default because subtraction quality is the load-bearing payoff.

## Conflict with pancetta's existing mechanisms

Pancetta's sync is currently a two-stage structure (coarse +
fine-correlation) per CLAUDE.md and the existing decoder. Adding the
**third (post-decode, known-symbol) stage** is the highest-value
piece of this cascade because it dramatically improves subtraction
residual quality. The hb-217 corpus-scale capture-effect finding
(0/1/2/3 neighbors → 76/43/27/15% recall) implies subtraction is
losing energy at signal boundaries. The third stage fixes this by
ensuring the subtracted waveform's (hz, off) match the received
waveform's within sub-Hz / sub-sample precision.

The **second-stage refinement** may already exist in pancetta in some
form (sub-bin correlation search). Worth a direct comparison; if
pancetta's existing fine-correlation sweep is denser or coarser than
ft8mon's `8 × 10 = 80`-probe grid, that's a tuning lever. ft8mon's
defaults are a good baseline.

Interaction with the sub-bin Costas spec
(`spec-ft8mon-sub-bin-costas.md`): the sub-bin Costas mechanism *is*
the modification to stage 1. The second and third stages run
unchanged regardless of whether sub-bin Costas is enabled at coarse.

Interaction with the fine() spec
(`spec-ft8mon-symbol-to-symbol-phase-fine.md`): `fine()` runs
**between** stage 2 and the soft demod. The cascade is more accurately
described as coarse → second → fine() → soft demod → LDPC → third
(post-decode). All four refinement steps compose.

Interaction with the Gaussian-ramp subtract spec: stage 3 is the
prerequisite for clean subtraction. Land stage 3 before tuning the
subtraction quality; otherwise the subtraction will leave
recoverable-looking residual that masks the actual subtraction
quality wins.

The cascade adds compute cost roughly proportional to candidate
count × stages. Stage 2 is ~80 probes per candidate; stage 3 is
~12 probes per *successful decode* (small). The biggest cost is
stage 2 multiplied by the candidate list size, which scales with
sub-bin Costas amplification. Tier gating:
- Slow: stage 1 only, no stage 2 sub-search; emit coarse candidates
  directly to fine().
- Moderate: stages 1 + 2.
- Fast: full cascade including stage 3 at full rate (`do_third = 2`).

## Estimated Rust port effort
- `search_both` (stage 2): ~150 LOC. Pancetta likely has equivalent
  scaffolding; mostly a parameter-tuning + drop-in.
- `search_both_known` (stage 3): ~150 LOC including the
  known-symbol variant of `one_strength_known`.
- `do_third` gating: ~30 LOC in the post-decode path.
- 2-3 sessions: (S1) port `search_both_known`, validate against
  synthesized known-(hz, off) signal that the refined output
  matches truth within tolerance; (S2) wire into `try_decode`
  equivalent, eval subtraction residual quality (spectral purity
  test); (S3) tier-gate and eval on hard-200.

## Implementation notes for the implementer thread

- The third stage is the highest-leverage piece. Implement it first
  even if stages 1 and 2 in pancetta are already configured
  differently — subtraction residual quality is the load-bearing
  win.
- `one_strength_known` with `known_strength_how = 7` is a
  phase-aware metric: it accumulates `|c[i] - c[i-1]|` over the
  symbol-to-symbol differences at the known tones. This is
  *intentional* — it punishes (hz, off) values that produce
  inconsistent symbol-to-symbol phases. Don't simplify to magnitude
  sum; the phase-difference structure is the load-bearing piece.
- `known_sparse = 1` (process every symbol) is the default. Higher
  values (e.g. 2 or 4) sub-sample symbols for speed. Stay at 1 in
  early ports; revisit only if profile shows hot.
- Stage 3 runs **once per successful decode**, not per candidate.
  Its compute cost is bounded by the decode rate, typically 5-30
  decodes per slot. The full-rate FFT is the dominant cost; if
  pancetta already caches a full-rate FFT for other purposes, reuse
  it.
- Pancetta's downstream consumers (subtraction, emit callback,
  QSO state machine) should receive the **stage-3 refined**
  (hz, off). If any consumer is currently using the second-stage
  estimate, switch to the refined values to get the precision
  benefit.
- Eval: spectral-purity test on subtraction residual *with* vs
  *without* stage 3 enabled. The without-stage-3 residual should
  show visible per-symbol energy at the original signal's
  frequency; the with-stage-3 residual should be -40 dB or better.
- For hard-200 eval, the metric to watch is the **per-pass decode
  count delta**: stage 3 should not change pass-0 decode count
  (it's post-decode) but should *increase* pass-1+ decode count
  (cleaner residual → more weak signals visible to subsequent
  passes). If pancetta's `multipass = 1` config is hit by the tier
  classifier, stage 3 has limited benefit on Slow tier.
