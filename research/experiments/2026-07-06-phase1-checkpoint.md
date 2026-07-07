# Phase 1 checkpoint — decoder-speed-overhaul (Tasks 1-7)

**Date**: 2026-07-06
**Branch**: worktree-decoder-speed-overhaul (subagent-driven-development), base main @ eaf2beab
**Status**: Phase 1 (mechanical fixes) complete — 7 tasks landed, all task-reviewed clean.

## What shipped

| Task | Fix | Gating | Landed default |
|------|-----|--------|-----------------|
| 1 | Profiling harness (F7) | docs | n/a |
| 2 | Lazy BP trajectory + array returns (F2) | BIT-EXACT | always on (pure refactor) |
| 3 | OSD sort key precompute + alloc trims (F6) | BIT-EXACT | always on (pure refactor) |
| 4 | Flatten spectrogram to contiguous storage (F3) | BIT-EXACT | always on (pure refactor) |
| 5 | Padé `fast_atanh` (F1) | A/B | `pade_atanh = true` (piecewise: Padé ≤0.95, ln above — raw Padé alone regressed the hard-200 gate) |
| 6 | `costas_half_loop_disabled` flip attempt (F5) | A/B | **stays `false`** — re-confirmed Batch 92's finding (flipping costs real recall); plan text's premise that Batch 92 supported the flip was factually wrong, corrected here |
| 7 | f32 real-input FFT + f32 spectrogram (F4) | A/B | `SpecScalar = f32` with `realfft::RealFftPlanner`, coherent-subtract path kept f64 arithmetic over f32 storage |

## Production-config benchmark (this checkpoint)

Methodology: built the pre-Phase-1 baseline (main @ eaf2beab) and the current Phase-1-complete HEAD as two
separate binaries, ran both back-to-back under identical (and, at measurement time, fairly loaded — shared
dev machine, load avg ~10) conditions to cancel ambient noise via direct A/B rather than trusting either
absolute number alone. `cargo run --release -p pancetta-ft8 --example profile_decode -- native 10` on the
harness's single fixture (`tests/fixtures/wav/wsjt/210703_133430.wav`, 8-message window).

| Config | Baseline (eaf2beab) | Phase 1 complete (HEAD) | Δ |
|--------|---------------------|--------------------------|---|
| Multi-thread | 144.01 ms/window | 107.18 ms/window | **-25.6%** |
| Single-thread (`RAYON_NUM_THREADS=1`) | 246.43 ms/window | 189.63 ms/window | **-23.1%** |

Both runs showed tight iteration-to-iteration variance (±1-2ms) once measured back-to-back at the same
moment, unlike earlier noisy single-run attempts on this shared machine — the ~23-26% figure is the
trustworthy read for this specific fixture.

## Why this is less than the plan's "~60-100ms vs 240ms" (75-60% reduction) estimate

That estimate anticipated the *stacked* effect of all optimizations under a workload that stresses BP/OSD/
FFT more heavily than this harness's single 8-message window does. Per-task evidence already on record:

- Task 2 (BP lazy trajectory): noise-level on this fixture — most of its BP calls already converge quickly,
  so the eliminated 17.4KB trajectory alloc rarely fires. The win is real but proportional to
  non-converging-BP-call volume, which this fixture doesn't stress.
- Task 5 (Padé atanh): -16.0% *elapsed* on the hard-200 research corpus (many more candidates/BP calls per
  slot than this one fixture), vs. a much smaller effect on this single window.
- Task 7 (f32 FFT): -3.8% elapsed on hard-200 vs. -3.6%/-1.0% on this fixture.

So the ~23-26% measured here is consistent with (and a lower bound relative to) the corpus-level wins already
gated — this single fixture just isn't the worst case Phase 1 targets. The plan's Phase 2 (anytime decoder:
floor/escalation ladder, budget checkpoints) is expected to deliver the larger remaining win by skipping work
on low-value candidates entirely, rather than only making each unit of work cheaper.

## Task 6 correction

Task 6's brief text asserted "Batch 92 already argued redundancy at TIME_OSR ≥ 2: the A/B confirms on
hard-200," implying the flip should succeed. Batch 92's actual documented verdict
(`research/experiments/2026-06-12-batch-92.md`) is the opposite: flipping `costas_half_loop_disabled` to
`true` costs real recall (-64 TPs / 200 slots) and was explicitly marked do-not-ship. Task 6 independently
re-verified this on hard-200 (Δrec=-58, CI[-80,-39]) and correctly declined to flip, per the standing
"A/B gate fails → do not flip the default" rule. No code was changed; the plan document itself has a factual
error here that should be corrected for future readers (flagged, not fixed, since editing the plan file is
outside any task's scope).

## Full-suite gate

`cargo test --workspace --features transmit` green throughout every task; no regressions introduced by
Phase 1. Decode counts on every WAV fixture unchanged from pre-Phase-1 baseline (verified per-task).

## Next

Phase 2 (Tasks 8-12): `DecodeBudget` type, floor/escalation candidate stages, budget checkpoints, coordinator
deadline wiring — the anytime-decoder work expected to deliver the larger win by skipping low-value work
under budget pressure rather than only speeding up each unit of work.
