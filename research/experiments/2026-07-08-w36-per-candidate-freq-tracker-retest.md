---
slug: w36-per-candidate-freq-tracker-retest
mode: ft8
state: declined
created: 2026-07-08
last_updated: 2026-07-08
parent_plan: docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md (Workstream 3)
scorecard: research/scorecards/w36-freqtracker-{hard200,synthclean}-{control,variant}.json
delta_vs_control: hard-200 rec Δ=-2 (95% CI [-5,0], NOT significant); hard-200 unverified-novels Δ=+3 vs allowance 2×ΔTP=0 (ΔTP negative) — HARD GATE FAILURE; noise_1000 FP unchanged (1 -> 1)
disposition: DECLINED — per-candidate frequency tracker does not help as a matched-demod-stage consumer either; net neutral-to-negative on hard-200 with a clean unverified-novel-growth gate failure. Ft8Config::per_candidate_freq_tracker_enabled stays default false.
---

## Task

Decoder-TP-sensitivity plan Task W3.6 (LAST task in Workstream 3). Brief: "Drift correction
(freq_tracker.rs) was built for the fine path; under W3.3 it finally has a worthy consumer. A/B
with W3.3 on; adopt or record why not; log; commit."

**IMPORTANT status inherited from W3.3/W3.3b/W3.4**: `fine_sync_enabled` (the Task W3.3
matched-demod stage) is declined by default in production — not because its own recall mechanism
doesn't work (it proved a genuine +54 TP gain on hard-200, `research/experiments/2026-07-08-w33-matched-demod-fine-sync.md`),
but because of a separate architectural finding (Task W3.3b,
`research/experiments/2026-07-08-w33b-budgeted-fine-sync-remeasurement.md`): one expensive
matched-demod candidate can starve a whole window's decode budget under a real bounded
`DecodeBudget`, causing a net recall regression at the realistic `Standard` preset. This means
`per_candidate_freq_tracker_enabled`'s effect measured below is **doubly inert-by-default today**
regardless of its own result — exactly the framing W3.4 used for `nsym_combining_enabled`, which
also only executes inside `matched_demod_attempt`.

## Locating the actual current state (verified before writing any code)

Per the task instructions, re-grepped the ACTUAL codebase state rather than trusting the brief's
"retest" framing at face value:

- `pancetta-ft8/src/freq_tracker.rs` — `FrequencyTracker`/`FreqTrackerConfig` already fully
  built and unit-tested (13 tests from Batch 50, 2026-06-09;
  `research/experiments/2026-06-09-batch-50.md`).
- `Ft8Config::per_candidate_freq_tracker_enabled` — already exists, default `false`, already wired
  into ONE consumer: the legacy 21-trial fine-FFT fallback (`par_extract_symbols_complex`, reached
  when `fine_sync_enabled = false`, the pre-existing/default production path). Batch 50 already
  measured this exact consumer on hard-200: **-2 TPs** ("HF static signals; tracker adds drift
  noise"). So the flag was NOT "never tested" — it already had a consumer and a negative-but-small
  result; the brief's "finally has a worthy consumer" claim is about the SECOND, NEW consumer this
  task adds, not the flag's very first exposure.
- `matched_demod_attempt` (`decoder.rs`, Task W3.3) — confirmed this function had **NO** frequency-
  tracker wiring at all before this task. It computes exactly ONE global `(dt, df)` pair per
  candidate via `fine_sync::refine` (a single 2D grid search over the WHOLE ~12.64 s slot's Costas
  correlation, `pancetta-ft8/src/fine_sync.rs`) and applies that single `df_hz` uniformly to every
  one of the 79 symbols via `matched_demod_tone_correlation`. This is the genuinely new "worthy
  consumer" gap the brief refers to: a per-candidate drift tracker has real headroom here because
  the existing correction is a single static number for the whole slot, with no mechanism to follow
  intra-slot drift beyond it — exactly analogous to how the legacy path's own fine-FFT loop (Batch
  50's original consumer) is also a fixed-per-candidate correction, just via a blunter 21-trial grid
  instead of a continuous search.

**Conclusion**: the mechanism itself was fully built (nothing missing/broken), but genuinely had a
real integration gap for the W3.3 stage. Built the wiring per Step 2 of the task's "Your Job."

## Implementation: wiring the tracker into `matched_demod_attempt`

`matched_demod_attempt` gained 4 new parameters (mirroring `Ft8Config`'s existing
`per_candidate_freq_tracker_{enabled,alpha,max_step_hz,max_error_hz}` fields, already present in
`DecodeContext` since Batch 50 — no new config surface needed), threaded through all 4 call sites:
`par_matched_demod_decode` (`par_decode_candidate`'s caller, via `ctx.*`) and the three rescue
passes (`cross_cycle_averaging_pass`, `joint_pair_retry_pass`,
`joint_residual_localized_sync_pass`, via `self.config.*` — the same pattern `fine_sync_enabled`
and `nsym_combining_enabled` already use for these exact 4 sites).

Inside `matched_demod_attempt`'s per-symbol loop:
- A `FrequencyTracker` is constructed (`None` when the flag is off — no allocation, no behavior
  change) at `coarse_hz = candidate_freq_hz`, offset starting at 0. `effective_df_hz` for each
  symbol is `df_hz + tracker.current_offset_hz()` (or plain `df_hz` when the tracker is absent — no
  redundant floating-point op, byte-identical to pre-task code).
- At each of the 3 Costas (pilot) blocks (protocol-generic via `pp.costas_positions`/
  `pp.costas_length`/`pp.costas_value`, so this also works for FT4's 4-block layout untouched by
  this task's A/B), a cheap 3-point correlation-power micro-search (`effective_df_hz` ± a 0.5 Hz
  step, matching `fine_sync::refine`'s own grid step) is accumulated across that block's known-tone
  symbols. The center point costs nothing extra (it's already `mags[tone]` from the per-symbol LLR
  loop); only the ± step endpoints need 2 extra `matched_demod_tone_correlation` calls per Costas
  symbol (42 extra correlations per FT8 candidate, against ~632 total — a small fraction).
- At the last symbol of each block, the 3-point sum is parabola-refined (the same
  `parabolic_peak_refinement` helper `fine_sync.rs` uses for its own axis refinements) into a
  residual and fed to `tracker.update(residual_hz)` — the tracker's existing damped-step/clamped-
  offset logic (unchanged from Batch 50) does the rest. This is exactly the "consume a residual
  measurement... at each pilot opportunity" algorithm `freq_tracker.rs`'s own module doc already
  describes; only the caller (a new one) is new.

Byte-identical-by-default verified 3 ways: (1) direct code read — the `None` branch of every new
match/if is textually `df_hz` alone, no new arithmetic; (2) all 17 pre-existing
`w33_matched_demod`/`w34_nsym_combining` tests (which leave this flag at its default `false`) still
pass unmodified; (3) full `cargo test --features transmit -p pancetta-ft8 --lib` (492 tests) green.

## TDD / regression tests added

`pancetta-ft8/tests/decoder_refinement_tests.rs::w36_freq_tracker` (2 new tests):
1. `per_candidate_freq_tracker_default_is_off` — regression guard, flag stays `false`.
2. `tracker_enabled_does_not_break_clean_decode` — sanity: `fine_sync_enabled=true` +
   `per_candidate_freq_tracker_enabled=true` still decodes a clean, on-grid signal (mirrors
   `test_fine_fft_rect_window_flag_does_not_break_clean_decode`'s rationale — a clean strong signal
   usually decodes via the coarse path before `matched_demod_attempt` is even reached, so this is a
   wiring regression guard, not a sensitivity claim).

**No discriminating RED/GREEN synthetic fixture was found** (unlike `w33_matched_demod`/
`w34_nsym_combining`'s curated-seed pairs). An exploratory sweep built a hand-rolled
phase-continuous (CPFSK, no Gaussian pulse shaping — a from-scratch test-only generator, not a
reuse of `Ft8Modulator`, since it needed a linear intra-slot frequency ramp `Ft8Modulator::
modulate_symbols` cannot express with its single constant `frequency_offset` parameter) signal with
a linear drift symmetric about the slot midpoint (0 Hz at slot start/end on average, ramping
through ±drift/2), first without any off-grid dt/df offset (drift 4-12 Hz × SNR -8..-16 dB × 20
seeds): the coarse spectrogram path proved highly robust to drift in this range — flag-on and
flag-off gave IDENTICAL pass counts at every point tested, including the one point that started
failing (12 Hz drift, -16 dB: 9/20 both), meaning most of these fixtures likely never reached
`matched_demod_attempt` at all (coarse path succeeds or fails identically regardless of the new
flag). A second attempt added `w33_matched_demod`'s own proven off-grid dt/df prerequisite
(+1.4 Hz / +37 ms, forcing coarse-path failure and reliable entry into the fine-sync stage) plus
drift (0-15 Hz) at SNRs near `w33_matched_demod`'s own -23 dB discriminating point, but was cut
short by the time budget available for this task (see "Process note" below) after only the
zero-drift sanity point completed. Given (a) the real corpus-level A/B below is the plan's standing
decisive evidence (not a bespoke unit fixture — the same methodology precedent as every other task
in Workstream 3), and (b) the corpus result is a clean, decisive DECLINE regardless, further sweep
time was not spent chasing a fixture whose only purpose would have been illustrative.

## Process note: measurement took much longer than W3.3/W3.4's precedent runs

`--tier curated-hard-200,noise_1000` took **1792.0s** (control) / **1715.9s** (variant) here, vs.
W3.3's own combined-tier report of ~903s for the identical tier combination on `fine_sync_enabled`
alone. The extra time is attributable to host contention from a concurrently-running exploratory
sweep process (see above) for roughly the first half of each run — confirmed by `ps` CPU%
measurements jumping from ~100-200% to 300-460% immediately after the sweep was killed. This does
not affect the correctness of the recall/FP numbers (elapsed time is not a gate criterion this task
measures; W3.3 already owns the elapsed-cost finding for the underlying `fine_sync_enabled` stage).

## The A/B: hard-200 + noise_1000, `fine_sync_enabled=true` in BOTH legs (isolating this flag)

Same isolation technique as W3.4: `fine_sync_enabled` forced `true` in both control and variant so
this flag's own effect is isolated from W3.3's already-known effect.

```
cargo build --release -p pancetta-research --bin eval --bin compare

./target/release/eval --tier curated-hard-200,noise_1000 --mode ft8 --fine-sync-enabled \
    --output research/scorecards/w36-freqtracker-hard200-control.json
# wrote scorecard: composite raw 0.3261, saturation-aware 0.3282, 2 tier(s), 1792.0s
# noise_1000: 1 FALSE POSITIVE decode(s) across 1/1000 noise-only WAVs

./target/release/eval --tier curated-hard-200,noise_1000 --mode ft8 --fine-sync-enabled \
    --per-candidate-freq-tracker-enabled \
    --output research/scorecards/w36-freqtracker-hard200-variant.json
# wrote scorecard: composite raw 0.3256, saturation-aware 0.3277, 2 tier(s), 1715.9s
# noise_1000: 1 FALSE POSITIVE decode(s) across 1/1000 noise-only WAVs

./target/release/compare research/scorecards/w36-freqtracker-hard200-control.json \
    research/scorecards/w36-freqtracker-hard200-variant.json
```

```
A: research/scorecards/w36-freqtracker-hard200-control.json (sha 5fa3d1b0, score 0.3261)
B: research/scorecards/w36-freqtracker-hard200-variant.json (sha 5fa3d1b0, score 0.3256 -0.0005)

REGRESSIONS:
  curated-hard-200      decode_rate   0.6522 → 0.6512  (-0.0010)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200          rec Δ=-2  (95% CI [-5.0, +0.0], n_bootstrap=1000) — NOT significant
  curated-hard-200          novel Δ=+7  (95% CI [+2.0, +13.0], n_bootstrap=1000) — significant

CONFIG DIFF:
  decoder.decoder.per_candidate_freq_tracker_enabled false → true

############################################################
# HARD GATE FAILURE — UNVERIFIED-NOVEL GROWTH EXCEEDS 2×ΔTP
############################################################
  aggregate: unverified-novels Δ=+3 exceeds allowance +0 (2×ΔTP, ΔTP=-2)
  curated-hard-200      verified-TP Δ=-2   unverified-novels Δ=+3
```

`truth_decodes_recovered`: control 1305 → variant 1303 (Δ=-2, hard-200 tier). `novels_verified`:
control 3255 → variant 3258 (+3, inside `novel Δ=+7`'s total which also includes
`novels_unverified` movement). noise_1000 FP-on-noise unchanged (1 → 1 both legs — the tracker
correction cannot itself invent a new sync candidate, only re-aim an already-admitted one's
frequency, so a flat FP count here is the expected/unsurprising outcome, not a notable finding).

## Gate check (standing rule: ΔTP with bootstrap CI excluding zero, ΔFP-on-noise = 0,
Δunverified-novels ≤ 2×ΔTP)

- **ΔFP-on-noise = 0**: passes (1 → 1, both legs).
- **ΔTP**: hard-200 recall **Δ=-2**, 95% CI **[-5, 0]** — the CI includes zero (not a decisively
  significant regression), but it is NOT a positive result either — the point estimate is negative
  and the CI's upper bound sits exactly at zero. This is a genuine null-to-slightly-negative
  result, not the "clearly positive" outcome the flip needs.
- **Unverified-novel allowance**: this is where the flip decisively fails. The standing rule is
  "unverified-novel growth ≤ 2×ΔTP." With `ΔTP = -2` (negative), the allowance is `2×(-2) = -4`,
  clamped to `0` by `compare`'s own allowance formula (an allowance can't go negative) — and the
  measured unverified-novel growth is `+3`, which exceeds `0`. **HARD GATE FAILURE**, flagged
  automatically by `compare`'s own regression scanner, not a judgment call on my part.

**Decision: DECLINE.** `Ft8Config::per_candidate_freq_tracker_enabled` stays default `false`.

## Why the hypothesis didn't hold here either

Consistent with Batch 50's original finding on the OTHER consumer ("HF static signals; tracker
adds drift noise"): the hard-200 corpus is real off-air recordings, most of which are NOT
meaningfully drifting within one 12.64 s slot (this plan's own synthetic exploration above found
the coarse decode path already tolerates 4-12 Hz of injected linear drift at these SNRs without any
observable differential effect). Absent real drift to correct, the tracker's 3-point-per-block
residual measurement is estimating pure noise (its `update()` damping/clamping keeps single
estimates bounded, but the measurement itself has no signal to lock onto), occasionally nudging an
already-marginal candidate's per-symbol frequency correction enough to flip a borderline BP/CRC
outcome — sometimes helping (a few `novels_verified` gained), slightly more often hurting (the net
`ΔTP=-2` and the unverified-novel growth). This is the same "adds noise where there's no real
drift to track" story Batch 50 told about the legacy-path consumer, now independently reproduced on
a SECOND, architecturally distinct consumer — a meaningfully stronger claim than either measurement
alone: it suggests the tracker's fundamental limitation (a 3-Costas-block measurement cadence is
simply too sparse/noisy a drift estimator for pancetta's real-world corpus) is a property of the
MECHANISM itself, not an artifact of either specific integration point.

## Synth-clean curve (SNR-sensitivity, secondary evidence per this task's brief)

```
./target/release/eval --tier synth-clean --mode ft8 --fine-sync-enabled \
    --output research/scorecards/w36-freqtracker-synthclean-control.json
# wrote scorecard: composite raw 0.1486, saturation-aware 0.1507, 1 tier(s), 884.1s

./target/release/eval --tier synth-clean --mode ft8 --fine-sync-enabled \
    --per-candidate-freq-tracker-enabled \
    --output research/scorecards/w36-freqtracker-synthclean-variant.json
# wrote scorecard: composite raw 0.1486, saturation-aware 0.1507, 1 tier(s), 860.1s

./target/release/compare research/scorecards/w36-freqtracker-synthclean-control.json \
    research/scorecards/w36-freqtracker-synthclean-variant.json
```

```
A: research/scorecards/w36-freqtracker-synthclean-control.json (sha 5fa3d1b0, score 0.1486)
B: research/scorecards/w36-freqtracker-synthclean-variant.json (sha 5fa3d1b0, score 0.1486 +0.0000)

REGRESSIONS:
  (none)

BOOTSTRAP CI:
  (skipped: neither scorecard carries `per_wav_records`.)

CONFIG DIFF:
  decoder.decoder.per_candidate_freq_tracker_enabled false → true
```

**Byte-for-byte identical** on every one of the 11 SNR steps (`by_snr_db`, -24..-14 dB, 50 attempts
each): both `snr_at_50pct_recovery_db = -19.909...` and `snr_at_90pct_recovery_db = -19.0` match
exactly, and every single per-SNR `decoded` count is identical between control and variant. Unlike
W3.4's `nsym_combining_enabled` (which showed a genuine, if non-decisive, -0.3 dB synth-clean win),
`per_candidate_freq_tracker_enabled` produces **zero measurable effect whatsoever** on this corpus.
This is a clean, informative null result: `synth-clean`'s WAVs are single, static-carrier synthetic
signals with no injected drift, so the tracker's per-Costas-block residual measurements consistently
converge to ~0 (or oscillate around 0 symmetrically enough to never flip a single BP/CRC outcome
across 550 WAVs × 11 SNR points). Combined with the exploratory hand-built drift sweep above (which
also found the tracker made no observable difference even with several Hz of injected drift, at
every point tested before the sweep was cut short by time), this strengthens rather than
contradicts the hard-200 finding: absent real intra-slot drift, the mechanism is inert; hard-200's
real off-air recordings apparently have just enough non-drift-related noise in their marginal
candidates for the extra micro-search-driven corrections to occasionally tip a borderline decode
the wrong way, netting a small negative.

## Files changed

- `pancetta-ft8/src/decoder.rs`: `matched_demod_attempt` gained 4 new parameters + the per-symbol
  tracker/Costas-residual logic (threaded through `par_matched_demod_decode` and the 3 rescue
  passes); doc updates on `Ft8Config::per_candidate_freq_tracker_enabled` and the `DecodeContext`
  field noting the second consumer.
- `pancetta-ft8/src/freq_tracker.rs`: module doc updated to document both consumers and their
  measured results.
- `pancetta-ft8/tests/decoder_refinement_tests.rs`: new `w36_freq_tracker` test module (2 tests).
- `pancetta-research/src/decoder.rs`: `DecoderBuilder::with_per_candidate_freq_tracker_enabled`.
- `pancetta-research/src/bin/eval.rs`: `--per-candidate-freq-tracker-enabled` CLI flag (struct
  field, parse arm, help text, populate, apply site) — mirrors `--nsym-combining-enabled` exactly.
- `research/scorecards/w36-freqtracker-{hard200,synthclean}-{control,variant}.json` (new,
  committed as evidence).
