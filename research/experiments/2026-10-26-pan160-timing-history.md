---
slug: pan160-timing-history
mode: ft8
state: experimental-no-gain
created: 2026-10-26
base_commit: 2420c36463d600cab78d69adbe121cfb6211ac12
source_ticket: PAN-160
---

# PAN-160 timing-history results

The AP timing retry ran on learned histories but added no exact-target
recoveries in this experiment. Both arms recovered 128/200 target messages,
including 88/160 weaker post-warmup messages. There were no lost target
recoveries. The original mini33 experiment remains blocked by missing inputs.
Neither requested empirical outcome is complete, and both timing flags remain
false by default. These are native decoder measurements, with no production
coordinator integration or on-air performance claim.

| Dataset | Arm | Status | Completed slots | Warm slots | Truth recovered / truth | New target recoveries | Lost target recoveries | Novel messages | Extra AP attempts | Elapsed seconds |
|---|---|---|---:|---:|---|---:|---:|---:|---:|---:|
| mini33-original | general-V2 | blocked_missing_inputs | 0/33 | null | null | null | null | null | null | null |
| synthetic stable | ordinary AP control | measured | 100/100 | 80 | 86/200 | 0 | 0 | 2 | 0 | 71.113 |
| synthetic stable | AP timing | measured | 100/100 | 80 | 86/200 | 0 | 0 | 2 | 60 | 71.631 |
| synthetic moderate | ordinary AP control | measured | 100/100 | 80 | 82/200 | 0 | 0 | 0 | 0 | 66.786 |
| synthetic moderate | AP timing | measured | 100/100 | 80 | 82/200 | 0 | 0 | 0 | 98 | 66.449 |
| four-slot real supplement | ordinary AP control | inconclusive_no_eligible_priors | 4/4 | 0 | null | null | null | null | 0 | 5.990 |
| four-slot real supplement | AP timing | inconclusive_no_eligible_priors | 4/4 | 0 | null | null | null | null | 0 | 6.005 |

The truth totals include the target and a second station at another frequency
and timing. Target-only recall was 66/100 in each stable arm and 62/100 in each
moderate arm. The extra AP attempts accepted no messages. The session-level
bootstrap produced a recall-delta interval of [0, 0] for these observations.
This describes this fixed generator and configuration; it does not establish
that timing history cannot help other recordings.

## Original recordings

Both preflight attempts exited 1 with `blocked_missing_inputs: 66 rejected
inputs`. All 33 original WAVs and all 33 SHA-keyed baseline caches are absent.
[preflight.json](../scorecards/pan160/preflight.json) lists every expected
path and digest. No original-recording recall scorecard was created. The four
committed fixture WAVs are a separate supplementary check and are not mini33.

The driver accepts an optional local media directory, resolves original
basenames beneath it, and checks SHA-256 before decoding. Typed baseline
caches must match both schema version and WAV SHA. Missing caches never
become empty truth. Its available-input path runs general control/V2 first,
then a separately initialized AP-only pair. The supplied baseline directory
is used directly for truth; `--fp-filter-baselines` is not misused to relocate
truth. The original `compare` compatibility/bootstrap run remains unexecuted
because no original scorecards can be measured here.

General control maintains the legacy history and observes residual lookup
eligibility without applying the filter. Its outside-window counters describe
would-be rejections. V2 treatment applies the unchanged filter. Both preserve
the historical wall-clock recorder and expiry-on-record policy. The AP pair
uses separate accepted-native histories and explicit slot timestamps.

## Portable experiment and controls

The frozen [configuration](../scorecards/pan160/evaluation-config.json) uses
10 stable sessions with seeds 1000-1009 and 10 moderately varying sessions
with seeds 1100-1109. Each session contains two strong -6 dB warmup slots and
eight slots cycling through -19, -20 and -21 dB. The target is
`K1ABC W1AW -12` at 1000 Hz and DT 0.36 s. Moderate DT adds the repeating
sequence [0, 0.04, -0.04, 0.08, -0.08] s. `CQ K2XYZ FN42` is mixed at
1600 Hz, DT 0.8 s and half the target amplitude.

Audio uses the existing `modulate_message_at`, `place_in_slot` and seeded
`add_awgn_2500hz_ref` helpers. Each arm reads the same float WAV bytes. Stored
hashes cover little-endian f32 samples; the generator parameters and seeds
allow regeneration without private media. The two arms have independent
histories, identical explicit AP context, and V2 disabled. Only
`ap_dt_history_enabled` differs. The fixed filter applies native acceptance
then rejects `has_high_risk_fp_pattern` before AP learning and scoring.

[synthetic-sessions.json](../scorecards/pan160/synthetic-sessions.json)
contains every slot's truth, decoded identities, actual pre-decode eligible
senders, counters and elapsed work. Comparisons freeze eligibility from the
control arm, retain each arm's own eligibility, and include cold slots and
the historical slots 1-5 and 6-33 views. [synthetic-summary.json](../scorecards/pan160/synthetic-summary.json)
resamples whole independent sessions 1000 times with seed `0xb007`.
A preliminary run reused noise seeds across the two DT groups; it was
replaced with disjoint groups before producing these final records.

Three calibration sweeps preceded held-out evaluation: 144 broad trials,
90 trials that proved too strong, and 90 trials around the actual sensitivity
transition. Their [broad](../scorecards/pan160/calibration.json),
[strong](../scorecards/pan160/calibration-strong.json), and
[marginal](../scorecards/pan160/calibration-fine.json) records retain all
outcomes, including 492 extra attempts and zero exact-target rescues. Thus no
positive rescue fixture was found. The positive waveform criterion is unmet;
the regression suite does not replace it with a mocked success.

[controls.json](../scorecards/pan160/controls.json) contains no-context,
one-sighting, stale-history, wrong-sender, shifted-DT and 100 noise-only pairs.
The first four controls launch no timing attempts. The shifted-DT control
launches one and accepts none. Noise uses seeds 2000-2099, valid-looking AP
context and two prior sightings. Treatment makes 101 extra attempts and
accepts none of them. Both arms nevertheless produce the same two false
positives through their ordinary paths: seed 2009 emits `IZVV79TXHX <...>`;
seed 2084 emits `CT8LIU I35HZB/P R CN11`. These remain in the denominator.
No noise control emits the target callsign. Non-noise control target decodes
already occur in ordinary AP and are unchanged by the retry.

[supplemental.json](../scorecards/pan160/supplemental.json) replays the four
`basicft8/170923_0820{00,15,30,45}.wav` fixtures in timestamp order. Neither arm
returns a message or reaches an eligible prior. Trusted truth is unavailable
for this check, so its recall and novelty fields remain null.

## Implementation and validation

`prior_at` rejects expired, future and nonfinite sightings and counts a
supplied slot timestamp once. The AP recorder deduplicates native sender
observations, rejects conflicting timings, and learns only after the window.
Legacy V2 storage is separate. The replay clock is fixed per native decoder
instance; neither truth nor future messages populate history.

The optional FT8 queue runs after ordinary candidate work. It uses the signed
native sample-offset conversion, the existing Costas score and AP acceptance
routines, exact normalized sender binding, four senders, four failed seed
frequencies per sender and two ranked new positions per seed. At most 32
extra LDPC calls run per window across passes. It requires an existing
frequency seed, never tries receiver-only hints, and checks the deadline
again after extraction. All public decode entry points already share the
same Rayon window driver, so the unused legacy serial `try_ap_decode`
implementation does not need a second retry loop.

Focused tests cover strict preflight, fresh history, slot independence,
conversion boundaries, sender selection, wrong-sender rejection, the exact
32-attempt cap, deterministic queue selection, deadlines, default-off tuples,
FT4, combined flags, and actual V2 lookup telemetry. The research integration
test regenerates a learned-history session and recomputes paired report
counts from identities. The positive waveform rescue remains absent.

Host: AMD Ryzen 9 5950X, 32 visible logical CPUs, cgroup quota
`600000 100000` (six CPUs), 9 GiB memory limit. Rust and Cargo are 1.98.0.
The dev profile optimizes pancetta-ft8 at level 3, without debug information.
Measured arms ran serially with `RAYON_NUM_THREADS=4`, unlimited decode
budgets, 30 LDPC iterations, no OSD, one main pass, 100 sync candidates and
50 candidate limit. Full effective configs are stored alongside the records.
Supplemental fixtures use defaults except the AP timing flag. No evaluator
ran concurrently with another measured arm. Calibration overlapped some
compilation/test work, so its elapsed values are not performance evidence.
Final paired timings measure decoder work on this shared host, not contact
latency or a statistically established speedup.

Experiment commands, all from the repository root:

```sh
cargo build -p pancetta-research --example pan160_timing_history
RAYON_NUM_THREADS=4 target/debug/examples/pan160_timing_history research/corpus/curated/ft8/chrono_replay_mini33.manifest.json research/baselines/ft8 research/scorecards/pan160
RAYON_NUM_THREADS=4 target/debug/examples/pan160_timing_history calibrate research/scorecards/pan160
RAYON_NUM_THREADS=4 target/debug/examples/pan160_timing_history calibrate-fine research/scorecards/pan160
RAYON_NUM_THREADS=4 target/debug/examples/pan160_timing_history portable research/scorecards/pan160
RAYON_NUM_THREADS=4 target/debug/examples/pan160_timing_history controls research/scorecards/pan160
RAYON_NUM_THREADS=4 target/debug/examples/pan160_timing_history supplemental research/scorecards/pan160
```

The build and all measured experiment commands exited 0. Original preflight
exited 1 on both attempts. The strong calibration record predates the finer
mode's SNR change; its individual seed/SNR/DT values remain explicit in that
record. The final portable generator uses the frozen configuration, and its
control/treatment deltas describe the native, unlimited-budget experiment.

Focused checks completed during implementation: fresh-history tests (9),
retry and boundary tests (4), waveform/compatibility tests (6), research
library (138), and evaluator (18). Final build and repository gate outcomes
are recorded in the dispatch receipts. The gate is `scripts/check.sh`, with
single-threaded Rust tests for deterministic Hamlib mocks. No CI exclusion,
acceptance threshold, test skip, station configuration or feature default
was weakened.
