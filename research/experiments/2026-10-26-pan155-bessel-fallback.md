---
slug: pan155-bessel-fallback
mode: ft8
state: deferred
created: 2026-10-26T10:02:30Z
last_updated: 2026-10-26
branch: PAN-155
parent_hypothesis: Running the existing Bessel metric (hb-253) only as a one-time AP0 retry on FT8 candidates that already failed DualMax captures some of its recall without the false positives that global Bessel caused on hard_200
wild_card: false
scorecard: research/scorecards/pan-155/*.json
delta_vs_main: fixtures 7/8 → 7/8; 64-file noise smoke 0 → 0 false decodes; elapsed +99.8% unlimited (compare elapsed hard gate FAILED), +14.4% at Standard; hard-200 not evaluated (all 200 WAVs and truth caches absent)
disposition: DEFAULT STAYS OFF. `Ft8Config::bessel_fallback_enabled` ships as an opt-in, default-false switch with eval's `--bessel-fallback-enabled`. The smoke A/B failed the 20% elapsed gate and the hard-200 graduation gate has not run.
---

# PAN-155: Bessel retry for failed DualMax candidates

## Question

hb-253's exact Bessel metric gained +0.273 dB on synthetic AWGN, but turning it
on for every candidate added 14 false positives against 1 true positive on 50
hard_200 recordings (Batch 99). PAN-155 changes only where the metric runs: a
candidate gets one more AP0 attempt with Bessel after the complete DualMax AP0
attempt fails, and never after a DualMax success. This record measures what
that costs and recovers on the inputs available in the implementation
container, and records which graduation evidence is still missing.

## Mechanism

`decode_candidate_with_bessel_fallback` (`pancetta-ft8/src/decoder.rs`) runs
the existing AP0 path, returns at once on success, and otherwise runs AP0 once
more with `llr_metric = Bessel` and `soft_combiner = None` in a copy of the
decode context. It retries only when the flag is set, the primary metric is
DualMax, the protocol is FT8, and the decode budget still has time. On a retry
failure the existing AP path runs with its unchanged gates. The library default
is `false`.

Deterministic tests cover the control flow and isolation:

- `bessel_fallback_tests` (lib): first success skips Bessel and AP; a failure
  records exactly `[DualMax, Bessel]`; flag off, primary Bessel, FT4 and an
  expired budget never retry; the retry neither accumulates into nor evicts
  soft-combiner buckets. Two cases are feature-gated: FT2 never retrying
  needs `--features ft2`, and the serde round trip (a config without the
  field deserializes to `false`) needs `--features serde`. CI's FT8 and
  workspace lanes enable neither, so run
  `cargo test -p pancetta-ft8 --features transmit,serde,ft2 --lib bessel_fallback`
  (10 passed on this branch).
- `decoder_refinement_tests::bessel_fallback_fixture`: a frozen generated
  waveform ("CQ K1ABC FN42", seed 194445316, −20 dB in 2500 Hz, 0 Hz / 0 s,
  one decode pass) that DualMax misses and both global Bessel and the fallback
  decode through `decode_window`. A second clean station 250 Hz away keeps the
  same accepted fields with the fallback on.

## Commands

Host `ccd9543b881c/linux/x86_64` (Catalyst cloud container, 32 cores, one
worker thread), rustc 1.98.0 release build with `research-eval`. Both arms of
each pair ran back-to-back on the same build and inputs; the flag is the only
config difference (`compare` CONFIG DIFF shows only
`bessel_fallback_enabled false → true`, and both arms keep
`llr_metric = DualMax`). The scorecards record `head_sha c7406177` with
`dirty: true`; the only uncommitted change at eval time was rustfmt
reformatting of PAN-155 test code.

```bash
cargo build --release -p pancetta-research --bin gen-noise --bin eval --bin compare
R=/tmp/pan155-eval-r4
target/release/gen-noise --count 64 --seed 20260706 --birdie-fraction 0.3 \
  --output-dir $R/noise --manifest $R/noise.manifest.json --label pan155-noise-smoke

# Smoke A/B, unlimited budget
RAYON_NUM_THREADS=1 target/release/eval --tier fixtures,noise_1000 --mode ft8 --seed 42 \
  --noise-manifest $R/noise.manifest.json --output $R/baseline.json
RAYON_NUM_THREADS=1 target/release/eval --tier fixtures,noise_1000 --mode ft8 --seed 42 \
  --noise-manifest $R/noise.manifest.json --bessel-fallback-enabled --output $R/fallback.json
target/release/compare $R/baseline.json $R/fallback.json \
  --bootstrap-n 1000 --bootstrap-seed 0xb007 --max-elapsed-regression-pct 20

# Standard-effort timing probe (250 ms budget, deadline at the native call)
RAYON_NUM_THREADS=1 target/release/eval --tier fixtures --mode ft8 --seed 42 \
  --effort standard --budget-anchor call --output $R/budget-baseline.json
RAYON_NUM_THREADS=1 target/release/eval --tier fixtures --mode ft8 --seed 42 \
  --effort standard --budget-anchor call --bessel-fallback-enabled --output $R/budget-fallback.json
target/release/compare $R/budget-baseline.json $R/budget-fallback.json \
  --bootstrap-n 1000 --bootstrap-seed 0xb007 --max-elapsed-regression-pct 20

# Graduation corpus: preflight attempt only (one arm; it stops at preflight
# here). A real A/B runs both arms like the smoke pair above.
target/release/eval --tier curated-hard-200 --mode ft8 --seed 42 --output $R/hard200.json
```

The `noise_1000` tier name is the CLI selector; this smoke manifest holds 64
generated noise-only WAVs (19 with birdies), not 1,000. Its sha256 was
`edd877e7729e166d94f0ae789b3ed3856f316fd38dad232b1878b2b6f55948bb`; the
manifest embeds its generation time, so a regenerated manifest hashes
differently while the seeded WAVs stay the same. Generated WAVs were not
committed.

## Results

Truth source: the fixtures tier's committed expectations (13 WAVs, 8 scored,
5 skipped by the tier); on the noise tier any decode is a false positive.

| Measurement | DualMax only | With fallback | Change |
|---|---:|---:|---:|
| Fixtures passed (unlimited) | 7 of 8 | 7 of 8 | 0 |
| Fixture failure | `basicft8/170923_082015.wav` (any-decode) | same | — |
| Noise false positives, 64 WAVs | 0 | 0 | 0 |
| Noise WAVs with any decode | 0 | 0 | 0 |
| Elapsed, unlimited (s) | 282.3 | 563.9 | **+99.8%** |
| Fixtures passed (Standard) | 7 of 8 | 7 of 8 | 0 |
| Elapsed, Standard (s) | 10.3 | 11.8 | +14.4% |
| Native decode p50 / p95 / max, Standard (ms) | 789.5 / 1049.0 / 1089.7 | 923.2 / 1131.5 / 1198.3 | +17% p50 |
| Windows over the 250 ms deadline | 13 of 13 | 13 of 13 | 0 |
| Max overrun past the deadline (ms) | 839.7 | 948.3 | +108.6 |

`compare` exit codes: smoke **1** (elapsed hard gate: +99.8% against the 20%
budget; no recall or false-positive regression flagged); Standard **0**
(+14.4%). Bootstrap intervals were skipped because these tiers carry no
`per_wav_records`. Unverified real-audio novels were not measured: the
`research/baselines/ft8` directory is absent, so eval omits the novel
classification.

The fallback recovered no additional fixture truth and added no noise decode.
Unlimited runtime roughly doubled, which is what a retry on nearly every
candidate would cost; most sync candidates in a slot are noise and fail
DualMax, so they all pay for the second attempt. This is an inference from the
elapsed totals; per-candidate retry counts were not instrumented.

At Standard on one worker thread, every window in both arms already overran
its 250 ms deadline, so the floor candidates dominate. With the fallback on,
native decode time grew 134 ms at p50 and 109 ms at max, and the worst overrun
grew by the same 109 ms. This is a container timing probe, not MiniPC or
on-air qualification.

### Hard-200: not evaluated

`eval --tier curated-hard-200` exited 1 at its corpus preflight: all 200
manifest WAVs (operator paths under `/Users/thagale/.pancetta/corpus/`) and
all 200 baseline truth caches under `research/baselines/ft8/` are missing in
this container. Nothing was scored, and the preflight failure is not a passing
safety check. The recall and false-positive question that motivated the ticket
is therefore still open.

## Disposition

Default stays off. The mechanism is available for opt-in A/B through
`Ft8Config { bessel_fallback_enabled: true, .. }` or eval's
`--bessel-fallback-enabled`. On the inputs available here it failed the
elapsed gate and showed no recall change.

Graduation requires complete paired hard-200 and 1,000-file noise runs on the
same WAV hashes, truth caches, decoder settings, host and worker count, with
every expected tier and per-WAV record present in both arms. The bar: a
positive recovered-TP change whose 1,000-resample bootstrap 95% CI excludes
zero; no increase in noise false positives or noise files with decodes;
unverified-novel growth no more than twice the verified-TP gain; and the
existing 20% comparable-host elapsed gate. Bounded-effort coverage needs its own
review before any production preset turns this on. Given the doubled
unlimited runtime, a passing elapsed gate likely needs a cheaper eligibility
rule than "every failed candidate", for example a sync-score floor; any such
rule is a new experiment, not part of this record.

## Artifacts

- `research/scorecards/pan-155/smoke-baseline.json`, `smoke-fallback.json`,
  `smoke-compare.txt` (unlimited smoke A/B, `compare` exit 1)
- `research/scorecards/pan-155/standard-fixtures-baseline.json`,
  `standard-fixtures-fallback.json`, `standard-fixtures-compare.txt`
  (Standard-effort timing probe, `compare` exit 0)
