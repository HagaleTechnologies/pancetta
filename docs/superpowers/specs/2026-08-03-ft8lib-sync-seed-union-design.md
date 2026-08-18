# PAN-7 — ft8_lib sync-candidate seed union (Design Spec)

**Status:** IMPLEMENTED, MEASURED, DECLINED
**Date:** 2026-08-08
**Hypothesis:** ft8_lib may identify useful Costas positions absent from pancetta's pass-0 candidate set
**Mode:** FT8 only
**Effort:** Offline research mechanism
**Default:** OFF behind `Ft8Config::ft8lib_sync_seeds_enabled`; production behavior byte-identical until graduated.

## 0. License note

This mechanism intentionally calls and follows the vendored, MIT-licensed ft8_lib implementation. Candidate-search behavior comes from `vendor/ft8_lib/ft8/decode.c`; waterfall construction and monitor geometry come from `vendor/ft8_lib/common/monitor.c`. Call-site comments preserve that attribution. No GPL implementation is copied by this work.

## 1. Mechanism

When the flag is enabled and pass 0 has budget remaining, pancetta passes the raw `f32` residual slot audio to ft8_lib's `monitor_*` and `ftx_find_candidates`, requesting `max_sync_candidates` seeds. ft8_lib contributes positions only. Pancetta translates, filters, re-scores, unions, deduplicates, sorts, and truncates them with its own pipeline.

The flag is a runtime no-op under `ft8lib_stub`. It is not exposed through `pancetta-config`; the research wrapper and `eval` CLI own opt-in measurement.

## 2. Coordinate translation

For compatible grids:

```text
time_step = time_offset * TIME_OSR + time_sub + spectrogram.time_padding
freq_bin  = monitor.min_bin + freq_offset
freq_sub  = seed.freq_sub
```

`SLIDING_FRAME_LOOKBACK_STEPS` is not applied. It corrects spectrogram-row-to-reported-sample time for both native and seeded candidates; it is not part of row identity. Independently, substituting a native row into `reverse_derive_candidate`'s reported-dt inverse returns that row only when no extra lookback is added. Feeding ft8_lib's reported `time_sec` back through that inverse would land two steps late.

Negative rows are rejected, not clamped. Frequency/time bounds, caller scope, `freq_sub`, oversampling factors, and symbol block size must all match the native spectrogram envelope.

## 3. Re-scoring and union

The ft8_lib integer score is diagnostic only. Every translated position is scored through the same helper as the native sweep: full Costas score, optional partial-metric maximum, and the same parabolic time interpolation. This deliberately goes beyond a bare `compute_costas_score`; otherwise seeds would be systematically biased low relative to native candidates.

The block performs key-grouping sort, exact `(time_step, freq_bin, freq_sub)` deduplication retaining the best score, descending score sort, and truncation. `SyncCandidateRecord::via_ft8lib_seed` attributes surviving candidates. The `ft8.seed` log and `S0-ft8lib-seed` budget stage expose raw, translated/dropped, and kept counts.

## 4. Known ceilings

1. Native Costas search is exhaustive on the same representable lattice. At cap 200 and 400, measurement found no seed-only position survived into a changed result.
2. ft8_lib searches negative time offsets, but pancetta's current zero-padding representation cannot encode them. This design does not test slot-edge recovery.
3. A novel below-threshold seed can enter AP0, but the equal `MIN_SYNC_SCORE_FOR_AP` threshold prevents AP1–AP4 use.
4. The research novel classifier also ingests ft8_lib baseline JSON, making the unverified-novel gate favourable to this mechanism. jt9 truth remains the binding recall oracle.

## 5. Ship gate and result

The standing gate is “ΔTP ≥ threshold with bootstrap CI excluding zero, ΔFP-on-noise = 0, Δunverified-novels ≤ 2× ΔTP.” Sixteen scorecards at caps 200 and 400 showed exact seeded/control nulls: curated ΔTP=0 and Δunverified=0 with 95% CI `[0,0]`; noise ΔFP=0; synth recovery unchanged. The TP clause failed, so the flag remains off.

Full evidence: `research/experiments/2026-08-03-ft8lib-sync-seed-union-declined.md` and `research/scorecards/pan7/`.

## 6. Revisit boundary

A revisit must be a distinct slot-edge experiment: add representable negative time padding, use partial-Costas scoring for clipped sync blocks, construct a slot-edge-specific corpus, and retain paired cap-only controls. Merely increasing the cap or re-running the same lattice is not sufficient.
