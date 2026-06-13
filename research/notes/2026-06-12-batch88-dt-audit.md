# Batch 88 — hb-249: reported time_offset one symbol period (0.16 s) late

Branch `iter/2026-06-12-batch-88`. Probe: `pancetta-research/examples/batch88_dt_audit.rs`
(three independent measurements), guard: `batch88_measure_200.rs`.
Trigger: Batch 86's `--dt-scan` side finding (decoder dt 0.03–0.18 s late
vs the sample-accurate LS-fit position).

## Question 1 — reproduce + characterize

**Part A, synthetic ground truth** (modulated `CQ W9XYZ EN50` embedded at
exactly known start sample, 64 trials = 4 frequencies x 2 base dts x 8
sub-step positions, SNR ~strong, default config):

- PRE-FIX delta (reported − true): n=64, **median +1920 samples
  (exactly one symbol period, 0.16 s)**, mean +1815, range [+1440, +2400].
  Histogram: +1440:15 +1680:16 +1920:16 +2160:16 +2400:1.
- Identical distribution at 887.5 / 1512.5 / 2287.5 / 1505.6 Hz and at
  base dt 0.5 s vs 2.34 s → **CONSTANT offset**: frequency-independent,
  dt-independent, with ±480-sample (half-subblock) sync quantization on
  top. Not subblock-quantized drift, not refinement error — a fixed
  alignment-convention bug.

**Part B, real corpus** (top-2 strongest decodes per slot over the 20
Batch-86 kill-switch slots; per-block LS fit of the decode's re-synth
across a (Δf, Δt) grid): 34/40 clean locks, delta (fitted − reported)
mean −1722, median −1700, sd 390, mode bucket −1680..−1920. Same story
on real signals (the sub-1920 mean is the half-plateau effect below plus
true-dt continuum quantization). corr(delta, freq) = +0.11,
corr(delta, dt) = −0.27 — no meaningful dependence.

**Part C, ft8_lib cross-check** (378 hash-normalized unique-text matches
across the same slots): pancetta − ft8_lib `time_sec` median **0.0**,
90% of pairs within float noise of 0, ~8% at −960 (one sync step early —
see residuals). **ft8_lib reports the SAME late convention**, which is
why every prior truth comparison was blind to this bug. The LS fit and
the synthetic embed are the sample-accurate arbiters.

## Question 2 — localization

`pancetta-ft8/src/decoder.rs`:

- `compute_spectrogram` (line ~2999, faithful port of ft8_lib
  monitor.c's sliding frame): at time step `t` the persistent
  3840-sample analysis frame holds the audio samples **ending** at
  `(t+1) * 960`. The symmetric Hann window is therefore centred at
  `(t−1) * 960`, and the symbol whose 1920-sample span best aligns with
  row `t` **starts at `(t−2) * 960`**.
- Every `time_step → samples` conversion used
  `coarse_offset = time_step * spec_step` (pre-fix lines 3937, 5392
  un-padded; 2443, 4352, 4565, 4598, 4682, 4821, 5108, 5267, 6178, 6449
  with `− time_padding` only, `time_padding` always 0 in production) —
  i.e. the code asserted "row t = symbol starting at t*960". That is 2
  steps (1920 samples = one symbol period = 0.16 s) late.

## Question 4 — root cause and fix

**Root cause**: the spectrogram row↔sample convention. Row `t`'s
analysis window is *centred* at `(t−1)*960` (frame ends at `(t+1)*960`),
so the represented symbol starts at `(t−2)*960`; the conversion omitted
the constant 2-step look-back. Inherited from ft8_lib, whose
`time_sec = (time_offset + time_sub/osr) * symbol_period` carries the
identical offset.

**Fix applied** (unambiguous constant-formula correction, shipped as
correctness, no config flag): new `SLIDING_FRAME_LOOKBACK_STEPS = 2`
(decoder.rs:83) + `candidate_offset_samples()` helper (decoder.rs:93);
all 12 conversion sites routed through it;
`reverse_derive_candidate` (decoder.rs:7065) made the exact inverse
(adds the 2 steps back). Candidates in the first two steps now report
small negative dt, which is physically correct.

This was not only a reporting bug. Two sample-domain consumers fed off
the biased value:

1. `subtract_signal` (decoder.rs:2758, multipass time-domain subtract)
   fine-searches only ±480 samples around `msg.time_offset` — it could
   **never** reach the true position 1920±480 samples away. This is why
   Batch 86 measured global-fit residual ratios of 0.9999 (subtract
   removing nothing) and plausibly why time-domain multipass historically
   added ~0 TPs.
2. The fine-timing time-domain extraction fallback in
   `decode_candidate` / `par_decode_candidate` (21 FFT trials, ±720
   samples around `coarse_offset`) was misaligned by a full symbol —
   structurally dead weight until now.

## Post-fix audit (same probe)

- Part A: median **+0**, mean −105, range [−720, +480] — pure sync
  quantization, centred.
- Part B: median **+220**, mean +198 (within a quarter subblock of 0).
- Part C: pancetta − ft8_lib median **−1920** — pancetta now reports
  ~0.16 s *earlier* than ft8_lib, which is correct. **Any future eval
  that matches pancetta decodes to ft8_lib truth by dt proximity must
  allow for this constant** (text matching, the Batch 87 rule, is
  unaffected).

## Question 4c — 200-slot before/after guard (raw_530_full[0..200], default config, hash-normalized ft8_lib truth)

| | decodes | TP | FP | truth found | miss rate |
|---|---:|---:|---:|---:|---:|
| before (main) | 4204 | 3563 | 641 | 3563/3660 | 2.65% |
| after (fix) | 4213 | 3563 | 650 | 3563/3660 | 2.65% |

TP-identical; +11/−2 changed texts (all classed "FP" = not in ft8_lib's
decode set for that slot). Content audit of the 11 new decodes: every
callsign is an active same-day station in the 5/30 truth corpus
(occurrence counts 2–503), and 6/11 exact texts appear verbatim in
OTHER slots' truth (e.g. `<...> K7CTV DM42` @2456.2 Hz — 61 other
slots; `CQ R6OJ KN87` — 4). These are genuine decodes ft8_lib missed,
surfaced by the now-correctly-aligned multipass subtract — not noise
FPs. Verdict: **TP-neutral, no regression**.

## Tests

- `cargo test --features transmit -p pancetta-ft8`: all 519 pass, **zero
  assertions needed updating** — no test ever pinned the (wrong) dt.
- `cargo test --workspace --features transmit`: exit code 0, 64/64
  suites ok.

## Residuals (documented, deliberately not bundled into this fix)

1. **Half-plateau (±960)**: `compute_costas_score_groups` takes
   `max` over `half ∈ {0,1}` (decoder.rs `for half in 0..2`), but with
   TIME_OSR=2 the t0 sweep already covers half-symbol offsets, so
   `score(t0) = max(g(t0), g(t0+1))` — a two-step plateau whose
   tie-break sometimes emits the candidate one step early (the ~8%
   −960 population in Part C, and Part A's mean −105 skew). Removing
   the redundant half-loop would change candidate scores/sets — a
   mechanism change requiring its own measured batch (bank candidate).
2. **Quantization** is inherently ±480 samples (sync resolution 960);
   hb-044 parabolic refinement (default off) is the existing knob.
3. ft8_lib truth `time_sec` retains its own +0.16 s convention; truth
   files were not touched.
