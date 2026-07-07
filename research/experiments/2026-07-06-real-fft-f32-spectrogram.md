# f32 real-input FFT spectrogram (F4) — decoder-speed-overhaul Task 7: gate PASSED

**Date**: 2026-07-06
**Branch**: `worktree-decoder-speed-overhaul` (control commit d109ebd8, variant = working tree at
measurement time, later committed)
**Status**: GATE PASSED. `Spectrogram` storage flipped from `f64` to `f32`
(`SpecScalar` alias) and the spectrogram FFT flipped from a full complex
`rustfft::Fft<f64>` to a real-input `realfft::RealToComplex<f32>` transform.

## What this was

Decoder-speed-overhaul plan Task 7 (F4, `[A/B]`): the spectrogram
(`Ft8Decoder::compute_spectrogram_with`) FFTs real audio samples through a
full complex FFT, computing and then discarding the upper (mirror) half of
the spectrum via the `src_bin < nfft/2+1` bin check that was already in the
code. A real-to-complex FFT (`realfft` crate) never computes that
redundant half in the first place — roughly half the FFT work for the same
result — and switching the spectrogram's scalar storage from `f64` to
`f32` halves its memory footprint (helps cache locality in the Costas
kernel that walks it) and lets `log10` run at native `f32` precision
instead of `f64`.

This is a genuine numerics change (reduced storage/compute precision), so
per the plan it's gated `[A/B]` — validated on the research-harness
hard-200 corpus before being considered final, same discipline as Task 5
(`2026-07-06-pade-atanh.md`) and Task 6
(`2026-07-06-costas-half-loop.md`).

## Design: what actually changed to f32, and what stayed f64

Two-step structure per the plan:

- **Step 1** (`d109ebd8`, pure refactor): introduced `type SpecScalar = f64;`
  and converted `Spectrogram::{power, complex}` + `at()`/`row()` to the
  alias. Zero behavior change (alias == f64); full `pancetta-ft8` suite
  (`--features transmit`) passed with identical counts before/after.
- **Step 2** (this experiment): flipped `SpecScalar` to `f32` and replaced
  the spectrogram's FFT plan with `realfft::RealFftPlanner::<f32>`.

Unlike the plan's fallback contingency ("if the A/B regresses, the complex
subtract path is the likely suspect and may need to stay f64
independently"), this task did NOT wait for a gate failure to make that
call — `Ft8Config::cross_cycle_coherent` (which populates
`Spectrogram.complex` and drives the iterative coherent-subtract/re-pass
pipeline) is `true` **by default**, so the complex-domain math is on the
hot, default-on path, not a rare opt-in. Given that, the design applied
the precision-conservative split up front rather than reactively:

- `Spectrogram.power`/`Spectrogram.complex` storage: `SpecScalar` (f32).
- The **primary FFT + `log10` computation** in `compute_spectrogram_with`
  (the actual site the real-FFT crate feeds) runs natively at f32
  precision — this is the mechanism's real numeric change, not a boundary
  cast.
- **Every other consumer** (Costas `block_score`/`compute_costas_score_groups`
  accumulators, `lookup_time_interp`, `noise_floor_db_median`,
  `mean_excess_above_noise_db`, `average_spectrum_per_bin`, and — the
  precision-sensitive one — `subtract_decode_coherent` /
  `known_coherence_score` / `par_extract_complex_symbols_from_spectrogram`,
  i.e. all the coherent-subtract/cross-cycle-coherent complex-domain math)
  **upcasts to `f64` immediately on read and downcasts on write**. The
  iterative ML-projection subtraction (`subtract_decode_coherent`, which
  runs `coherent_multipass_iterations` times per decode, reading and
  rewriting the same complex buffer each round) does its
  conjugate-multiply/subtract arithmetic entirely in `f64`; only the
  storage between rounds is `f32`. This bounds the precision cost to one
  quantization step per round rather than compounding f32 rounding error
  *inside* the arithmetic across rounds.

This is a stricter (safer) design than the plan's minimum bar, chosen
because the complex-subtract path is both (a) the plan's own named risk
and (b) on the default-hot path — not a stretch goal, a risk-avoidance
default.

## Real-FFT bin equivalence (not just a precision change)

`compute_spectrogram_with`'s freq-oversampling loop already only consumed
`src_bin < nfft/2+1` — the unique half of a real signal's spectrum — and
treated everything past that as the `-120 dB` sentinel. `realfft`'s
`RealToComplex::process` returns exactly `nfft/2+1` complex bins and never
computes the mirrored half at all, so the bin-selection logic is
unchanged bit-for-bit (same `if src_bin < spectrum.len()` guard, same
`spectrum.len() == nfft/2+1` value as before); only the FFT algorithm
computing those bins changed.

## `realfft` vs `FftPlanner::<f32>` decision

Went with `realfft::RealFftPlanner<f32>` (added `realfft = "3.5"` to
`pancetta-ft8/Cargo.toml`, built on the same `rustfft` 6.x already a
dependency) rather than just swapping `FftPlanner::<f64>` for
`FftPlanner::<f32>` and keeping a full complex transform. Audio is real —
the code was already discarding the mirrored upper half of the complex
spectrum — so a real-to-complex transform is a direct, structural fit:
it eliminates computing a redundant half of the FFT outright (not just a
narrower scalar type for the same amount of work), and it slightly
simplifies the hot loop (`real_input: Vec<f32>` windowed samples in,
`spectrum: Vec<Complex<f32>>` of length `nfft/2+1` out — no more manual
`Complex::new(windowed_sample, 0.0)` construction into a full-length
complex buffer).

## Commands run (real CLI, `--help`-verified before use)

```bash
# Control: Step-1 commit (d109ebd8, SpecScalar = f64, complex rustfft FFT)
git stash push -- Cargo.lock pancetta-ft8/Cargo.toml pancetta-ft8/src/decoder.rs
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/f4-realfft-control.json
git stash pop

# Variant: SpecScalar = f32, realfft real-to-complex plan
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/f4-realfft-variant.json

cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/f4-realfft-control.json research/scorecards/f4-realfft-variant.json
```

No new eval CLI flag was added — F4 is not config-gated (there is no
`Ft8Config` bool for it; the scalar type + FFT plan are compile-time), so
control/variant are two different git states of the same binary rather
than one binary with a flag, unlike Task 5/6.

## Results

| | control (f64, complex FFT) | variant (f32, real FFT) |
|---|---:|---:|
| composite (raw) | 0.3138 | 0.3118 |
| composite (saturation-aware) | — | — (not separately reported by this compare run; raw shown) |
| decode_rate (truth_decodes_recovered/total) | 0.6277 (1256/2001) | 0.6237 (1248/2001) |
| novel_decodes (false positives) | 4989 | 4954 |
| harness elapsed_seconds | 51.96 | 49.96 (**-3.8%**) |

Bootstrap CI (n=1000, seed 0xb007):
- `rec` (recall) Δ=**-8**, 95% CI **[-22, +3]** — **NOT significant** (CI includes zero → recall
  within CI of baseline)
- `novel` (false positives) Δ=-35, 95% CI [-51, -21] — significant improvement (fewer FPs)

Gate criteria (plan Step 4):
1. **Recall within CI of baseline** — PASS (Δ=-8, CI spans zero, not a significant regression).
2. **ΔFP ≤ 2×ΔTP** — PASS. ΔTP=-8, 2×ΔTP=-16; ΔFP=-35 ≤ -16 holds trivially since FPs *also*
   decreased (more than proportionally) rather than increasing — there is no accuracy/FP
   tradeoff to bound here, unlike Task 5's raw-Padé attempt.
3. **Elapsed improved** — PASS, -3.8% wall on the hard-200 harness's full pipeline (modest; this
   harness runs `max_decode_passes=1, coherent_multipass_iterations=3, ldpc_iterations=100,
   osd_depth=0`, so LDPC/BP dominates total wall time far more than the spectrogram build — F4
   only touches the latter).

All three criteria pass — **gate PASSED**, no fallback to a separate f64-for-complex alias was
needed (the design already isolated that risk up front; see "Design" above).

## Standalone profile_decode benchmark (release, `WINDOW_SAMPLES`-sized single-window decode)

`cargo run --release -p pancetta-ft8 --example profile_decode --features transmit -- native 10`,
same control/variant git states as above (uncommitted `profile_decode.rs` harness from a prior
task, `pancetta-ft8/examples/profile_decode.rs`):

| | control (f64) | variant (f32 real-FFT) |
|---|---:|---:|
| multi-thread (default rayon) | 114.0 ms/window | 109.9 ms/window (**-3.6%**) |
| `RAYON_NUM_THREADS=1` | 214.5 ms/window | 212.3 ms/window (-1.0%, within run-to-run noise) |

Both runs decode the same 8 messages/iteration on the fixture window (no recall change on this
single file, consistent with the fixture-sweep check below). The single-thread number is
dominated by other stages at this production-ish config (this profiling harness's default config
is closer to the hard-200 eval's multi-pass/high-iteration settings than a bare spectrogram
microbenchmark), so F4 alone yields a modest, not dramatic, wall-time win in isolation — consistent
with the hard-200 harness result above. The larger combined win the plan's Phase-1 checkpoint
targets (~60-100ms vs 240ms single-thread) depends on the *other* Phase-1 tasks (candidate-count
and decode-pass reduction) stacking with this one; that combined re-measurement is a Phase-1
checkpoint activity, not scoped to this individual task.

## Fixture decode-count check (Step 3, all WAV fixtures — not just profile_decode's one file)

`cargo test -p pancetta-ft8 --features transmit --test wav_decode_tests -- --nocapture
--test-threads=1`, same control/variant git states, single-threaded for deterministic ordering:

| fixture | control decodes | variant decodes |
|---|---:|---:|
| generated/ft8_cq.wav | 1 | 1 |
| generated/ft8_report.wav | 1 | 1 |
| generated/ft8_rr73.wav | 1 | 1 |
| jtdx/000000_000001.wav | 0 | 0 |
| jtdx/190227_155815.wav | 25 | 25 |
| wsjt/210703_133430.wav | 9 | 9 |
| wsjt/181201_180245.wav | 8 | 8 |
| wsjt/170709_135615.wav | 0 | 0 |
| basicft8/170923_082000.wav | 0 | 0 |
| basicft8/170923_082015.wav | 0 | 0 |
| basicft8/170923_082030.wav | 0 | 0 |
| basicft8/170923_082045.wav | 0 | 0 |
| cross-validation totals (ours/ft8_lib) | 42/38 | 42/38 |

Byte-identical decode counts on every fixture, both directions of comparison (native-vs-truth and
native-vs-ft8_lib). `f32`'s ~7 significant digits is far more precision than the ~0.1 dB comparison
granularity these decode decisions turn on, as the plan anticipated — confirmed rather than
assumed.

## Decision

**Ship.** `Spectrogram::{power, complex}` are `f32` (`SpecScalar` alias), the spectrogram FFT is
`realfft::RealToComplex<f32>`. Full `pancetta-ft8` test suite (`--features transmit`, all feature
combinations including `benchmark`/`ft2`) green throughout, identical pass/fail counts to the
Step-1 baseline (432/7/10/11/7/11/30/5/8/25/10/15/1 across the lib + integration test binaries).
`cargo fmt` + `cargo clippy --all-targets` clean.

## Counters

- Ships: `Spectrogram` storage `f64`→`f32` (`SpecScalar` alias), spectrogram FFT
  `rustfft::Fft<f64>`→`realfft::RealToComplex<f32>` (`pancetta-ft8/src/decoder.rs`,
  `pancetta-ft8/Cargo.toml` adds `realfft = "3.5"`).
- No config flag added or changed — this is not behind an `Ft8Config` toggle (compile-time
  scalar type + FFT plan).
- Scorecards: `research/scorecards/f4-realfft-{control,variant}.json` (untracked, same convention
  as the Task 5/6 scorecards).
- Deferred (not this task): the Phase-1 checkpoint's full ablation-sweep re-measurement combining
  F1+F3+F4(+F5-not-shipped) at production config — belongs to whichever task closes out Phase 1,
  not to this individual F4 task.
