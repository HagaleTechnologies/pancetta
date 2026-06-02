---
slug: hb-173-chrono-retest-note
mode: ft8
state: defer-pending-uncontended-eval
created: 2026-06-02T22:00:00Z
branch: iter/2026-06-02-hb-173-chrono (NOT merged to main)
parent_hypothesis: hb-173 within-QSO Session 2 (deferred pending chronological-replay)
disposition: DEFER — implementation correct + mechanism PROVEN firing, but eval blocked by CPU contention
---

## What landed on the iter branch

Three commits on `iter/2026-06-02-hb-173-chrono` (in worktree `/tmp/pancetta-hb173-chrono`):
- `51d28cb` — cherry-pick of hb-173 Session 2 impl (b958603) with additive conflict resolution (Ft8Config now carries a7_*, dt_history_*, AND within_qso_*)
- `749e48e` — wire `within_qso` snapshot to `ChronoReplayState`
- `b3eee85` — journal: DEFER

236/236 pancetta-ft8 lib tests pass.

## What was proven

The chrono-replay tier successfully populates the `within_qso` snapshot across consecutive WAVs. Smoke test at slot 5/33: callsign snapshot grew from 108 (baseline, mechanism off) to 178 (mechanism on, +70 callsigns persisted). **The mechanism IS firing.**

## What's NOT determined

- Bootstrap CI on recall + composite: only 5 of 33 slots completed under load avg ~96% from ~32 concurrent eval processes in sibling agents
- At slots 1-5, recall was identical (17/17/19/19/20 baseline = 17/17/19/19/20 treatment) — mechanism's emissions don't intersect jt9 truth at observed slots, but 5/33 is insufficient for the CI bar

## Why not merged

Cherry-picking the implementation onto main would require additive merge of 6+ conflict blocks (with hb-048 a7, hb-057 dt_history, hb-064 ensemble Ft8Config additions). Production behavior is default-off and the binding A/B test hasn't completed.

Per Phase A discipline: don't merge code that can't be tested against composite. The implementation is preserved on the iter branch for the future Session-4 retest under lower CPU load.

## Pattern

This is the third Batch 18 item (hb-057 V2, hb-064 S3, hb-173 chrono-retest) blocked by concurrent CPU contention from sibling agents. The chronological-replay tier itself works — the bottleneck is running multiple long-elapsed evals simultaneously under bounded compute.

Recommended infra follow-up: add a `--max-concurrent-tiers` knob to eval (or document a serialization protocol for agents) so future batches sequence rather than saturate.
