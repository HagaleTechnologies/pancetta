---
slug: hb-067-mbp-offset-finalize
mode: ft8
state: shelved
created: 2026-05-26T18:30:00Z
last_updated: 2026-05-26T18:30:00Z
branch: iter/2026-05-26-batch-12
parent_hypothesis: hb-067
wild_card: false
scorecard: research/scorecards/mbp-{0.0,1.0,2.0}.json (transient, removed)
delta_vs_main: hard-200: offset=2.0 gives +1 rec / -11 novel vs offset=0.0 — within noise
disposition: SHELVE hb-067 definitively. Default stays 0.0 (library + CLI in place for future use). The mBP-offset lever's edge shrunk under new production (FP filter + cross-cycle MRC catch most of what it used to filter).
---

## Hypothesis

hb-067 was a SOFT WIN at batch 8 (precision-only, mechanism mismatch
with arXiv:2306.00443). The pre-graduation deferral was contingent on
re-testing under future production state. Re-tested today under the
post-hb-075 baseline.

## Result

hard-200 sweep at `bp_offset_subtract ∈ {0.0, 1.0, 2.0}` (with FP filter):

| offset | recovered | novel | rate    |
|--------|----------:|------:|--------:|
| 0.0    |      4430 |   845 | 0.51656 |
| 1.0    | 4431 (+1) | 837 (-8) | 0.51667 |
| 2.0    | 4431 (+1) | 834 (-11) | 0.51667 |

## Why shelve now

- **Precision benefit shrunk.** Batch 8 found −32 novels at offset=2.0
  under the older production state; today it's −11. The intervening
  graduations (hb-052/062 FP filter, hb-058 contest-type rejection,
  hb-056 non-coherent cross-cycle, hb-075 MRC coherent) catch most of
  what mBP offset was reducing. The marginal precision is now small.
- **+1 recovered is run-to-run noise.** A single decode at the LDPC
  threshold; not reproducible-evidence of a real recall gain.
- **Mechanism mismatch persists.** Per batch 8's analysis: the offset
  interacts with the parity gate (which uses offset-adjusted LLRs),
  effectively tightening the gate — *not* the "more flip patterns"
  lever the paper described. We'd be graduating a knob whose actual
  function we can't cleanly defend.
- **Cognitive cost of another graduated knob.** Each additional config
  field with a non-zero default needs explanation in onboarding, README,
  etc. For −11 novels at noise-level recall, not worth the surface.

## Decision

**SHELVE definitively.** `bp_offset_subtract` stays at default 0.0
(library and CLI flag remain in place so future sweeps can revisit if
the production state changes again). No code change.

## Learnings / follow-ups

- **Pattern recognised:** post-hb-075, the FP filter + cross-cycle
  pipeline have absorbed most of what individual precision knobs used
  to add. Future precision-only sweeps should expect diminishing
  returns. Recall-focused or structural work is the higher-leverage
  surface now.
- No new spawns.
