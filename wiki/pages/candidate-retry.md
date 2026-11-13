# What will bite you about retrying a failed decode candidate?

PAN-155's `Ft8Config::bessel_fallback_enabled` gives an FT8 candidate that
failed its DualMax AP0 attempt one more AP0 attempt with the Bessel metric
(`decode_candidate_with_bessel_fallback` in `pancetta-ft8/src/decoder.rs`).
Four constraints shape any per-candidate retry:

- **The soft combiner must be bypassed.** When the optional soft combiner is
  on, AP0 accumulates each reception's LLRs into it and evicts the bucket on
  a CRC success. A retry that kept the combiner would add the same audio a
  second time and, on success, evict the bucket earlier receptions built, so
  the retry runs with `soft_combiner: None`.
- **Check the budget before the retry, and keep the floor.** Floor candidates
  always get their standard attempt; the optional retry checks
  `ctx.budget.has_time()` first and is skipped once the deadline passes. The
  check runs only before the retry starts, so a retry already running can
  still overrun, and under a finite budget it can take time from later
  candidates.
- **Optional FFT refinement is also budget-gated, including inside the floor
  (PAN-313).** The legacy per-symbol FFT fallback (the 21-trial time/frequency
  sweep after a coarse decode fails, ~80k 1920-point FFTs worst case for the
  50 floor candidates) is admitted only while the budget covers a sweep
  estimate and re-checked per trial; refusal skips just the sweep and the
  candidate worker moves on. The floor's unconditional promise covers the
  coarse spectrogram attempt. Watch the sample length, too. At exactly
  `WINDOW_SAMPLES` (151,680), only a zero sample offset fits; every positive
  offset returns `InsufficientData`. Production's 162,000-sample windows
  admit offsets up to ~0.86 s, while full recordings admit more offsets. Fixture tests that truncate to one window exercise a different
  sweep than the coordinator or research harness.
- **A retry on failure is not cheap.** Most candidates in a slot are noise
  and fail, so "retry every failure" retries most of them; the smoke A/B
  doubled unlimited eval time. A cheaper eligibility rule is a new
  experiment.

Match the protocol explicitly (`Protocol::Ft8`): FT2 also has three-bit
symbols, so `bits_per_symbol == 3` would wrongly include it. The FT2 case in
`bessel_fallback_tests` compiles only with `--features ft2`, which default and
CI builds do not enable.

Sources: `research/experiments/2026-10-26-pan155-bessel-fallback.md`,
`research/experiments/2026-11-13-pan313-fft-fallback-budget.md`,
`research/hypothesis_bank.md` hb-253. Code and docs win any conflict.
