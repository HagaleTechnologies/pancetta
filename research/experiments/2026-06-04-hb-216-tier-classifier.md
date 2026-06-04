# hb-216 — Hardware-tier classifier (runtime decoder-budget probe → adaptive config presets)

**Date**: 2026-06-04
**Branch**: iter/2026-06-04-tier-classifier
**Status**: **SESSION-1-COMPLETE — probe + classifier shipped; per-tier config plumbing deferred to S2**
**Effort**: ~1.5 hours (probe module + tests + CLI + M4 baseline)

## Motivation

User framing (2026-06-04, in conversation after S3 mechanism diagnostic):

> People may run this code on very minimal hardware, and we'll need to
> know what's possible and potentially (in the future) support more and
> less aggressive decoding based on hardware capability.

Direct follow-on to hb-091:

- S3 mechanism diagnostic measured M4 Mac Mini hard-200 full-decode p99 =
  2332ms (busts 2000ms slot budget) vs scoped p99 = 866ms.
- S3 production wiring shipped a coordinator scoped fast-path gated
  behind `PANCETTA_SCOPED_FAST_PATH=1` env var.
- S3 QSO/hr A/B simulation showed M4 doesn't clear the +10% deployment
  gate (best 1.05× under heavy fade), so default-off is correct on M4.

The open question: **how does the operator know which env var settings
their hardware needs?** The user explicitly flagged this as a load-bearing
concern for minimal-hardware deployments.

This session ships the runtime classifier that answers it.

## Design

Module `pancetta-ft8::tier_probe`. Public API:

```rust
pub enum HardwareTier { Fast, Moderate, Slow }
pub fn classify_tier(p95_ms: u64) -> HardwareTier;
pub fn recommend_actions(tier: HardwareTier) -> Vec<TierRecommendation>;
pub fn summarize_samples(samples: Vec<Duration>) -> TierProbeResult;

#[cfg(feature = "transmit")]
pub fn probe_hardware_tier(n: usize) -> Ft8Result<TierProbeResult>;
```

`probe_hardware_tier`:

1. Encodes a synthetic FT8 message ("CQ K5ARH EM10") via `Ft8Encoder` +
   `Ft8Modulator` at 500 Hz audio offset.
2. Runs one warmup decode (discarded — stabilizes CPU caches + FFT
   planner state).
3. Runs N measured decodes, capturing `Instant::elapsed()` per call.
4. Sorts samples, computes p50/p95/p99/max, classifies by p95, attaches
   per-tier recommendations.

## Tier thresholds (synthetic-baseline)

**Crucial calibration note**: the synthetic FT8 signal decodes via the
easy fast path (pass 1 finds the message, no multipass, no OSD fallback).
So the probe's p95 measures the **per-decode FLOOR** on this hardware,
NOT real-world hard-200 p95 (which is bimodal and dominated by the
multipass+OSD tail).

M4 Mac Mini calibration (2026-06-04):

- Synthetic p95 = 211ms.
- Hard-200 p95 = 2132ms (from `decode_latency_profile.rs` 2026-06-04).
- Multiplier ≈ 10×.

Tier boundaries (synthetic basis):

| Tier | Synthetic p95 | Expected hard-200 p95 | Recommendations |
|---|---|---|---|
| **Fast** | < 400 ms | ~< 3000 ms | None — defaults are fine. |
| **Moderate** | 400–1199 ms | ~4000–8000 ms | Enable `PANCETTA_SCOPED_FAST_PATH=1`. |
| **Slow** | ≥ 1200 ms | ~8000+ ms | Require scoped fast-path; consider lowering `max_decode_passes` / OSD depth. |

Hardware mapping (informed guesses, to be refined as data lands):
- M4 Mac Mini: Fast (measured 213ms).
- Typical Intel N100 / N305 mini PC: Moderate.
- Older or ARMv7 hardware (Pi 4 class): Slow.

## M4 reference baseline (this session)

```
== Pancetta Hardware-Tier Probe ==
  CPU model:     Apple M4
  Logical cores: 10
  Target arch:   aarch64
  OS:            macos
  Iterations:    10

== Results ==
  Wall-clock:    p50=210ms  p95=213ms  p99=213ms  max=213ms
  Tier:          FAST

== Recommendations ==
  No tuning needed — defaults are fine on this hardware.
```

Consistent with hb-091's M4 A/B simulation conclusion: default-off is
correct on M4.

## Tests

`pancetta-ft8::tier_probe::tests`:
1. `classify_tier_below_fast_threshold_is_fast` (boundary 0, 399).
2. `classify_tier_at_moderate_boundary_is_moderate` (boundary 400, 1199).
3. `classify_tier_at_slow_boundary_is_slow` (boundary 1200, 5000).
4. `recommend_actions_fast_is_empty`.
5. `recommend_actions_moderate_suggests_scoped_fast_path`.
6. `recommend_actions_slow_includes_multipass_advice`.
7. `summarize_samples_computes_sorted_percentiles` (with known input).
8. `summarize_samples_classifies_moderate` (constructed range).
9. `probe_hardware_tier_smoke_yields_sane_stats` (N=2, gated on `transmit`).

All 9 pass; broader pancetta-ft8 suite (~237 lib tests + integration)
also green.

## Artifacts

- `pancetta-ft8/src/tier_probe.rs` — module (Fast/Moderate/Slow tier,
  threshold constants, `classify_tier`, `recommend_actions`,
  `summarize_samples`, `probe_hardware_tier` behind `transmit` feature).
- `pancetta-ft8/src/lib.rs` — `pub mod tier_probe;` declaration.
- `pancetta-research/examples/tier_probe.rs` — operator-facing CLI.
- `research/hypothesis_bank.md` — hb-216 entry.
- `research/experiments/2026-06-04-hb-216-tier-classifier.md` —
  this journal.

## Session 2 scope (next batch)

Wire the probe into pancetta's startup:

1. On coordinator startup, run `probe_hardware_tier(10)` (synthronously,
   ~2-10s on Fast tier, ~30s on Slow).
2. Log the classification + recommendations at INFO level.
3. **For Moderate/Slow tiers**, set
   `PANCETTA_SCOPED_FAST_PATH=1` programmatically (env var or a
   coordinator-level flag) so the fast-path activates without operator
   intervention.
4. **Per-tier `Ft8Config` preset** selection for Slow tier:
   `max_decode_passes = 1` (skip multipass), `osd_depth = Some(1)`
   (cheaper OSD fallback).
5. **On-disk classification cache** keyed by CPU model + core count
   hash, so the probe runs once per machine. Invalidate on hash mismatch.
6. **Operator override** via `pancetta-config` if they want to force a
   specific tier.

## Lessons / notes

1. **Probe ≠ hard-200 p95 predictor.** The synthetic signal hits the
   easy fast path on any decoder; the probe measures the baseline floor,
   not the multipass-tail-heavy real-world distribution. Honest framing:
   probe is a relative classifier (slower hardware → slower baseline →
   slower worst-case), not a wall-clock predictor for production.

2. **Cross-hardware data is now the bottleneck.** The Fast/Moderate/Slow
   thresholds are M4-anchored guesses. Refining them requires probing
   on diverse hardware (Windows 11 MiniPC, ARMv7 Pi 4, etc.). Operator's
   MiniPC is the next target.

3. **Synthetic-only probing has known caveats.** A future improvement:
   add an option to probe on a small embedded corpus (e.g., 5 hard-200
   WAVs ship inside `pancetta-ft8` for probe-only use). Would give a
   more representative p95 at the cost of binary size.

4. **The hb-091 + hb-216 pair is the right shape for hardware-adaptive
   pancetta**: hb-091 shipped the *capability* (scoped fast-path,
   env-var gate), hb-216 ships the *decision logic* (when to flip).
   Session 2 of hb-216 closes the loop by wiring the probe into
   startup so the operator doesn't need to manually set env vars.
