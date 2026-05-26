---
slug: osd-ge-profile
mode: ft8
state: shelved
created: 2026-05-25T21:40:00Z
last_updated: 2026-05-25T21:40:00Z
branch: iter/2026-05-25-batch-11
parent_hypothesis: hb-065
wild_card: false
scorecard: n/a (profile diagnostic)
delta_vs_main: n/a
disposition: SHELVE hb-065 — GE is 0.4% of OSD time; adaptive-GE removal is a no-op. TEP enumeration dominates (→ hb-064 territory).
---

## Hypothesis

hb-065 (mr-003, 0.45): OSD complexity is often dominated by the per-call
Gaussian elimination on the most-reliable basis; arXiv:2206.10957's two
early-decision conditions can skip GE on many calls. Profile first
(per the bank entry) — if GE < 20% of OSD time, shelve.

## Method

Temporary instrumentation in `osd.rs::decode`: time `gaussian_eliminate`
directly, and use a drop-guard (`TepTimer`) started right after GE to
accumulate all post-GE time (the TEP enumeration — OSD-0/1/2 flip loops
+ CRC) across every early return. Drove it with a research example over
all 200 hard-200 WAVs at the production config (layered BP, gate=6,
OSD-2). Instrumentation + example reverted after the run.

## Result

hard-200, 5947 decodes:

| phase                | total time | share |
|----------------------|-----------:|------:|
| Gaussian elimination |     529 ms |  0.4% |
| TEP enumeration      | 147,336 ms | 99.6% |

**GE is 0.4% of OSD time.** Eliminating it entirely would be a 0.4% OSD
speedup — well below the 20% shelve threshold.

## Why

At OSD-2, GE is a single GF(2) reduction of a 91×174 matrix per call,
while TEP enumeration runs OSD-0 (1) + OSD-1 (91) + OSD-2 (C(91,2)=4095)
= ~4187 trials per call, each recomputing parity + CRC-14. The
enumeration's ~4000:1 trial-count advantage swamps the one-time GE. The
bank's own evidence_against predicted exactly this.

## Decision

**SHELVE.** Adaptive GE removal (arXiv:2206.10957) is a no-op for
pancetta's OSD-2. Instrumentation reverted; production OSD unchanged.

## Learnings / follow-ups

- **Redirects OSD-speed work to TEP, not GE.** If OSD CPU ever needs
  cutting, the lever is reducing TEP trials — which is exactly **hb-064**
  (DIA-augmented OSD with iteration-trajectory features; that paper's
  headline is "97% TEP-enumeration reduction at SNR=2 dB"). The profile
  makes hb-064 the correct OSD-speed bet and closes hb-065.
- Note for hb-064: it's now doubly motivated — TEP dominates OSD (this
  profile) AND layered BP (batch 10) changed the per-iteration LLR
  trajectories the DIA model would consume, so a retrain on layered
  trajectories is the natural pairing.
- OSD speed isn't urgent regardless: the decoder is well within the
  3000 ms/WAV budget and layered BP already cut wall-clock -16%. This is
  a "if ever needed" lever, and hb-064 is the right one.
</content>
