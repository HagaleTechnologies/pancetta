---
slug: adaptive-osd-depth
mode: ft8
state: shelved
created: 2026-05-25T19:30:00Z
last_updated: 2026-05-25T19:30:00Z
branch: iter/2026-05-25-batch-10
parent_hypothesis: hb-055
wild_card: false
scorecard: n/a (architecture-fit shelve — no code, no eval)
delta_vs_main: n/a
disposition: SHELVE hb-055 via mr-007 — adaptive OSD depth has no headroom in pancetta (no depth 4/5; deeper refuted; trigger needs offline-unavailable hints).
---

## Hypothesis

hb-055 (mr-002 JTDX harvest 2026-05-25): JTDX `lib/ft8b.f90` runs
OSD at `ndeep=3` by default, `ndeep=4` when a QSO/MyCall signal is
detected at proximity, and `ndeep=5` when the reacquisition filter is
active — spending OSD effort where prior evidence says a real signal
lives. Port the adaptive-depth idea to pancetta.

## Architecture-fit audit (mr-007, applied at pick time)

Three independent blockers, each fatal on its own:

### 1. pancetta's OSD has no depth 4 or 5

`pancetta-ft8/src/osd.rs::decode` implements exactly OSD-0 (hard
decision), OSD-1 (single flip), OSD-2 (pair flips), OSD-3 (triple
flips, C(91,3)=125,580 trials). There is no depth-4/5 code. JTDX's
ndeep ladder {3,4,5} maps onto pancetta's depth ladder {0,1,2,3},
which has already been **fully swept** — there is nothing deeper to
adapt up to.

### 2. The deeper end of pancetta's ladder is already refuted

- **hb-034** (batch 2): OSD-3 vs OSD-2 at production state loses 1
  real decode AND adds +284 novels (CRC-14 collisions from the wider
  trial expansion). Net-negative.
- **hb-041** (post-batch-2): OSD-0 (disable fallback) drops fixtures
  1.0 → 0.875 — OSD-2 is the only decode path for the basicft8
  ground-truth signal. Can't go shallower either.
- **hb-053 revisit** (batch 7): OSD-3 + the production FP filter still
  loses 1 real decode vs OSD-2 + filter. The filter neutralizes the
  novel cost but not the recall loss.

OSD-2 is the pinned elbow from both directions. Adaptive depth among
{0,1,2,3} has nowhere profitable to move.

### 3. The adaptive *trigger* needs hints offline eval can't provide

JTDX bumps ndeep based on QSO/MyCall proximity and the reacquisition
filter — i.e. the AP context (my_call list, recent_calls). The eval
harness doesn't populate that context, and **hb-051** (batch 4)
measured the AP-recovery ceiling at **1 decode out of 8576** on
hard-200 even with truth-callsign-perfect AP information. So even an
on-air adaptive trigger has essentially zero headroom on this corpus;
offline it has literally none.

## Why OSD depth is the wrong lever generally

hb-014 established that OSD contributes ~0 recall vs jt9 truth on the
hard corpora (gate=0 = gate=6 on recovered) — its sole demonstrated
value is the one basicft8 fixture (hb-041). "Spend more OSD effort"
therefore can't add recall here, and "spend less" risks the fixture.
The recall lever that *did* work this batch was the BP schedule
itself (hb-063 layered), not OSD depth.

## Decision

**SHELVE.** No code, no eval. The mr-007 audit at pick time closed it
— exactly the procedure's purpose. The harvest-time mr-007 (mr-002
agent) rated this 0.50 / "clean attach" because it lacked pancetta's
internal OSD-ladder findings (hb-034/041/051/053); pick-time mr-007
has them.

## Learnings / follow-ups

- No new hypotheses. The OSD-depth surface is now closed from every
  direction documented (shallower: hb-041; deeper: hb-034/053;
  adaptive trigger: hb-051).
- Pattern: a JTDX/WSJT-X technique whose value depends on QSO-context
  AP hints (hb-055, and earlier hb-027/hb-050) is structurally a
  poor fit for pancetta's offline single-slot eval — and hb-051 says
  the on-air ceiling is tiny too. Future harvests should down-rank
  AP-hint-dependent ideas for the ft8 focus mode.
- Remaining mr-003 OSD work that ISN'T about depth: hb-065 (adaptive
  Gaussian-elimination removal — a speed lever, needs an OSD profile
  first) and hb-064 (DIA trajectory features — could retrain the
  neural OSD on the new layered-BP trajectories). Both orthogonal to
  hb-055.
</content>
