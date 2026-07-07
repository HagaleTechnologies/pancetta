# 2500 Hz SNR calibration + real sensitivity curve (Workstream 0, Task W0.2)

**Date**: 2026-07-07
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: HARNESS calibration + first real (pancetta vs jt9) sensitivity-curve measurement on
the recalibrated corpus. Not a decoder A/B — no decoder code changed in this task.

## What this is

Task W0.1 built the FP-on-noise tier. This task (W0.2) fixes the harness's other measurement-trust
gap identified in the design spec (§2): the synth `clean` tier's SNR axis was full-band (6 kHz
Nyquist), not the field-standard WSJT-X convention (SNR relative to a 2500 Hz reference noise
bandwidth) — a ~3.8 dB discrepancy that made every recorded "SNR@50%" number incomparable to
jt9/WSJT-X's own published sensitivity figures. The corpus was also coarse (2 dB steps, n=6
messages/step, fixed 1500 Hz / fixed dt=0) — too few trials and too easy to overfit a single
fixed grid position for a real sensitivity curve.

This task:
1. Fixes the noise-scaling formula to the WSJT-X 2500 Hz convention (with a genuine TDD
   calibration test).
2. Fixes `SNR@50%`/`SNR@90%` to real linear interpolation instead of "first bin >= threshold".
3. Regenerates the `clean` corpus: −24 → −14 dB in 1 dB steps, n=50 distinct messages/step,
   randomized base freq (400-2600 Hz) and dt (±0.3 s) per file — 550 WAVs total.
4. Runs the jt9 oracle over the new corpus to produce a reference curve.
5. Runs pancetta (production `Ft8Config::default()`) over the same corpus.
6. Reports the headline metric: pancetta SNR@50% − jt9 SNR@50%, same corpus, same convention.

## Part 1 — calibration bug found and fixed

### The noise-scaling formula

`add_awgn_2500hz_ref` (`pancetta-research/src/synth.rs`) now implements:

```
noise_rms = signal_rms / 10^(snr_db/20) * sqrt(full_band_hz / 2500.0)
```

with `full_band_hz = 6000` (12 kHz sample rate / 2, Nyquist). This is the same convention
independently re-derived in this crate's own `examples/batch62_soft_combiner_repeats.rs::
sigma_for_snr_db`, and matches Franke & Taylor's SNR-reporting convention (QEX 2020).

### TDD evidence (genuine RED -> GREEN)

`pancetta-research/tests/snr_calibration_tests.rs` generates a real synth WAV at label −15 dB,
writes it to 16-bit PCM and re-reads it (a full round trip, not an in-memory shortcut), then
**independently** measures the achieved SNR via a method that never calls
`add_awgn_2500hz_ref`'s own formula:
- **Signal power**: a per-symbol rectangular DFT (window = exactly one FT8 symbol span,
  `SAMPLES_PER_SYMBOL` = 1920 samples — the same rectangular, symbol-length window this repo's
  own design spec recommends for matched per-candidate demodulation, §3/D3) at each of the 79
  known tone bins, debiased by the known per-bin white-noise contribution (`N * noise_variance`,
  from DFT scaling), averaged across all 79 symbols.
- **Noise power**: measured directly (time domain) from the WAV's silent lead-in region, then
  scaled from the full [0, 6000] Hz band down to the 2500 Hz reference band by the same 2500/6000
  ratio the generator's convention is built on.

**RED** (formula's `sqrt(6000/2500)` factor temporarily removed, reverting to the old full-band
convention):
```
snr_calibration: label=-15.0 dB, independently-measured=-11.130 dB, delta=3.870 dB (tolerance ±0.3 dB)
thread '...' panicked: measured SNR -11.130 dB is more than 0.3 dB from the label -15.0 dB
```
The measured 3.870 dB discrepancy matches the design spec's predicted ~3.8 dB almost exactly —
strong independent confirmation the bug (and the fix) are real, not an artifact of the test.

**A second, more interesting bug found along the way**: the very first calibration attempt (with
the sqrt factor correctly in place) still failed by −1.7 dB. Root cause: `Ft8Modulator::
apply_final_processing` (called internally by `modulate_symbols`, designed for real transmission)
**unconditionally peak-normalizes its output to 0.95** regardless of the `tx_power` constructor
argument — so the pre-W0.2 generator's clean signal always sat at ~0.95 peak / ~0.67 RMS. At the
very negative SNRs this corpus needs (down to −24 dB), the required noise RMS is *many times*
larger than that (e.g. `noise_rms ≈ 16.5` at −24 dB against an unscaled 0.67-RMS signal), so
`[-1.0, 1.0]` clamping before 16-bit quantization destructively clipped the overwhelming majority
of samples. That clipping non-linearly compresses the waveform, which measurably suppresses the
coherent tone-bin signal power relative to the noise floor — exactly the −1.7 dB this task's own
calibration test caught. Confirmed by direct measurement: passing a 200x-smaller `tx_power` to
the modulator produced **bit-identical** output amplitude (proving `tx_power` alone has zero
effect once `apply_final_processing` runs). Fixed by explicitly scaling the modulator's returned
buffer down by `SYNTH_SIGNAL_SCALE = 0.005` (post-hoc, after the library's forced normalization),
which keeps even the worst-case (−24 dB) noise RMS at ~6σ ≈ 0.49 — comfortably inside `[-1.0,
1.0]`. Verified empirically: max |sample| across the 10 worst-case (−24 dB) files in the real
corpus is 0.43 (no clipping).

**GREEN** (both fixes in place):
```
snr_calibration: label=-15.0 dB, independently-measured=-14.919 dB, delta=0.081 dB (tolerance ±0.3 dB)
test synth_wav_snr_matches_2500hz_wsjtx_convention_within_tolerance ... ok
```

### SNR@50%/90% interpolation

`first_threshold_db` (`pancetta-research/src/bin/eval.rs`) now does real linear interpolation
between the two bins straddling the threshold, instead of returning the first bin whose
recall >= threshold (which quantized the reported number to the corpus's step size). Unit-tested
(`snr_interpolation_tests`, 5 tests) with a genuine RED->GREEN cycle: a synthetic 5-bin fixture
crossing 50% between −20 dB (0.20 recall) and −19 dB (0.60 recall) expects the interpolated
−19.25 dB; the pre-fix implementation returned −19.0 (confirmed FAILED before the fix, `ok` after).

## Part 2 — corpus regeneration

`research/corpus/synth/manifests/clean.config.json`:
- `snr_steps_db`: −24.0 to −14.0 in 1 dB steps (11 bins; was 2 dB steps, −28 to −10).
- `messages`: 50 distinct `"CQ K1xxx GGNN"` messages (was 6 fixed QSO-exchange-style messages);
  distinct callsign suffixes (`K1AAA`..`K1ABX`) paired with 20 rotating valid Maidenhead grids.
- `channel`: `awgn` (unchanged).
- Per-file **base audio frequency** now randomized uniformly in [400, 2600] Hz and **dt** (time
  offset within a 15 s slot buffer, with a 1.0 s lead-in) randomized uniformly in [−0.3, +0.3] s
  — both deterministic per-file (derived from the existing per-wav seed via a distinct derived
  seed so the freq/dt draw doesn't consume the same RNG stream as the AWGN fill). Was: fixed 1500
  Hz, fixed dt = 0 (signal always at sample 0, no lead-in silence at all) — a decoder could
  overfit that one grid position.
- Total: 50 x 11 = **550 WAVs** (was 60).
- `SynthEntry` gained `base_freq_hz`/`dt_s` fields (`#[serde(default)]` to the exact historical
  fixed values, so the still-committed `doppler.manifest.json`/`synth_pair_200.manifest.json`
  deserialize unchanged — not regenerated in this task, out of scope).

Generation moved from `src/bin/gen_synth.rs` into the library (`pancetta_research::synth`) so
`tests/snr_calibration_tests.rs` can exercise the noise-scaling formula directly — mirrors the
`gen_noise.rs` / `bin/gen_noise.rs` split from Task W0.1. `gen-synth` is now a thin CLI wrapper.

Verified: no clipping in the regenerated corpus (max |sample| = 0.434 across the 10 worst-case
−24 dB files checked); all 550 WAVs generated without an encode error.

## Part 3 — jt9 oracle run

```
cargo build --release -p pancetta-research --bin baseline
./target/release/baseline --tier synth --mode ft8 \
  --synth-manifest research/corpus/synth/manifests/clean.manifest.json
```

- 550 WAVs, jt9 invoked as `jt9 -8 -d 3` per WAV (existing `bin/baseline.rs` pattern, unchanged —
  reused as-is per the task brief, no new jt9-invocation plumbing built).
- jt9 binary: `/Applications/wsjtx.app/Contents/MacOS/jt9` (default fallback path, confirmed
  present on this machine).
- Elapsed: ~2.7-3.2 s/WAV, **~26 minutes wall-clock** for all 550 files.
- Cached to `research/baselines/ft8/<wav_sha256>.json` (550 new cache files, same shape as every
  other tier's jt9 cache).

Pancetta eval run over the identical corpus:

```
cargo build --release -p pancetta-research --bin eval
./target/release/eval --tier synth-clean --mode ft8 \
  --output research/scorecards/synth_clean_w02_new_corpus.json
```

No CLI overrides — this is `Ft8Config::default()`, the exact production config.

## Result — both curves

| SNR (dB) | pancetta decoded/50 | jt9 decoded/50 |
|---:|---:|---:|
| −24 | 0 | 1 |
| −23 | 0 | 3 |
| −22 | 0 | 14 |
| −21 | 1 | 30 |
| −20 | 3 | 48 |
| −19 | 31 | 50 |
| −18 | 46 | 48 |
| −17 | 50 | 49 |
| −16 | 50 | 49 |
| −15 | 50 | 50 |
| −14 | 50 | 49 |

- **pancetta `snr_at_50pct_recovery_db` = −19.214 dB** (interpolated between −20 dB [3/50 = 0.06]
  and −19 dB [31/50 = 0.62]).
- **jt9 `snr_at_50pct_recovery_db` = −21.313 dB** (interpolated between −22 dB [14/50 = 0.28] and
  −21 dB [30/50 = 0.60]).

### Headline number

**pancetta SNR@50% − jt9 SNR@50% = −19.214 − (−21.313) = +2.10 dB.**

Pancetta needs ~2.1 dB **more** SNR than jt9 to reach 50% recall on this corpus, under the
identical WSJT-X 2500 Hz convention. This is now the reference sensitivity-gap number for the
rest of the decoder-tp-sensitivity plan (Workstreams 1-5 aim to close it). It is directionally
consistent with the design spec's hard-200 finding (pancetta recovers ~59.3% of what `jt9 -d 3`
decodes there) — a real-signal recall gap and a synthetic-AWGN sensitivity gap pointing the same
way, from two independently-built measurements.

Both curves are stored in the scorecard `by_snr_db` / `jt9_snr_curve` fields:
`research/scorecards/synth_clean_w02_new_corpus.json`.

## Corpus-refresh composite offset (hb-133 convention)

Same production decoder, synth-clean tier only (isolating the tier's own composite contribution;
all other tiers absent contribute 0 identically on both sides, so the difference is pure corpus
shift):

- Pre-refresh (old 2 dB / n=6 / fixed-freq-dt corpus, old uncalibrated formula, old
  first-bin-threshold): composite = 0.150000 (`snr_at_50pct_recovery_db = -20.0`).
- Post-refresh (new 1 dB / n=50 / randomized-freq-dt corpus, calibrated formula, interpolated
  threshold): composite = 0.138214 (`snr_at_50pct_recovery_db = -19.214`).
- `offset_to_subtract = 0.138214 - 0.150000 = -0.011786`, recorded in
  `research/scorecards/refresh_offsets.json` (2026-07-07 entry) so cumulative graduation
  tracking stays comparable across this corpus swap.

## Files

- `pancetta-research/src/synth.rs` — generation core moved here + fixed noise scaling.
- `pancetta-research/src/bin/gen_synth.rs` — thin CLI wrapper, freq/dt randomization.
- `pancetta-research/src/bin/eval.rs` — linear interpolation, jt9-curve wiring (`run_synth_tier`,
  `jt9_recovered`, `sha256_file`), unit tests.
- `pancetta-research/src/scorecard.rs` — `jt9_snr_curve` / `jt9_snr_at_50pct_recovery_db` fields.
- `pancetta-research/tests/snr_calibration_tests.rs` — new calibration test.
- `research/corpus/synth/manifests/clean.config.json` / `clean.manifest.json` — regenerated.
- `research/scorecards/refresh_offsets.json` — new entry.
- `research/scorecards/synth_clean_w02_new_corpus.json` — new-corpus scorecard (both curves).
- Real corpus WAVs (not committed, gitignored): `research/corpus/synth/wavs/clean/` (550 files,
  ~189 MB).
- jt9 baseline cache (not committed, gitignored): `research/baselines/ft8/` (+550 new files).
