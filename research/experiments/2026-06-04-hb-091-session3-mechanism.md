# hb-091 — a8-style early-decode latency reduction (Session 3 mechanism check)

**Date**: 2026-06-04
**Branch**: iter/2026-06-03-hb-091-s3ab
**Status**: **SESSION-3-MECHANISM-CONFIRMED — PROCEED to Session 3 production wiring**
**Effort**: ~1.5 hours (diagnostic build + smoke + full 9-min run + analysis)

## Why this session existed

Session 2 settled recall feasibility (HW=±5 PROCEED at 96.03% retention).
Session 3 was scoped as "DSP partial-buffer emit + coordinator scoped-decode
dispatch + loopback QSO/hr A/B" — but reading the DSP code revealed a
load-bearing architectural fact that the original journal missed:

**Pancetta's DSP already fires decode at t=13s into the slot**
(`pancetta/src/coordinator/dsp.rs` line 217-220, 363-365). The 15s window
delivered at t=13 covers slot N-1's last 2s + slot N's first 13s — the
WSJT-X reference buffer layout, with the full 12.64s FT8 message
contained.

The WSJT-X-Improved a8 framing ("display partner messages 0.5-1s earlier")
is about WSJT-X conventionally firing at slot-END (t=15) and a8 moving
that to t=13-14. **Pancetta is already at t=13.** There is no "fire 1s
earlier" lever left to pull at the buffer-delivery layer.

What CAN still apply is the **wall-clock benefit of scoped decode**
itself: scoping the Costas sweep to ±31 Hz (11 of ~640 bins) reduces
per-call compute by ~58×. If the full-decode wall-clock tail
approaches the slot deadline (t=15, ~2s after DSP fire), scoped's
faster wall-clock prevents late-firing on the next-slot TX.

This session built a generic latency-profile diagnostic to answer the
load-bearing question:

> Does scoped decode reliably save wall-clock at p95/p99 where late-firing
> risk lives?

## Method

`pancetta-research/examples/decode_latency_profile.rs`. For each WAV in
`hard_200.manifest.json` (200 real off-air recordings):

1. Load full 15s samples; truncate/pad to exactly 180,000 samples.
2. WARMUP: decode once without timing (stabilizes CPU caches).
3. MEASURE **full**: time `decode_window(buf)`.
4. MEASURE **scoped** (when jt9 baseline exists, n=197): pick the FIRST
   jt9 truth's `freq_hz`, derive `freq_bin = round(freq_hz / 6.25)`, time
   `decode_window_scoped(buf, (bin - 5)..=(bin + 5))`.

Report: p50, p90, p95, p99, max wall-clock for each arm; Δ at each
percentile; 10-bucket histogram aligned to the full arm's range.

Hardware context (self-reported): CPU model, logical core count,
target arch, OS, build profile. This is the M4 Mac Mini reference
baseline; cross-machine comparison data slots in here.

## Results — M4 Mac Mini, 10 cores

Full hard-200 run, 532.9s wall (3 WAVs skipped scoped — no jt9 baseline).

```
                 full         scoped       Δ           ratio
mean:           1136 ms       369 ms      -767 ms      0.32×
p50:             862 ms       329 ms      -533 ms      0.38×
p90:            1980 ms       605 ms     -1375 ms      0.31×
p95:            2132 ms       712 ms     -1420 ms      0.33×
p99:            2332 ms       866 ms     -1466 ms      0.37×
max:            2446 ms       917 ms     -1529 ms      0.37×
```

Full-decode distribution is **bimodal**: a peak around 500-700ms (easy
WAVs, few candidates, fast LDPC) and a long tail at 1400-2400ms (hard
WAVs hitting multipass + OSD fallback). Scoped collapses to a single
mode: 160/197 WAVs ≤ 513ms (81%).

```
full           histogram (range 298..2446 ms):
  [ 298 ..  513]   3  █
  [ 513 ..  728]  78  ████████████████████████████████████████
  [ 728 ..  943]  30  ███████████████
  [ 943 .. 1158]   7  ███
  [1158 .. 1372]   7  ███
  [1372 .. 1587]  19  █████████
  [1587 .. 1802]  21  ██████████
  [1802 .. 2017]  17  ████████
  [2017 .. 2231]  13  ██████
  [2231 .. 2446]   5  ██

scoped         histogram (same range):
  [ 298 ..  513] 160  ████████████████████████████████████████
  [ 513 ..  728]  28  ███████
  [ 728 ..  943]   9  ██
  [ 943 .. 2446]   0
```

## Operational interpretation: late-firing risk

DSP fires decode at t=13.0s of slot N. The decoder runs and completes at
t = 13 + wall_clock. The next-slot TX boundary is t=15.0, so the slack
is `2.0 - wall_clock` seconds.

| arm @ percentile | wall-clock | completes at slot-time | slack to t=15 |
|---|---:|---:|---:|
| full p50 | 862 ms | 13.86 | +1140 ms |
| full p90 | 1980 ms | 14.98 | **+20 ms** |
| full p95 | 2132 ms | 15.13 | **−130 ms LATE** |
| full p99 | 2332 ms | 15.33 | **−330 ms LATE** |
| scoped p50 | 329 ms | 13.33 | +1670 ms |
| scoped p90 | 605 ms | 13.60 | +1400 ms |
| scoped p95 | 712 ms | 13.71 | +1290 ms |
| scoped p99 | 866 ms | 13.87 | +1130 ms |

**Full decode at p90+ already busts the slot deadline on M4.** TX
scheduler can recover via `tx_late_max_ms=8s` skip-ahead-cursoring,
but cursored audio truncates leading symbols — degraded QSO success on
the receiving end.

Scoped at p99 (866ms) completes at t=13.87 with 1130ms of slack — well
inside the slot budget. **Scoped reliably keeps decode wall-clock under
the slot deadline; full does not.**

## Decision: PROCEED to Session 3 production wiring

The hb-091 mechanism for pancetta is settled, but it's NOT what the
journal framed as "fire 1s earlier." It's:

> Scoped decode reliably completes within slot budget; full decode
> intermittently busts it.

Production wiring (Session 3b) should:

1. When `activeQso` is set, run a **scoped fast-path FIRST** on the
   delivered 15s buffer, anchored at the partner's last `frequency_offset`
   ± 5 bins. Advance the QSO state machine on hit.
2. Run the standard ft8_lib + native full decode SECOND (existing path).
   Treat its output as the authoritative ground truth; the scoped
   fast-path is opportunistic.
3. Gate behind a config flag (`coordinator.scoped_fast_path`, default
   off) until the loopback A/B confirms QSO/hr benefit.

Session 3c (loopback A/B with slot-timing model + variable fade) is the
operational deployment gate. This diagnostic only confirms the
mechanism; the QSO/hr A/B confirms the ship.

## Broader value: hardware-tier baseline

Beyond hb-091, the diagnostic establishes a reproducible wall-clock
baseline on the M4 Mac Mini reference machine. Future hardware-tier
work — e.g., adaptive multipass/OSD depth on lower-tier MiniPCs — can
compare against this baseline to set tier-appropriate decode budgets.

Suggested follow-ups (separate batches):
- Run the diagnostic on the operator's Windows 11 MiniPC (Phase 5 target
  hardware) to characterize that tier's wall-clock distribution.
- Build a "tier classifier" that runs the diagnostic at startup and
  selects decoder config presets (max_passes, OSD depth, scoped fast-path
  enable threshold) based on measured p95 vs slot deadline.
- Extend the diagnostic to vary decoder config (max_passes, OSD depth,
  multipass on/off) and plot a Pareto frontier of (recall, p95 wall-clock).

## Artifacts

- `pancetta-research/examples/decode_latency_profile.rs` — the
  diagnostic, with `--scoped`, `--half-width`, `--max-wavs` CLI flags;
  reports hardware context.
- `research/experiments/2026-06-04-hb-091-session3-mechanism.md` —
  this journal.
- (To follow) `research/hypothesis_bank.md` hb-091 status update.

## Lessons

1. **Always read the production code before extrapolating from a
   diagnostic.** Session 1's "1s earlier = retention 97.73%"
   measurement was correct as a feasibility result for the
   truncation-cost question. But the production-mechanism inference
   ("we save 1s of latency") was based on an unverified assumption
   that pancetta fires at slot-end. Pancetta already fires at t=13.
   Reading `dsp.rs` first would have re-framed Session 3 immediately.

2. **Bimodal distributions hide the load-bearing question in the
   mean.** Full mean is 1136 ms — looks fine if the slot budget is
   2000 ms. But the p90 (1980 ms) and p99 (2332 ms) reveal the real
   risk: 10-15% of decodes already bust the deadline. Always check
   percentile structure when a deadline is involved.

3. **A small generic measurement tool pays for itself.** The
   diagnostic is ~300 LOC and runs in ~9 minutes. It answered Session
   3's load-bearing question definitively AND laid the groundwork for
   hardware-tier work. Generic measurement infrastructure outperforms
   bespoke per-hypothesis diagnostics over time.
