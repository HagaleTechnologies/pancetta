---
slug: costas-two-of-three
mode: ft8
state: shelved
created: 2026-05-25T18:00:00Z
last_updated: 2026-05-25T18:00:00Z
branch: iter/2026-05-25-batch-10
parent_hypothesis: hb-054
wild_card: false
scorecard: research/scorecards/hb-054-{off,on}.json
delta_vs_off: -1 rec / +35 novel on hard-200 (both no-filter)
disposition: SHELVE hb-054 — trailing-block sync fallback adds FPs with zero recall gain on pancetta's corpus. Code reverted; production bit-identical. Spawned hb-070 (gated rescue).
---

## Hypothesis

hb-054 (spawned from mr-002 JTDX harvest 2026-05-25): JTDX
`lib/sync8.f90` scores each Costas candidate as
`sync2d(i,j) = max(syncf, syncs)`, where `syncf` averages the
correlation across all three 7-symbol Costas blocks (leading +
middle + trailing) and `syncs` averages only the trailing two
(middle + trailing). On a signal whose **leading** Costas block is
corrupted — a collision, a late start, or ionospheric onset eating
the first ~0.5 s — accepting the trailing-block score recovers an
otherwise-lost candidate. mr-002 confirmed a clean attach point in
pancetta's `compute_costas_score`, which already iterates all three
blocks.

## Change

`pancetta-ft8/src/decoder.rs::compute_costas_score`: bucket the
per-symbol neighbor-difference score per Costas block instead of one
running sum. Form `syncf = sum(all blocks) / sum(all nums)` (the
existing behavior) and `syncs = sum(trailing blocks) / sum(trailing
nums)`. When the new `Ft8Config::costas_two_of_three` flag is set,
the candidate score is `max(syncf, syncs)`; otherwise just `syncf`.
Research `with_costas_two_of_three` builder + `--costas-two-of-three`
eval flag for the A/B.

## Result

Both runs no-filter on hard-200 (the FP filter never drops
*recovered* decodes — their callsigns match jt9 truth, hence are in
the baseline reference set — so recall is filter-invariant and the
no-filter A/B isolates the sync change cleanly):

| config                       | recovered | novel | rate    |
|------------------------------|----------:|------:|--------:|
| flag OFF (= production)       |      4377 |  1787 | 0.51038 |
| flag ON (max(syncf, syncs))  |      4376 |  1822 | 0.51026 |
| delta                        |        -1 |   +35 |  ~0     |

- **Zero recall gain** (-1 recovered is float-order noise at the
  sync threshold).
- **+35 novels (+2.0%)** — pure FP inflation.

### Methodology note (recorded so the next iter doesn't repeat it)

The first comparison ran flag-ON no-filter (novel=1822) against
`main.json` hard-200 (novel=823) and looked like +999 novels. That
was apples-to-oranges: `main.json` was generated **with**
`--fp-filter-baselines` (batch 9), the eval's FP filter is opt-in,
and my run had no filter flags. Running a flag-OFF no-filter control
collapsed the apparent +999 to the true +35. **Always A/B against a
control built with the identical filter invocation, never against a
stored scorecard whose filter flags you can't see.**

## Why it loses

`max(syncf, syncs)` can only ever *raise* a candidate's score, so it
strictly relaxes the effective `min_sync_score` gate. On pancetta's
hard corpus — busy-band operator recordings of full-slot signals —
the leading Costas block is rarely the limiting factor, so no real
signal is rescued. What the relaxed gate does surface is noise
candidates whose trailing two blocks happen to align better than all
three; a fraction of those clear LDPC+CRC as CRC-14 collisions and
become novel FPs. This is the **precision wall** again (batches 2-8):
any lever that admits more candidates admits more FPs, and on this
corpus the recall side is already saturated.

JTDX gets value from this because its sensitivity edge is on weak,
single-station, sometimes slot-misaligned captures where the leading
block genuinely drops out — not the dense multi-signal slots that
dominate pancetta's curated tiers.

## Decision

**SHELVE.** Reverted all three source edits so production sync
scoring is bit-identical (no dead flag left in the hot path). Journal
retained.

## Learnings / follow-ups

- **hb-070 (NEW, priority 0.30):** *gated* trailing-block rescue —
  apply `syncs` only when the leading-block score is detectably
  depressed relative to the trailing blocks (e.g. `syncf < syncs - δ`
  AND `block[0] << block[1..]`), so a clean signal never gets the
  relaxed gate. Upside is bounded by how many hard-corpus signals
  actually have a corrupted leading block (this run says: very few),
  so priority is low. Better motivated against a slot-misaligned
  corpus (cf. hb-025: wild-50 captures sit at dt ∈ [-2.5, -1.4]).
- Confirms the mr-007 lesson once more: a JTDX technique that helps
  *its* corpus (weak single-station) can be net-negative on
  pancetta's (dense busy-band). The architecture-fit question is
  really a *corpus*-fit question for sensitivity levers.
</content>
</invoke>
