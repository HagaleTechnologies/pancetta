---
slug: pan162-impulse-llr-storm
mode: ft8
state: shipped
created: 2026-10-25T23:55:00Z
last_updated: 2026-10-27
branch: PAN-162
parent_hypothesis: hb-256
scorecard: research/scorecards/pan162-impulse-llr-storm.json (synthetic plants, verdict), research/scorecards/pan162-impulse-llr-offair.json (off-air do-no-harm) and research/scorecards/pan162-{synth,noise}-{control,variant}.json (production gate)
delta_vs_main: FT8 decodes run the k=6 knee (coordinator default). Storm plant (decision) k=6 threshold shift +0.465 dB [+0.308, +0.653]; awgn k=6 +0.000 dB; off-air ΔTP k=6 +1, k=3 -7; production gate synth-clean k=6 recovered Δ+0 (95% CI [+0.0, +0.0]), elapsed -4.5%; noise_1000 false_positives_total 5 → 5, noise_files_decoded 5 → 5, elapsed +14.0%
disposition: SHIPPED for FT8 at k=6. The pre-registered verdict is candidate-k6, which triggers Phase 6, and its native-only production gate passed in PAN-241 (synth-clean and noise_1000 legs, elapsed on both). `apply_effort_overrides` sets `impulse_robust_llr = Some(6.0)` for FT8 and `None` for FT4 and FT2; the library default `Ft8Config::impulse_robust_llr` stays `None`.
---

# PAN-162: hb-256's lightning-static LLR knee, measured on the stormiest days

## Question

hb-256's robust-noise LLR knee (`Ft8Config::impulse_robust_llr`) is free on
clean audio but measured just under the +0.3 dB shipping bar on batch101's
general-purpose synthetic plant (≈4% of symbols hit). Two reopen paths were
never built: a way to find the most impulsive recorded days, and a fairer
re-measurement on stormier audio. This experiment builds both and fixes the
decision rule before any storm result exists.

## What was run in this implement run (2026-10-25)

Host: Catalyst cloud container, 6-CPU cgroup quota (`/sys/fs/cgroup/cpu.max`
= `600000 100000`), `RAYON_NUM_THREADS=6`, release build at branch PAN-162
(on `origin/main` d000373).

```sh
cargo build --release -p pancetta-research --example batch104_impulse_llr_storm_remeasure \
  --example batch101_impulse_robust_llr_kill_switch --bin impulse-survey --bin eval
target/release/impulse-survey --dir pancetta-ft8/vendor/ft8_lib/test/wav --dir pancetta-ft8/tests/fixtures/wav \
  --output research/corpus/characterizations/pan162-impulse-survey-offair.json      # 0.2 s
RAYON_NUM_THREADS=6 target/release/examples/batch104_impulse_llr_storm_remeasure --offair \
  --output research/scorecards/pan162-impulse-llr-offair.json                         # 103 s
```

The implement run did not run the twelve synthetic shards or `--summarize`.
They ran in the remediate run below, with the pre-registration unchanged.

## What was run in the remediate run (2026-10-26)

Same host class: Catalyst cloud container, 6-CPU cgroup quota
(`600000 100000`), release build at branch PAN-162 commit 4f36178. The shard
directory was `/workspace/tmp/pan162-rem/shards`, a scratch path outside the
checkout; the shards ran one at a time from a shell loop, in this order:

```sh
cargo build --release -p pancetta-research --example batch104_impulse_llr_storm_remeasure \
  --bin eval --bin compare
export RAYON_NUM_THREADS=6
B=target/release/examples/batch104_impulse_llr_storm_remeasure
for plant in awgn storm base severe; do for config in off k3 k6; do
  $B --plant $plant --config $config --shard-dir /workspace/tmp/pan162-rem/shards
done; done
$B --summarize --shard-dir /workspace/tmp/pan162-rem/shards \
  --offair research/scorecards/pan162-impulse-llr-offair.json \
  --output research/scorecards/pan162-impulse-llr-storm.json
```

Shard wall times (decodes are seeded, so contention changes only time, never outcomes; the six awgn and storm shards shared the CPU quota with the aborted eval arms below, a build, or both):

| shard | exit | wall (s) |
|---|---|---|
| awgn-off | 0 | 323 |
| awgn-k3 | 0 | 215 |
| awgn-k6 | 0 | 309 |
| storm-off | 0 | 560 |
| storm-k3 | 0 | 464 |
| storm-k6 | 0 | 492 |
| base-off | 0 | 157 |
| base-k3 | 0 | 148 |
| base-k6 | 0 | 176 |
| severe-off | 0 | 139 |
| severe-k3 | 0 | 165 |
| severe-k6 | 0 | 182 |

`--summarize` reported no grid extension, ran `check_invariants()` before saving, and printed:

```text
PAN-162 verdict: candidate-k6 — storm k=6 +0.465 dB [+0.308, +0.653], k=3 +0.523 dB [+0.358, +0.732]; awgn k=6 +0.000 dB, k=3 +0.000 dB; off-air ΔTP k=6 +1, k=3 -7
```

The scorecard records `commit` 4f36178 and `rayon_threads` 6. Its sha256,
`90f1527ca2605744bd70d154d8a4abd7c80ae57f21e9e8cb9a14baad86b76f2c`, equals that
of the validate phase's independent scratch run of the same commands at the
same commit, so the record reproduces byte for byte.

## Instrument (Scenario 1)

`pancetta-research/src/impulsiveness.rs`, pre-registered in the plan:

- P[b] = mean x² over each full 1920-sample block (one FT8 symbol at 12 kHz);
- ref[b] = median of P over a 25-block window shifted to stay inside the slot;
- block b is impulsive iff ref[b] > 0 and P[b] > 3·ref[b];
- a slot's impulsive-block fraction (IBF) is impulsive blocks / blocks, and a
  day's score is the mean IBF over its non-silent slots (ties: share of slots
  with any impulsive block, then date).

Pilot table from planning (synthetic means over 16 seeds; real = 72 off-air
12 kHz WAVs):

| metric (block = 1920 samples, knee 3×) | AWGN | AWGN, 8 signals at 0 dB | plant p=0.02 | p=0.05 | p=0.10 | real off-air: files > 0 (max) |
|---|---|---|---|---|---|---|
| time-domain, slot-global median | 0 | 0 | 0.0435 | 0.0870 | 0.1867 | 2/72 (0.086) |
| per-bin spectral, slot-global median | 0 | 0 | 0.0435 | 0.0870 | 0.1867 | **43/72** (0.140) |
| per-bin spectral, ±6-block local median | 0 | 0 | 0.0435 | 0.0854 | 0.1725 | 33/72 (0.097) |
| time-domain, ±6-block window truncated at slot edges | 0 | 0 | 0.0435 | 0.0847 | 0.1653 | 7/72 (0.032) |
| **time-domain, 25-block window kept inside the slot (chosen)** | **0** | **0** | **0.0435** | **0.0870** | **0.1859** | **1/72 (0.0215)** |

Why the others fail on real audio. Per-block dumps showed slow, band-wide level drift across a slot (`191111_110145.wav`: block power 2.1× the slot median at 4.5 s, 0.86× at 9.4 s), which fools any slot-global median. In dense bands (`20m_busy/test_34`, `test_38`) hopping 8-FSK tones fill more than half the bins, so the per-bin approach flags signal-dominated blocks; their in-block crest factor is about 4 dB, nowhere near impulsive. A truncated edge window flags the next slot's transmissions starting at t ≥ 14.4 s (6 of the 7 flagged files). The one real file the chosen metric flags is `basicft8/170923_082045.wav`, blocks 16-17 (2.56-2.88 s): power 3.7-3.8× the local median with a 12.4-12.8 dB crest factor, against about 10.6 dB for Gaussian noise and about 5 dB for signal-dominated blocks. That fits a genuine short burst. With no labelled lightning audio in the container, this cannot be confirmed.

The chosen definition is the only one that tracked the synthetic burst duty
and stayed quiet on real off-air audio.

### Survey of the in-repo off-air audio

`research/corpus/characterizations/pan162-impulse-survey-offair.json`: 82 WAVs
(62 vendored ft8_lib 12 kHz + 13 pancetta fixtures + 7 skipped at 6400 Hz),
58 undated (`test_*`, `websdr_*`), 0 unreadable, 0 silent. Six real capture
days rank, and only one slot in the whole set has impulsive blocks:
`basicft8/170923_082045.wav`, 2 blocks (IBF 0.0215). It is the same file the
pilot flagged, with a 12.4-12.8 dB in-block crest factor that fits a genuine
short burst. Day 20170923 therefore ranks first (mean IBF 0.0054). None of
this audio is a storm capture; it demonstrates the tooling on real recordings.
The station's own archive is the real Scenario 1 input (follow-up below).

## Off-air do-no-harm (c′)

`research/scorecards/pan162-impulse-llr-offair.json`: the 60 vendored 12 kHz
WAVs with ft8_lib reference lists, 1289 distinct reference messages.
TP counts distinct `hash_normalize_message` texts in the WAV's reference set.

| config | ΣTP | ΣFP |
|---|---|---|
| off | 941 | 56 |
| k3 | 934 | 60 |
| k6 | 942 | 60 |

off-air ΔTP k=6 +1, k=3 -7

Paired per-WAV bootstrap (n=1000, seed 0xb007; reported, not gated): k=6
[-2.0, +5.0], k=3 [-15.0, +2.0].

Against bar (c′) ΔTP ≥ −2: **k=6 passes, k=3 fails**. Under the decision
rule, k=3 can therefore at best reach `stays-off-harm-gate`; only k=6 can
still become a candidate, and only if the storm plant clears bar (b). It
does (see Verdict).

Reference-list parsing note: 85 of the 1298 vendored reference lines carry a
WSJT-X country annotation after the message (`CQ TA6CQ KN70      AS Turkey`).
`parse_reference_txt` cuts the text after `~` at the first run of two spaces,
so those references can match a decode. The plan's wording was "text after
`~`, trimmed"; without the cut those 85 references could never be TPs for any
config.

## Pre-registration (copied unchanged from the PAN-162 plan)

Nothing in this section may change after a storm or off-air result is seen. Code constants in `pancetta-research/src/storm_remeasure.rs` mirror it, and the record test checks the committed scorecard against them.

**Stimulus (identical to batch101 Part A):** the 20 `MESSAGES` rotate by `trial % 20`; the signal is modulated at offset 0.0 and resized to `WINDOW_SAMPLES` (151680); σ comes from `sigma_for_snr_db` (2500 Hz reference); the seed is `0xB101_0000 + si*1000 + trial` with `si = round((snr_db + 24.0) * 2)`. Base AWGN is drawn first, then bursts, so the base noise is paired across all plants and configs at each (SNR, trial). Configs are `off` (None), `k3` (Some(3.0)) and `k6` (Some(6.0)), decoded with `Ft8Decoder::decode_window` and `Ft8Config::default()` otherwise. Success means some decode's text equals the transmitted message.

**Plants:** all share batch101's duration (uniform 1..=3 symbols) and amplitude (Uniform(10, 30) dB over the base σ). Only the burst-start probability per symbol varies.

| plant | burst-start p | approx. symbols hit | role | trials/point | SNR grid (0.5 dB steps) |
|---|---|---|---|---|---|
| `awgn` | none | 0% | **decision**: bar (a) | 200 | −21.0 … −16.0 (11 points) |
| `base` | 0.02 (`ImpulseBurstPlant::BATCH101`) | ~4% | descriptive: re-measures the original with a CI | 100 | −21.0 … −12.0 (19 points) |
| `storm` | 0.05 | ~10% | **decision**: bar (b) | 200 | −21.0 … −12.0 (19 points) |
| `severe` | 0.10 | ~19% | descriptive: dose-response | 100 | −21.0 … −12.0 (19 points) |

Why `storm` is the decision plant: it is the smallest materially stormier step (2.5× the original's burst rate). It directly tests the original note's hypothesis (3), "the burst-duty here (4% of symbols) caps the recoverable margin". It also stays far from contaminating the per-candidate median, the regime the knee was designed for. Choosing the milder of the two stormier plants is the conservative choice: a stormier plant makes passing easier, so `severe` gets no decision weight. Each plant's mean impulsive-block fraction is recorded, so later real-day surveys can be placed on the same scale.

**Grid extension rule (mechanical; it can only add points):** if any config's rate curve has not crossed 50% at the top of a plant's grid, extend that plant's grid upward in 0.5 dB steps, up to −6.0 dB, for all three configs. If any config is already ≥ 50% at the bottom point, extend downward to −24.0 dB. Seeds depend only on (SNR, trial), so existing points never change.

**Statistic:** `threshold_50` exactly as batch101: scan upward and linearly interpolate the first crossing of 50%. Shift = threshold(off) − threshold(k), so positive means improvement. The **paired bootstrap** uses n=1000 and seed `0xb007`. At each SNR point it resamples trial indices independently with replacement and applies the same indices to `off` and `k`. The CI is the type-7 2.5/97.5 percentile range over resamples whose thresholds are defined; undefined resamples are counted.

**Off-air do-no-harm (c′):** the 60 vendored 12 kHz WAVs that have reference lists, decoded with `off`, `k3` and `k6`. TP means a decode whose `hash_normalize_message` text is in the WAV's reference set. ΔTP(k) = ΣTP(k) − ΣTP(off). The paired `bootstrap_recall_delta` CI is reported but not gated. It replaces the original (c) corpus (hard_200/50, absent from every container) at the original threshold. This is a declared deviation: a different real corpus, the same bar.

**Decision rule.** Evaluate k=6 first, then k=3. The doc's own description is "k=6 conservative", and the bank entry recommends `Some(6.0)` for operators. The first k that passes all three bars is the candidate:
- (a) `awgn` point shift ≥ −0.05 dB;
- (b) `storm` point shift ≥ +0.30 dB **and** bootstrap `ci_low > 0`, with at most 50 of 1000 resamples undefined;
- (c′) off-air ΔTP(k) ≥ −2.

(a) and (b) are hb-256's own bars, with (b) applied to the stormier plant. The ticket names this plant as the alternative to real storm days ("or a stormier synthetic corpus"). The `ci_low > 0` clause is the RUNBOOK's standing significance convention ("`NOT significant` … default to SHELVE", `docs/RUNBOOK.md` §Running an experiment, step 6). It can only make a pass harder.

Verdict strings (serde) are listed in precedence order:

| verdict | meaning |
|---|---|
| `"candidate-k6"` / `"candidate-k3"` | that knee passed (a), (b) and (c′) |
| `"stays-off-harm-gate"` | some k passed (b) but failed (a) or (c′) |
| `"stays-off-inconclusive"` | no k passed (b) and some k had an undefined point threshold after extension, or more than 50 undefined resamples |
| `"stays-off-below-bar"` | all other cases |

**Production gate (runs only on a candidate).** eval runs native-only and serially, without and with `--impulse-robust-llr <k>`:
- synth-clean: recovered delta ≥ −2, and `compare`'s paired recall CI is not significantly negative;
- noise_1000: `false_positives_total` and `noise_files_decoded` do not increase;
- elapsed: within `compare`'s elapsed gate (`max_elapsed_regression_pct` default 20.0, `compare.rs:41`) on both corpora. On visible host contention, rerun the pair once, as PAN-158 did.

Native-only is deliberate. A merged ft8_lib + native result would hide native losses, and this gate exists to catch harm.

If all of these pass, the final disposition is **default-on** for FT8 at that k. Otherwise it is `stays-off-harm-gate`.

**Stop rule (copied from PAN-158 on purpose):** do not change seeds, plants, grids, trials, bars or k order after seeing results. Do not search for favourable seeds, add plants, or call a below-bar result complete with a different meaning. A failing gate is a resolved outcome ("stays off by default, same as today"), not a question for a human.

## Results: synthetic plants

`research/scorecards/pan162-impulse-llr-storm.json` (per-trial bitstrings,
rates, thresholds, paired bootstrap CIs and the verdict; `check_invariants()`
recomputes every derived number and the verdict from the bitstrings). The
grid-extension rule did not apply: every curve crosses 50% inside its
pre-registered grid, and none is at ≥ 50% at the bottom point.

### `awgn` — 200 trials/point, mean impulsive-block fraction 0.0000

| SNR (dB) | off | k3 | k6 |
|---|---|---|---|
| -21.0 | 0.5% | 0.5% | 0.5% |
| -20.5 | 0.5% | 0.5% | 0.5% |
| -20.0 | 1.5% | 1.5% | 1.5% |
| -19.5 | 14.0% | 14.0% | 14.0% |
| -19.0 | 30.0% | 30.0% | 30.0% |
| -18.5 | 61.0% | 61.0% | 61.0% |
| -18.0 | 81.5% | 81.5% | 81.5% |
| -17.5 | 95.5% | 95.5% | 95.5% |
| -17.0 | 99.0% | 99.0% | 99.0% |
| -16.5 | 100.0% | 100.0% | 100.0% |
| -16.0 | 100.0% | 100.0% | 100.0% |
| threshold | -18.68 | -18.68 | -18.68 |

- shift off → k=6: +0.000 dB, 95% CI [+0.000, +0.000], significant=false, undefined resamples 0/1000
- shift off → k=3: +0.000 dB, 95% CI [+0.000, +0.000], significant=false, undefined resamples 0/1000

### `storm` — 200 trials/point, mean impulsive-block fraction 0.0949

| SNR (dB) | off | k3 | k6 |
|---|---|---|---|
| -21.0 | 0.0% | 0.0% | 0.0% |
| -20.5 | 0.0% | 0.0% | 0.0% |
| -20.0 | 0.0% | 0.0% | 0.0% |
| -19.5 | 3.5% | 4.0% | 4.0% |
| -19.0 | 5.0% | 9.0% | 7.5% |
| -18.5 | 14.5% | 23.0% | 21.0% |
| -18.0 | 25.5% | 41.0% | 37.5% |
| -17.5 | 41.5% | 61.5% | 60.0% |
| -17.0 | 59.0% | 84.0% | 79.5% |
| -16.5 | 71.0% | 89.0% | 88.5% |
| -16.0 | 80.5% | 97.5% | 95.0% |
| -15.5 | 87.5% | 98.5% | 98.0% |
| -15.0 | 92.0% | 100.0% | 100.0% |
| -14.5 | 95.0% | 100.0% | 100.0% |
| -14.0 | 95.0% | 100.0% | 100.0% |
| -13.5 | 97.5% | 100.0% | 100.0% |
| -13.0 | 99.5% | 100.0% | 100.0% |
| -12.5 | 99.5% | 100.0% | 100.0% |
| -12.0 | 99.5% | 100.0% | 100.0% |
| threshold | -17.26 | -17.78 | -17.72 |

- shift off → k=6: +0.465 dB, 95% CI [+0.308, +0.653], significant=true, undefined resamples 0/1000
- shift off → k=3: +0.523 dB, 95% CI [+0.358, +0.732], significant=true, undefined resamples 0/1000

### `base` — 100 trials/point, mean impulsive-block fraction 0.0396

| SNR (dB) | off | k3 | k6 |
|---|---|---|---|
| -21.0 | 0.0% | 0.0% | 0.0% |
| -20.5 | 0.0% | 0.0% | 0.0% |
| -20.0 | 0.0% | 0.0% | 0.0% |
| -19.5 | 6.0% | 7.0% | 7.0% |
| -19.0 | 17.0% | 21.0% | 20.0% |
| -18.5 | 41.0% | 47.0% | 46.0% |
| -18.0 | 56.0% | 61.0% | 61.0% |
| -17.5 | 72.0% | 80.0% | 82.0% |
| -17.0 | 86.0% | 96.0% | 94.0% |
| -16.5 | 97.0% | 99.0% | 100.0% |
| -16.0 | 99.0% | 100.0% | 100.0% |
| -15.5 | 100.0% | 100.0% | 100.0% |
| -15.0 | 99.0% | 100.0% | 100.0% |
| -14.5 | 99.0% | 100.0% | 100.0% |
| -14.0 | 100.0% | 100.0% | 100.0% |
| -13.5 | 100.0% | 100.0% | 100.0% |
| -13.0 | 100.0% | 100.0% | 100.0% |
| -12.5 | 100.0% | 100.0% | 100.0% |
| -12.0 | 100.0% | 100.0% | 100.0% |
| threshold | -18.20 | -18.39 | -18.37 |

- shift off → k=6: +0.167 dB, 95% CI [+0.038, +0.519], significant=true, undefined resamples 0/1000
- shift off → k=3: +0.193 dB, 95% CI [+0.049, +0.535], significant=true, undefined resamples 0/1000

### `severe` — 100 trials/point, mean impulsive-block fraction 0.1857

| SNR (dB) | off | k3 | k6 |
|---|---|---|---|
| -21.0 | 0.0% | 0.0% | 0.0% |
| -20.5 | 0.0% | 0.0% | 0.0% |
| -20.0 | 0.0% | 0.0% | 0.0% |
| -19.5 | 1.0% | 1.0% | 1.0% |
| -19.0 | 1.0% | 1.0% | 1.0% |
| -18.5 | 3.0% | 7.0% | 7.0% |
| -18.0 | 4.0% | 13.0% | 10.0% |
| -17.5 | 7.0% | 25.0% | 21.0% |
| -17.0 | 16.0% | 48.0% | 43.0% |
| -16.5 | 28.0% | 69.0% | 60.0% |
| -16.0 | 40.0% | 79.0% | 75.0% |
| -15.5 | 49.0% | 82.0% | 77.0% |
| -15.0 | 66.0% | 90.0% | 87.0% |
| -14.5 | 54.0% | 92.0% | 87.0% |
| -14.0 | 69.0% | 96.0% | 91.0% |
| -13.5 | 78.0% | 96.0% | 93.0% |
| -13.0 | 84.0% | 97.0% | 95.0% |
| -12.5 | 86.0% | 95.0% | 95.0% |
| -12.0 | 84.0% | 98.0% | 95.0% |
| threshold | -15.47 | -16.95 | -16.79 |

- shift off → k=6: +1.324 dB, 95% CI [+0.771, +1.666], significant=true, undefined resamples 0/1000
- shift off → k=3: +1.482 dB, 95% CI [+0.981, +1.766], significant=true, undefined resamples 0/1000

| plant (mean IBF) | threshold off / k3 / k6 (dB) | shift k=6 [95% CI] | shift k=3 [95% CI] |
|---|---|---|---|
| `awgn` (0.0000) | -18.68 / -18.68 / -18.68 | +0.000 [+0.000, +0.000] | +0.000 [+0.000, +0.000] |
| `base` (0.0396) | -18.20 / -18.39 / -18.37 | +0.167 [+0.038, +0.519] | +0.193 [+0.049, +0.535] |
| **`storm` (0.0949), decision plant** | -17.26 / -17.78 / -17.72 | **+0.465 [+0.308, +0.653]** | +0.523 [+0.358, +0.732] |
| `severe` (0.1857) | -15.47 / -16.95 / -16.79 | +1.324 [+0.771, +1.666] | +1.482 [+0.981, +1.766] |

Shift = threshold(off) − threshold(k), so positive is a gain. Undefined
resamples: 0 of 1000 for every plant and knee.

**Port-fidelity cross-check (recorded, not a gate).** First 50 trials at −21.0…−16.0 dB, compared with the batch101 rows quoted in the plan: `awgn`/`off` `0,0,1,5,16,33,40,49,49,50,50` (identical); `base`/`off` `0,0,0,3,8,19,26,35,42,49,49` (identical). `base`/`k3` `0,0,0,4,8,23,28,41,49,50,50` and `base`/`k6` `0,0,0,4,8,22,28,42,48,50,50` (batch101: `0,0,0,4,8,23,28,41,49,50,50` and `0,0,0,4,8,22,28,42,48,50,50`).

## Verdict

PAN-162 verdict: candidate-k6 — storm k=6 +0.465 dB [+0.308, +0.653], k=3 +0.523 dB [+0.358, +0.732]; awgn k=6 +0.000 dB, k=3 +0.000 dB; off-air ΔTP k=6 +1, k=3 -7

Applying the bars to k=6 first: (a) awgn shift +0.000 dB ≥ −0.05 passes;
(b) storm shift +0.465 dB [+0.308, +0.653] ≥ +0.30 with `ci_low` > 0 and no
undefined resamples passes; (c′) off-air ΔTP +1 ≥ −2 passes. k=6 is the
candidate. (k=3 also clears (a) and (b), but fails (c′) at ΔTP −7, and it is
evaluated second.) The storm plant clears bar (b) that the original batch101
plant missed: on `base` (the batch101 plant re-measured with a CI) k=6 gains
+0.167 dB [+0.038, +0.519], below the bar as in batch101, and the gain grows with
burst rate (`severe`: +1.324 dB [+0.771, +1.666]).

## Phase 6: production gate

`candidate-k6` triggers the plan's Phase 6. Its first step is the
pre-registered native-only production gate above (synth-clean, noise_1000 and
the elapsed gate, control vs `--impulse-robust-llr 6`); only if all of it
passes does the coordinator turn the knee on for FT8. PAN-241 ran both legs,
and every bar passed, so the final disposition is **default-on for FT8 at
k=6** (see "Disposition" below).

### Synth-clean leg (PAN-241, 2026-10-27): passed

Host: Catalyst cloud container `8ea99d0ef503/linux/x86_64` on a shared
machine, 6-CPU cgroup quota (`/sys/fs/cgroup/cpu.max` = `600000 100000`),
`RAYON_NUM_THREADS=6` for both arms (`harness.cores_used` 6 in both
scorecards), release build at branch PAN-241 (`d963768`, clean tree). The two
arms ran serially, with nothing else running in the container. `gen-synth`
regenerates the WAVs under `research/corpus/synth/wavs/clean` that
`eval --tier synth-clean` reads through the tracked
`research/corpus/synth/manifests/clean.manifest.json`; the manifest it writes
to `$PAN162_RUN` is a by-product.

```sh
cargo build --release -p pancetta-research --bin eval --bin compare --bin gen-synth --bin gen-noise
PAN162_RUN=$(mktemp -d)
target/release/gen-synth --config research/corpus/synth/manifests/clean.config.json --output "$PAN162_RUN/clean.manifest.json"
RAYON_NUM_THREADS=6 target/release/eval --tier synth-clean --mode ft8 --output research/scorecards/pan162-synth-control.json
RAYON_NUM_THREADS=6 target/release/eval --tier synth-clean --mode ft8 --impulse-robust-llr 6 --output research/scorecards/pan162-synth-variant.json
target/release/compare research/scorecards/pan162-synth-control.json research/scorecards/pan162-synth-variant.json --bootstrap-n 1000 --bootstrap-seed 0xb007
```

| arm | `impulse_robust_llr` | wall (UTC) | `elapsed_seconds` | recovered | novel |
|---|---|---|---|---|---|
| control | `null` | 16:39:07-16:50:10 | 663.1 | 285 / 550 | 7 |
| variant | `6.0` | 16:50:10-17:00:43 | 633.4 | 285 / 550 | 7 |

`compare` exited 0 and printed:

```
A: research/scorecards/pan162-synth-control.json (sha d963768b, score 0.1390)
B: research/scorecards/pan162-synth-variant.json (sha d963768b, score 0.1390 +0.0000)

REGRESSIONS:
  (none)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  synth-clean               rec Δ=+0  (95% CI [+0.0, +0.0], n_bootstrap=1000) — NOT significant
  synth-clean               novel Δ=+0  (95% CI [+0.0, +0.0], n_bootstrap=1000) — NOT significant

CONFIG DIFF:
  decoder.decoder.impulse_robust_llr               null → 6.0
ELAPSED GATE:
  elapsed_seconds        663.1 → 633.4  (-4.5%, budget +20.0%)
```

| bar | measured | result |
|---|---|---|
| recovered Δ ≥ −2 | +0 | pass |
| paired recall CI not significantly negative | [+0.0, +0.0], not significant | pass |
| elapsed within +20% (synth-clean) | −4.5% | pass |

The variant scorecard records `git.dirty: true` because the control arm's
scorecard was by then an untracked file; no tracked file changed until the
variant arm had finished, both arms record the same `head_sha`, and both ran
the same `target/release/eval`, built at 16:38 before the first arm. The
machine is shared: during the control arm its load average was about 23 and
the container's cgroup was being throttled. The pre-registered contention
clause asks for a rerun only when an elapsed failure coincides with
contention, and this elapsed bar passed. The planning run for PAN-241 (same
commands at `RAYON_NUM_THREADS=2`, not committed) gave the same per-WAV
outcomes and elapsed 1229.6 → 1168.8 s (−4.9%).

### Noise_1000 leg (PAN-241, 2026-10-27): passed

Host: Catalyst cloud container `f429152c3267/linux/x86_64` on a shared machine,
6-CPU cgroup quota (`/sys/fs/cgroup/cpu.max` = `600000 100000`),
`RAYON_NUM_THREADS=6` for both arms (`harness.cores_used` 6 in both
scorecards), release build at branch PAN-241 (`a1e6622`), built at 21:02
before the first arm. `a1e6622` differs from the synth leg's `d963768` only
in doc text and the two synth scorecards. The two arms ran serially, with no
build or test running in the container. The remediate round's coordinator and
doc edits were written while the arms ran, so both scorecards record
`git.dirty: true`; `eval` does not depend on the `pancetta` crate, and the
binary both arms ran was built before any edit.

```sh
cargo build --release -p pancetta-research --bin eval --bin compare --bin gen-noise
PAN162_RUN=$(mktemp -d)
export RAYON_NUM_THREADS=6
target/release/gen-noise --count 1000 --seed 20260706 --birdie-fraction 0.3 \
  --output-dir "$PAN162_RUN/noise-wavs" --manifest "$PAN162_RUN/noise.manifest.json" --label noise_1000
target/release/eval --tier noise_1000 --mode ft8 --noise-manifest "$PAN162_RUN/noise.manifest.json" --output research/scorecards/pan162-noise-control.json
target/release/eval --tier noise_1000 --mode ft8 --noise-manifest "$PAN162_RUN/noise.manifest.json" --impulse-robust-llr 6 --output research/scorecards/pan162-noise-variant.json
target/release/compare research/scorecards/pan162-noise-control.json research/scorecards/pan162-noise-variant.json
```

| arm | `impulse_robust_llr` | wall (UTC) | `elapsed_seconds` | `false_positives_total` | `noise_files_decoded` |
|---|---|---|---|---|---|
| control | `null` | 21:02:27-21:19:49 | 1042.2 | 5 | 5 |
| variant | `6.0` | 21:19:49-21:39:37 | 1188.1 | 5 | 5 |

`compare` exited 0 and printed:

```
A: research/scorecards/pan162-noise-control.json (sha a1e6622b, score 0.0000)
B: research/scorecards/pan162-noise-variant.json (sha a1e6622b, score 0.0000 +0.0000)

REGRESSIONS:
  (none)

BOOTSTRAP CI:
  (skipped: neither scorecard carries `per_wav_records`. Re-eval with the Phase-B build to enable.)

CONFIG DIFF:
  decoder.decoder.impulse_robust_llr               null → 6.0
ELAPSED GATE:
  elapsed_seconds        1042.2 → 1188.1  (+14.0%, budget +20.0%)
```

| bar | measured | result |
|---|---|---|
| `false_positives_total` does not increase | 5 → 5 | pass |
| `noise_files_decoded` does not increase | 5 → 5 | pass |
| elapsed within +20% (noise_1000) | +14.0% | pass |

The machine is shared: the host's load average was about 24 when the control
arm started, 18.5 when it ended and 36.5 when the variant arm ended, so part of
the +14.0% is likely contention. The pre-registered contention clause asks for
a rerun only when an elapsed failure coincides with contention, and this
elapsed bar passed, so the pair was not rerun.
The validate run for PAN-241 ran the same pair in another container
(not committed) and got false positives 5 → 5, noise files decoded 5 → 5 and
elapsed 1189.0 → 1230.6 s (+3.5%).

### Disposition: default-on for FT8 at k=6

Every bar of the gate passed: synth-clean recovered Δ+0 with a paired recall
CI of [+0.0, +0.0], noise_1000 false positives and noise files decoded did
not rise, and elapsed stayed within +20% on both corpora (−4.5% and
+14.0%). PAN-241 applied the plan's steps 6.2-6.3:
`pancetta/src/coordinator/effort.rs` sets `impulse_robust_llr` to
`FT8_IMPULSE_ROBUST_LLR_KNEE` (6.0) for FT8 on every `apply_effort_overrides`
call and to `None` for FT4 and FT2, and the knee is a `DecoderRebuildKey`
field, so a protocol switch rebuilds the live decoder. The library default
`Ft8Config::default().impulse_robust_llr` stays `None`. The operator-side
revert rule below ("If the default shipped") now applies.

### PAN-162's attempts

PAN-162's implement run did not run the gate: its eval arms cost far more
than the storm shards. In that container, a native-only, unlimited-budget
`eval --tier noise_1000` arm with `RAYON_NUM_THREADS=1` reached its 40th
noise WAV (read from the process's open WAV file) after 371 s while ten
runnable threads (the shards, a build, both arms) shared the 6-CPU quota,
about 0.6 of a core each. That is roughly 5.5 CPU-s per WAV, or about 90
single-core minutes per 1000-WAV arm (an estimate from one partial run). Two
attempts were stopped and discarded, so neither produced a production-gate
number: all four arms at `RAYON_NUM_THREADS=3`, beside the shards (stopped
after 193 s), then the two noise arms at `RAYON_NUM_THREADS=1` (stopped after
371 s).


## Caveats

- Synthetic Gaussian-burst plant, not alpha-stable or recorded lightning.
- Native decoder only; single host.
- The off-air set is 60 WAVs of mostly busy-band audio with no known storm
  day; it is a do-no-harm check, not a gain check.
- Nothing here is an on-air, MiniPC or real-storm-day result.

## Operator-side follow-up

These commands run on the station machine, where `~/.pancetta/recordings` and `research/baselines/ft8/` exist.

```sh
# 1. Rank the station's days and select the stormiest (Scenario 1 on real recordings)
cargo run --release -p pancetta-research --bin impulse-survey -- --dir ~/.pancetta/recordings \
  --output research/corpus/characterizations/impulse-survey-station.json \
  --emit-manifest research/corpus/curated/ft8/storm_top3.manifest.json --top-days 3 --min-fraction 0.02
# 2. ft8_lib truth for the selected slots (existing tool; idempotent)
cargo run --release -p pancetta-research --example batch66_ft8lib_truth -- --manifest research/corpus/curated/ft8/storm_top3.manifest.json
# 3. Paired A/B on the storm-day slots, then the original bar (c)
cargo run --release -p pancetta-research --example batch104_impulse_llr_storm_remeasure -- \
  --manifest research/corpus/curated/ft8/storm_top3.manifest.json --output research/scorecards/pan162-storm-days.json
cargo run --release -p pancetta-research --example batch104_impulse_llr_storm_remeasure -- \
  --manifest research/corpus/curated/ft8/hard_200.manifest.json --limit 50 --output research/scorecards/pan162-hard200-50.json
# 4. Standard graduation (RUNBOOK §Running an experiment)
cargo run --release -p pancetta-research --bin eval -- --tier curated-hard-1000 --mode ft8 --output research/scorecards/pan162-hard1000-control.json
cargo run --release -p pancetta-research --bin eval -- --tier curated-hard-1000 --mode ft8 --impulse-robust-llr <k> --output research/scorecards/pan162-hard1000-variant.json
cargo run --release -p pancetta-research --bin compare -- research/scorecards/pan162-hard1000-control.json research/scorecards/pan162-hard1000-variant.json
```

Pre-registered rule for these results:
- **If the default stays off:** turn it on, as a follow-up ticket reusing Phase 6, only if the storm-day paired recall CI lower bound is > 0 for some k, hard_200/50 ΔTP ≥ −2, and hard-1000 `compare` is not significantly negative.
- **If the default shipped:** revert the coordinator assignment if hard_200/50 ΔTP < −2, or the storm-day or hard-1000 paired recall CI is significantly negative.
- **Either way:** record each day's mean impulsive-block fraction next to the synthetic plants' values (≈0.044 base, ≈0.087 storm, ≈0.186 severe) to show how stormy the real days were compared with the decision plant.
