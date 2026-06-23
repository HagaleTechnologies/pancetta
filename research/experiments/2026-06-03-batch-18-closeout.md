---
slug: batch-18-closeout
mode: ft8
state: completed
created: 2026-06-03T08:00:00Z
disposition: Batch 18 COMPLETE — 8/10 items decided + landed; 2 items decided on iter branches (not merged to main due to additive-conflict cost vs SHELVED-or-DEFER value)
---

## Batch 18 final tally

| Item | Outcome | Status |
|---|---|---|
| Chronological-replay tier | SHIPPED-INFRA | on main |
| hb-093 step-4 extension | SHELVED (family closed both populations) | on main |
| hb-048 S3 chrono-retest | SHELVED-CHRONO DEFINITIVE (CI-rigorous, family closed both paths) | on main |
| hb-057 V2 hook fix | DEFER (impl on main, CI-binding A/B blocked by contention) | on main |
| hb-173 chrono-retest | DEFER (mechanism PROVEN firing; impl on iter branch) | note on main |
| hb-064 S3 Wortsman-compliant | SESSION-3-COMPLETE-A/B-PENDING (negative offline prior) | on main |
| **hb-091 a8 early-decode** | **INCOMPLETE — never finalized** | iter branch only |
| **hb-194 S3 output-space ensemble** | **SHELVED — output-space mode closed** | iter branch only |

## hb-091 disposition

The hb-091 a8 early-decode diagnostic was started but never finalized. Two finish-up agents zombie-fired without completing. The diagnostic example file was authored but never built/run, so no PROCEED/SHELVE decision can be claimed. Status: pending re-dispatch in a future batch under lower CPU load.

Iter branch: `iter/2026-06-02-hb-091` (only contains the previous main tip + untracked diagnostic example file)

## hb-194 S3 output-space ensemble disposition

A/B did complete in background despite the agent zombie-firing. Real numbers (FP filter ON, deterministic seed):

| metric | baseline (N=1) | ensemble (N=8) | Δ |
|---|---:|---:|---:|
| hard-200 recovered | 4942 / 8576 | 4934 / 8576 | **−8** |
| hard-1000 recovered | 14987 / 28104 | 14996 / 28104 | **+9** |
| composite | 0.279114 | 0.278663 | **−0.000452** |

Net-zero recall delta + 8× per-OSD-call inference cost = unambiguous SHELVE.

**Family closed across both modes**: weight-space averaging (Session 2, Batch 17 SHELVE) AND output-space ensemble (this Session 3 SHELVE). Per Lakshminarayanan 2017 + Wortsman 2022 correctly applied.

**Session 1's +55% offline metric is now formally disqualified** as a production indicator — the N=55 test fold's distribution did not represent the production composite distribution.

Decision logged in journal at `research/experiments/2026-06-02-hb-194-session3.md` on the iter branch. Not merged to main because the impl carries additive conflicts with hb-048 S3 + hb-057 + hb-064 (which all landed on main first) and the result is SHELVED with no production change anyway.

Iter branch: `iter/2026-06-02-hb-194-session3` (3 commits: 04e970c impl + 0640a4b export script + fbb9660 SHELVE journal)

## Cross-cutting patterns observed across Batch 18

1. **CPU contention dominated wall-clock.** With 8+ concurrent eval agents from sibling iters, load avg peaked 96-135 on 10 cores. Several agents (hb-057, hb-064, hb-173, hb-091) had to DEFER because evals never completed. Future batches should serialize heavy evals OR add a `--max-concurrent-tiers` knob.

2. **Cross-slot mechanisms cleanly SHELVED on real data.** Chronological-replay tier worked (statefulness proven for all 3 cross-slot items), but a7 (hb-048) and within-QSO (hb-173) emissions added FPs without recall gains. The "cross-slot context unlocks new recall" hypothesis is closed for the a7-template family at least.

3. **Neural OSD family closed on both ensemble modes.** Output-space ensemble (hb-194 S3) and weight-space (Session 2) both SHELVED. Future neural-OSD work needs different architecture or larger dataset.

4. **Post-audit rules worked.** Bootstrap CI gated every decision. No false GRADUATEDs. Phase A sub-state labels used correctly. Primary sources cited per shelve.

## Cleanup actions

- Both /tmp worktrees (`/tmp/pancetta-hb091`, `/tmp/pancetta-hb194-s3`) remain available for inspection.
- Iter branches preserved (no force-delete) for future cherry-pick if priorities change.
- Production state unchanged from this batch (no GRADUATED items).

## Next-batch suggestions

- Resolve the additive-conflict surface by merging Ft8Config field additions in a single integration pass before launching next batch
- Add `--max-concurrent-tiers` or similar concurrency guard to prevent CPU starvation
- Re-dispatch hb-091 (small task) under lower load to get the missing latency-axis data point
- Consider whether the "chronological-replay tier unlocks recall" thesis warrants more attempts after a7 + within-QSO both SHELVED
