---
slug: hb-069-linear-power-interp
mode: ft8
state: shelved
created: 2026-05-31T23:00:00Z
last_updated: 2026-06-01T00:30:00Z
branch: iter/2026-05-31-hb-069
parent_hypothesis: hb-069 (linear-power interp rescue for hb-044/hb-068)
wild_card: false
scorecard: research/scorecards/sweep/hb069-{baseline,linear-power}.json
delta_vs_main: composite -0.003049 (-54 hard-200 rec, -94 hard-1000 rec); synth preserved
disposition: SHELVED — linear-power interp regresses composite and recall vs dB interp; production stays at b-0.3 dB
---

## Hypothesis

hb-069 proposed converting spectrogram values from dB → linear power
before parabolic time-axis interpolation, then back to dB. The
defensible prior was that hb-044's residual hard-200 cost (rescued
to -7 by hb-068 b-scale=0.3, but not eliminated) might be partly
caused by dB-space interpolation introducing non-physical energy
values near the noise floor. Linear-space interpolation should
preserve real symbol energies more accurately.

## Implementation

`Ft8Config::sync_time_interp_linear_power: bool` (default false).
When true, `lookup_time_interp` converts dB → linear (10^(dB/10)),
performs parabolic interpolation in linear power, then converts back
to dB. CLI flag `--sync-time-interp-linear-power` in eval.

Implementation committed (`a20fc6d`) — production behavior unchanged
at default false.

## A/B sweep on refreshed main.json baseline

Both runs: production config (b-scale=0.3, FP filter ON, multipass=3,
layered BP, etc.) + the linear-power toggle.

| tier | baseline (dB) | linear-power | Δ |
|---|---:|---:|---:|
| **composite** | **0.579114** | **0.576065** | **-0.003049** |
| fixtures pass_rate | 1.0 | 1.0 | 0 ✓ |
| synth-clean snr@50/@90 | -20/-20 | -20/-20 | 0 ✓ |
| synth-doppler @50/@90 | None/None | None/None | 0 (decoder fails Doppler, known) |
| hard-200 rec / novel | 4942 / 1024 | **4888 / 990** | **-54 rec / -34 novel** |
| hard-1000 rec / novel | 14987 / 3053 | **14893 / 3020** | **-94 rec / -33 novel** |
| wild-100 rec / novel | 1879 / 391 | 1879 / 386 | 0 rec / -5 novel |
| wild-50 | 0/0 | 0/0 | 0 |

## Decision: SHELVE

Linear-power interpolation regresses composite by -0.003049 and
loses recall on both hard tiers. The hypothesis that "dB-space
interpolation introduces non-physical energy values that hurt LDPC
LLR computation" is not supported by the data — the dB-space
interpolation is actually BETTER for the downstream LDPC pipeline.

## Why the hypothesis didn't hold

Two plausible structural reasons:

1. **Symbol energy estimation downstream operates in dB.** The LLR
   computation (`par_compute_soft_llrs_db`) consumes dB-domain tone
   magnitudes. If interpolation produces linear-space values that
   are then log-converted, that's two transformations of noise vs
   one — adding variance. dB-native interpolation skips one round
   of conversion error.

2. **Noise-floor estimation calibrates to dB-domain values.** The
   spectrogram's noise floor is estimated in dB by design. Linear-
   power interpolation produces values whose "noise floor" doesn't
   match the calibrated reference — small effect per bin but
   cumulative across symbol windows.

Both consistent with the small but consistent recall loss across hard
tiers and zero change on synth (where SNR is controlled, so any
calibration drift from linear-back-to-dB is minimal).

## Production change

**None.** `sync_time_interp_linear_power` stays at default false.
Config knob preserved for future research.

## Implications

- hb-068's b-scale=0.3 production setting is confirmed near-optimal
  for hb-044-class refinement on the refreshed corpus.
- The hb-044 family is now effectively closed: no remaining variants
  to explore (a/b/c/d from hb-068 batch 14, b-0.25 retested 2026-05-31
  → KEEP-b-0.3, linear-power interp → SHELVE here).
- Future refinement-axis hypotheses would need a fundamentally
  different mechanism (frequency-axis refinement? subsample timing
  via DFT-based fine sync? sub-bin spectrogram via FFT zero-padding?).
  None of those are urgent given the closed structural picture.
