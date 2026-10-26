---
slug: pan162-impulse-llr-storm
mode: ft8
state: tooling-built-measurement-pending
created: 2026-10-25T23:55:00Z
last_updated: 2026-10-25
branch: PAN-162
parent_hypothesis: hb-256
scorecard: research/scorecards/pan162-impulse-llr-offair.json (storm scorecard research/scorecards/pan162-impulse-llr-storm.json not yet produced)
delta_vs_main: none in production. Off-air do-no-harm (c′) on 60 vendored WAVs — ΔTP k=6 +1, k=3 -7
disposition: STAYS OFF BY DEFAULT (no verdict yet). The synthetic storm shards have not been run, so the pre-registered rule has nothing to decide on; `Ft8Config::impulse_robust_llr` stays `None` and production is unchanged. Phase 6 skipped: no verdict.
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

**Not run:** the twelve synthetic shards (`awgn`, `base`, `storm`, `severe` ×
`off`, `k3`, `k6`; 29,400 decodes) and `--summarize`. At the measured off-air
throughput they need well over half an hour of exclusive CPU, which did not
fit beside the repository gate inside this run's deadline. They are the next
step (commands below); nothing about the pre-registration changes in the
meantime. The port-fidelity cross-check against batch101's published rows
runs with them.

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
still become a candidate, and only if the storm plant clears bar (b).

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

## Verdict

No verdict yet: the storm scorecard does not exist, and the default stays off
— the ticket's "stays off by default otherwise, same as today". When the
shards are run, `--summarize` writes the scorecard, prints `verdict_line()`,
and this doc must quote that line verbatim.

Phase 6 skipped: no verdict.

## Next step: the pre-registered synthetic run

```sh
cargo build --release -p pancetta-research --example batch104_impulse_llr_storm_remeasure
PAN162_RUN=$(mktemp -d /tmp/pan162-measurement.XXXXXX); export RAYON_NUM_THREADS=<CPU quota>
B=target/release/examples/batch104_impulse_llr_storm_remeasure
for plant in awgn storm base severe; do for config in off k3 k6; do
  $B --plant $plant --config $config --shard-dir "$PAN162_RUN/shards"
done; done
$B --summarize --shard-dir "$PAN162_RUN/shards" \
  --offair research/scorecards/pan162-impulse-llr-offair.json \
  --output research/scorecards/pan162-impulse-llr-storm.json
```

Shards are resumable. If `--summarize` reports that the grid-extension rule
applies, run exactly the extra shard commands it prints, then summarize again.

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
