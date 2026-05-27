---
slug: hb-018-osd3-reconfirm
mode: ft8
state: shelved
created: 2026-05-26T19:00:00Z
last_updated: 2026-05-26T19:00:00Z
branch: iter/2026-05-26-batch-12
parent_hypothesis: hb-018
wild_card: false
scorecard: research/scorecards/hb018-osd3.json (transient, removed)
delta_vs_main: -1 rec / +10 novel — unchanged from hb-053's earlier finding
disposition: SHELVE hb-018 (re-confirmed). OSD-3 direction structurally closed under new production.
---

## Why re-eval

hb-053 (batch 7) already shelved OSD-3 + filter (-1 rec / +x novel).
The question: does the hb-056 → hb-075 cross-cycle pipeline change
the candidate population reaching OSD enough to flip the verdict?
Both layered BP and cross-cycle averaging alter what BP outputs and
which candidates fall to the OSD fallback, so a fresh check was
warranted.

## Result

hard-200 with FP filter:

| config       | recovered | novel | rate    |
|--------------|----------:|------:|--------:|
| OSD-2 (prod) |      4430 |   845 | 0.51656 |
| OSD-3        |      4429 |   855 | 0.51644 |

**Δ rec −1, Δ novel +10.** Identical signature to hb-053's earlier
finding: OSD-3 surfaces ~10 CRC-14-collision novels while losing 1
real decode at the threshold. The cross-cycle averaging didn't change
this signature — candidates that arrive at OSD-3 are still in the
same precision/recall regime.

## Decision

**SHELVE definitively.** OSD-3 direction structurally closed. Same
direction as hb-053; the new pipeline doesn't unlock it.

## Learnings

- The OSD depth ladder is now closed from every direction we can
  test: gate=0 (hb-041 breaks fixtures), gate=2 (production sweet
  spot per hb-014), gate=6 (graduated by hb-053 with FP filter),
  OSD-2 (production), OSD-3 (this + hb-053 — both confirm net-
  negative).
- Future OSD-speed work is hb-064 (TEP pruning via DIA neural OSD).
- No new spawns.
