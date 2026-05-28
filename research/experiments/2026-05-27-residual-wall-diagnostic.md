---
slug: residual-wall-diagnostic
mode: ft8
state: completed
created: 2026-05-27T17:30:00Z
last_updated: 2026-05-27T17:30:00Z
branch: iter/2026-05-27-batch-13
parent_hypothesis: batch-13 iter 6 (diagnostic only)
wild_card: false
scorecard: n/a (diagnostic; data from main.json + cross_validate_novels)
delta_vs_main: n/a
disposition: COMPLETED — characterises the post-hb-079 wall and quantifies the joint-decoding opportunity. Feeds the hb-086 spec.
---

## Goal

After hb-079's coherent multi-pass graduated and the follow-up tuning
saturated (hb-080 graduated +N=3, hb-081/082/085 shelved), characterise
the remaining recall wall on hard-* to inform the next structural lever
(joint multi-candidate decoding, hb-086).

## Findings

### Tier-level miss share

| tier | truth | rec | missed | miss% |
|---|---:|---:|---:|---:|
| hard-200 | 8576 | 4588 | **3988** | 46.5% |
| hard-1000 | 28104 | 14916 | **13188** | 46.9% |

~46.5% of jt9-truth signals are missed even after hb-079 + cross-cycle
+ multi-pass. That's the gross wall; the structure below tells us what
to do.

### Worst-WAV concentration (hard-200)

| metric | top-20 WAVs |
|---|---:|
| total truth | 1214 |
| recovered | 518 |
| **missed** | **696** |
| per-WAV miss rate | 50-62% |
| share of all hard-200 misses | **17%** |
| WSJT-X (jt9) recovers? | 100% |

These 20 WAVs each carry 50-70 truth decodes (densest end of the
busy-band distribution). Pancetta gets ~40-50% of each. Jt9 gets all.
**Pattern: very dense interference is the binding constraint.**

The remaining 83% of misses (3292 of 3988) is distributed across the
other 180 WAVs at ~18 per WAV average — broader-but-shallower wall.

### What's NOT the constraint

We've definitively closed (across batch 11 + batch 13):
- LDPC iteration count, OSD depth, NMS, min_sync_score (parameter sweeps).
- Multi-pass iteration count (hb-080 saturates at N=3).
- Subtract scaling (hb-081 full subtract is optimal; under-subtract
  regresses by orders of magnitude more than it gains).
- Residual sync threshold (hb-082 not binding).
- Cross-cycle on residual (hb-085 structurally redundant).

### What IS the binding constraint (hypothesis)

**Mutually masking signal pairs and 3+ way clusters.** hb-079's
coherent subtract works one signal at a time:
1. Decode the strongest decodable signal.
2. Subtract it.
3. Find the next strongest, decode, subtract.
4. Repeat (up to N=3).

This pipeline cannot recover **pairs that interfere bidirectionally
where neither decodes first** — neither has a coherent rotor estimate
clean enough to drive subtraction; both stay masked. On dense WAVs (the
top-20), most pairs/clusters are like this.

### Cross-validation note

`cross_validate_novels` was launched (iter 5) but timed out at 21+
minutes of CPU before the in-batch turn budget ran out — its
sequential-decode loop under hb-079 multipass is too slow to complete
in a single turn. Killed and deferred. The qualitative finding stands
regardless: hb-080's N=3 sweep showed ZERO additional novels across
all iteration counts (1→2 +7 rec/0 novel; 2→3 +9 rec/0 novel), so the
hb-079 → hb-080 gain is recall-only on the *filtered* novel count.
Whether the *unfiltered* novels also pass cross-validation is the
deferred question.

Follow-up: parallelise `cross_validate_novels` with rayon (mirror the
eval's `par_iter` pattern) or partition the corpus, OR run it as a
proper background batch job.

## Recommendation

**hb-086 joint multi-candidate pair decoding** is the natural next
structural lever. Targets the measured top-20 WAVs (17% of misses,
60% per-WAV miss rate) directly. Design spec at
`docs/superpowers/specs/2026-05-27-joint-decoding-design.md`.

**Kill-switch / risk-bound:** spec specifies a diagnostic-first step
that quantifies how many of the top-20 misses have a "nearby recovered
decode" pair structure. If <30% of misses match the pair pattern, the
mechanism doesn't fit and we pivot without implementing.

## Why this matters operationally

The session's cumulative composite gain (+0.012360, 8 graduations)
already represents ~5-6 percentage points of absolute hard-1000
decode-rate improvement (0.476 → 0.531). Joint decoding plausibly adds
another ~0.5-2 percentage points by attacking the dense-interference
wall — but the marginal value depends on operator priorities:

- For **DX hunting** (pancetta's main use case): the missed
  decodes on dense WAVs are usually the *less rare* signals (busy bands
  have dense common stations). Marginal operational value.
- For **contest participation**: dense busy bands ARE the operating
  environment; joint decoding is high-value.
- For **rare-DX listening (single weak signal under QRM)**: hb-079's
  current single-station subtract already handles this. Joint
  decoding doesn't add.

The operator should weigh whether to invest 3-5 sessions in hb-086
or accept the current state as the diminishing-returns frontier and
shift effort to Phase-5 on-air validation. Both are defensible.
