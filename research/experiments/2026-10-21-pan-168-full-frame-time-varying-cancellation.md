---
slug: pan-168-full-frame-time-varying-cancellation
mode: ft8
state: shipped-opt-in
created: 2026-10-21T00:00:00Z
last_updated: 2026-10-21T00:00:00Z
branch: PAN-168
parent_hypothesis: a per-block amplitude/phase fit absorbs the residual fine_sync carrier error that defeats the full-frame path's whole-frame audio cancellation
wild_card: false
scorecard: research/scorecards/pan168-pair-*.json
delta_vs_main: "synth-pair-200: ff-tv 276/360 vs ff-whole 269/360; defaults 277/360, tv-only 277/360; no bootstrap CI (tier has no per_wav_records)"
disposition: SHIP OPT-IN. Keep Ft8Config::coherent_subtract_full_frame_time_varying_enabled false pending the real-signal gate below.
---

# PAN-168: per-block audio cancellation on the full-frame coherent-subtraction path

## Hypothesis

With `coherent_subtract_full_frame_reference_enabled` on, `coherent_subtract_and_repass` removes each decoded signal from `residual_audio` with `subtract_reconstructed_signal_at_known_position`. That function fits one constant amplitude and phase over the whole 12.64 s frame at `base_frequency + fine_sync::refine(..).df_hz`. A carrier error of a few hundredths of a hertz rotates the reference against the signal across the frame, so the fit cancels almost nothing and the signal stays in `residual_audio` for later candidates to trip over (PR #397 review, thread `discussion_r_PRRT_kwDORcJovs6lO3Ee`).

Fitting per ~2-symbol block instead, with the existing `Ft8Decoder::time_varying_complex_amplitude` (the W4.2 mechanism behind `time_varying_subtraction_enabled`), should absorb that error. The new flag is `Ft8Config::coherent_subtract_full_frame_time_varying_enabled` (default off; only takes effect under the parent flag, FT8 only).

## Mechanism-level evidence

Measured during the PAN-168 plan run (2026-10-21) with scratch tests on a real GFSK (BT=2.0) frame at `1500 + df` Hz, cancelled at the nominal 1500 Hz. The metric is the share of signal energy left behind, `‖signal − (before − after)‖² / ‖signal‖²`. The 1.0% floor at zero error is the fixed 0.9 subtraction scale.

Clean signal, whole-frame vs per-block:

| df (Hz) | whole-frame | per-block |
|---:|---:|---:|
| ±0.500 | 0.9982 | 0.0443 |
| ±0.300 | 0.9974 | 0.0202 |
| ±0.200 | 0.9844 | 0.0142 |
| ±0.116 | 0.9538 | 0.0113 |
| ±0.050 | 0.7891 | 0.0102 |
| 0.000 | 0.0100 | 0.0100 |

`fine_sync::refine`'s carrier error against SNR (a clone of `fine_sync.rs`'s `minus_18db_awgn_meets_median_tolerance_over_50_trials`, same seeds and draws, 50 trials per row):

| SNR (dB) | median | p90 | max | trials > 0.05 Hz | trials > 0.1 Hz |
|---:|---:|---:|---:|---:|---:|
| 99 | 0.001 | 0.002 | 0.002 | 0/50 | 0/50 |
| -5 | 0.025 | 0.054 | 0.080 | 6/50 | 0/50 |
| -10 | 0.047 | 0.098 | 0.142 | 19/50 | 3/50 |
| -14 | 0.069 | 0.159 | 0.233 | 32/50 | 15/50 |
| -18 | 0.116 | 0.261 | 0.391 | 38/50 | 26/50 |
| -21 | 0.193 | 0.386 | 0.597 | 41/50 | 37/50 |

At -10 dB, a typical strong masking signal, the median error already leaves about 74% of the signal behind under the whole-frame fit. So the problem is not limited to weak signals.

Noisy signal (WSJT-X SNR convention, `σ = signal_rms / 10^(snr/20) · √(6000/2500)`), 20 noise seeds per cell, mean residual fraction:

| SNR | df (Hz) | whole-frame | per-block |
|---|---:|---:|---:|
| -14 dB | 0.000 | 0.0103 | 0.0246 |
| -14 dB | 0.116 | 0.9551 | 0.0277 |
| -18 dB | 0.000 | 0.0114 | 0.0475 |
| -18 dB | 0.116 | 0.9568 | 0.0516 |
| -18 dB | 0.500 | 1.0008 | 0.0800 |
| -21 dB | 0.116 | 0.9595 | 0.0907 |
| -24 dB | 0.000 | 0.0178 | 0.1616 |
| -24 dB | 0.116 | 0.9649 | 0.1685 |

With an exact carrier, per-block leaves more behind than whole-frame, because each ~0.32 s block averages ~40× less signal. At any realistic `fine_sync` error, whole-frame leaves 95-100% behind and per-block leaves 3-17%.

The unit tests in `pancetta-ft8/src/decoder.rs` (`mod pan168_full_frame_time_varying_tests`) pin these properties: the whole-frame collapse at |df| ∈ {0.05, 0.116} Hz, per-block < 3% for |df| ≤ 0.2 Hz and < 8% out to 0.5 Hz on a clean signal, per-block mean < 10% vs whole-frame > 50% at -18 dB and df = 0.116 Hz, parity at an exact carrier, and buffer-edge placements.

## Alternative considered: phase-tracked refit

Take the per-block trajectory from `time_varying_complex_amplitude`, fit a weighted linear or quadratic polynomial to its unwrapped phase, derotate the reference by that fit, then do one whole-frame 2×2 refit. Drift simulated as a quadratic phase on the analytic signal; QSB as a 50%-depth sinusoidal fade. 12 seeds per noisy cell:

| Case | SNR | whole | per-block | quad-tracked refit |
|---|---|---:|---:|---:|
| df 0.116 | clean | 0.954 | 0.011 | 0.010 |
| df 0.116 | -18 dB | 0.956 | 0.054 | 0.017 |
| df 0.116 | -24 dB | 0.961 | 0.173 | 0.031 |
| drift 0.1 Hz/s | -18 dB | 0.953 | 0.067 | 0.015 |
| drift 0.5 Hz/s | clean | 0.989 | 0.543 | 0.120 |
| drift 0.5 Hz/s | -18 dB | 0.991 | 0.585 | 0.978 |
| QSB 50% @ 0.5 Hz | clean | 0.061 | 0.012 | 0.061 |
| QSB 50% @ 0.1 Hz | -24 dB | 0.061 | 0.162 | 0.244 |

Not chosen. The tracked refit wins on weak signals with a steady carrier, but it keeps one amplitude for the whole frame, so it loses under fading, and it loses under fast drift. At -24 dB, phase-unwrap errors make it worse than doing nothing. Strong signals cause the masking, and for them per-block is as good or better and tracks fading. Weak signals leave little absolute power behind either way: a -24 dB signal's 17% leftover sits ~6 dB under the noise in its own 6.25 Hz bins. The tracked refit is a candidate follow-up for weak-signal cancellation.

## Synthetic A/B: `synth-pair-200`

`synth-pair-200` is the on-target tier: a -8 dB strong signal masking a weaker one 0-12 dB down, 6-50 Hz away, 0-0.25 s offset (180 WAVs, 360 truths). It regenerates deterministically from its committed config. `synth-clean` was skipped: it holds one signal per WAV, so a cleaner `residual_audio` has no later candidate to help.

```bash
cargo build --release -p pancetta-research --bin eval --bin compare --bin gen-synth-pair --features research-eval
mkdir -p /tmp/pan168
./target/release/gen-synth-pair --config research/corpus/synth/manifests/synth_pair_200.config.json \
  --output /tmp/pan168/synth_pair_200.manifest.json
diff <(python3 -m json.tool /tmp/pan168/synth_pair_200.manifest.json) \
     <(python3 -m json.tool research/corpus/synth/manifests/synth_pair_200.manifest.json) && echo identical
# → MANIFEST IDENTICAL

E=./target/release/eval; T=/tmp/pan168
$E --tier synth-pair-200 --mode ft8 --coherent-subtract-full-frame-reference-enabled \
   --output $T/pan168-pair-ff-whole.json
$E --tier synth-pair-200 --mode ft8 --coherent-subtract-full-frame-time-varying-enabled \
   --output $T/pan168-pair-tv-only.json
$E --tier synth-pair-200 --mode ft8 --output $T/pan168-pair-default.json
$E --tier synth-pair-200 --mode ft8 --coherent-subtract-full-frame-reference-enabled \
   --coherent-subtract-full-frame-time-varying-enabled --output $T/pan168-pair-ff-tv.json

./target/release/compare $T/pan168-pair-ff-whole.json $T/pan168-pair-ff-tv.json --bootstrap; echo "exit=$?"
./target/release/compare $T/pan168-pair-default.json  $T/pan168-pair-tv-only.json --bootstrap; echo "exit=$?"
./target/release/compare $T/pan168-pair-default.json  $T/pan168-pair-ff-tv.json --bootstrap; echo "exit=$?"
```

Run in a 32-core container on 2026-10-21.

| Arm | Flags | strong / 180 | weak / 180 | truth recovered / 360 | decode_rate | `harness.elapsed_seconds` |
|---|---|---:|---:|---:|---:|---:|
| default | none | 176 | 101 | **277** | 0.7694 | 483.1 |
| tv-only | `--coherent-subtract-full-frame-time-varying-enabled` | 176 | 101 | **277** | 0.7694 | 484.7 |
| ff-whole | `--coherent-subtract-full-frame-reference-enabled` | 176 | 93 | **269** | 0.7472 | 744.9 |
| ff-tv | both | 176 | 100 | **276** | 0.7667 | 750.8 |

The two controls reproduce the plan's numbers exactly. `ff-whole` = 269 shows the flag-off path is unchanged; `tv-only` = 277 (identical to `default`) shows the new flag does nothing without its parent. `ff-tv` = 276 also matches the plan's scratch run. Per-block cancellation wins back 7 of the 8 truths that turning on the full-frame reference loses on this tier.

`synth-pair-200` scorecards carry no `per_wav_records`, so `compare` cannot compute a bootstrap CI for this tier. This is directional evidence, not a significance result.

**Timing.** The committed scorecards come from one run of all four arms launched together, with nothing else on the machine, so their `elapsed_seconds` share the same load and compare fairly. Per-block cancellation costs +0.8% over the whole-frame fit (750.8 s vs 744.9 s), and the new flag alone costs nothing (+0.3%, within noise). Two earlier passes (`ff-whole` alone at 336.4 s; the other arms in parallel with a `cargo build --all-targets`, then `default` and `ff-tv` rerun without it) produced identical decode counts in every arm, but their elapsed times were taken under different loads and are not comparable. On the first pass `compare` flagged a spurious ELAPSED GATE failure for `ff-whole` → `ff-tv` (336.4 s → 788.3 s).

The one `compare` hard-gate failure below (`default` → `ff-tv`, +55.4% elapsed) is the parent flag's cost, not this ticket's: `ff-whole` alone is +54.2% over `default` (744.9 s vs 483.1 s). Record it alongside the side finding below for any default flip of `coherent_subtract_full_frame_reference_enabled`.

### `compare` outputs

Scorecard paths shown as `$T/`; the committed copies are `research/scorecards/pan168-pair-*.json`.

```
$ ./target/release/compare $T/pan168-pair-ff-whole.json $T/pan168-pair-ff-tv.json --bootstrap; echo "exit=$?"
A: $T/pan168-pair-ff-whole.json (sha c6508d76, score 0.0000)
B: $T/pan168-pair-ff-tv.json (sha c6508d76, score 0.0000 +0.0000)

WINS:
  synth-pair-200        decode_rate   0.7472 → 0.7667  (+0.0194)

REGRESSIONS:
  (none)

BOOTSTRAP CI:
  (skipped: neither scorecard carries `per_wav_records`. Re-eval with the Phase-B build to enable.)

CONFIG DIFF:
  decoder.decoder.coherent_subtract_full_frame_time_varying_enabled false → true
ELAPSED GATE:
  elapsed_seconds        744.9 → 750.8  (+0.8%, budget +20.0%)
exit=0
```

```
$ ./target/release/compare $T/pan168-pair-default.json $T/pan168-pair-tv-only.json --bootstrap; echo "exit=$?"
A: $T/pan168-pair-default.json (sha c6508d76, score 0.0000)
B: $T/pan168-pair-tv-only.json (sha c6508d76, score 0.0000 +0.0000)

REGRESSIONS:
  (none)

BOOTSTRAP CI:
  (skipped: neither scorecard carries `per_wav_records`. Re-eval with the Phase-B build to enable.)

CONFIG DIFF:
  decoder.decoder.coherent_subtract_full_frame_time_varying_enabled false → true
ELAPSED GATE:
  elapsed_seconds        483.1 → 484.7  (+0.3%, budget +20.0%)
exit=0
```

```
$ ./target/release/compare $T/pan168-pair-default.json $T/pan168-pair-ff-tv.json --bootstrap; echo "exit=$?"
A: $T/pan168-pair-default.json (sha c6508d76, score 0.0000)
B: $T/pan168-pair-ff-tv.json (sha c6508d76, score 0.0000 +0.0000)

REGRESSIONS:
  synth-pair-200        decode_rate   0.7694 → 0.7667  (-0.0028)

BOOTSTRAP CI:
  (skipped: neither scorecard carries `per_wav_records`. Re-eval with the Phase-B build to enable.)

CONFIG DIFF:
  decoder.decoder.coherent_subtract_full_frame_reference_enabled false → true
  decoder.decoder.coherent_subtract_full_frame_time_varying_enabled false → true
ELAPSED GATE:
  elapsed_seconds        483.1 → 750.8  (+55.4%, budget +20.0%)

############################################################
# HARD GATE FAILURE — ELAPSED TIME REGRESSED BEYOND BUDGET
# Raise --max-elapsed-regression-pct only with an explicit rationale.
############################################################
  elapsed_seconds        483.1 → 750.8  (+55.4%, budget +20.0%)
exit=1
```

## Side finding (PAN-166)

`--coherent-subtract-full-frame-reference-enabled` alone loses 8 truths against production defaults on this tier (277 → 269, all on the weak signal). Per-block cancellation recovers 7 of them. Any default flip of the parent flag must account for this. Chasing it is outside PAN-168's scope.

## Real-signal gate (operator runbook, not run)

`curated-hard-200`, `hard-jt9-rich-200` and `noise_1000` resolve to `~/.pancetta` corpora that cloud containers do not have, so this gate was not run. On the operator host:

```bash
# On the operator host (corpora under ~/.pancetta; run ./scripts/research-env.sh --preflight first).
E=./target/release/eval; O=research/scorecards
for T in noise_1000 hard-jt9-rich-200 curated-hard-200; do
  $E --tier $T --mode ft8 --coherent-subtract-full-frame-reference-enabled --output $O/pan168-$T-ff-whole.json
  $E --tier $T --mode ft8 --coherent-subtract-full-frame-reference-enabled \
     --coherent-subtract-full-frame-time-varying-enabled --output $O/pan168-$T-ff-tv.json
  ./target/release/compare $O/pan168-$T-ff-whole.json $O/pan168-$T-ff-tv.json --bootstrap
done
```

The bar for flipping either flag's default, as in the PAN-7 plan: `compare`'s hard gates pass (no false-positive increase on `noise_1000`, unverified-novel growth ≤ 2×ΔTP), and the bootstrap 95% CI on ΔTP excludes zero in the improving direction. A default flip is a separate decision.
