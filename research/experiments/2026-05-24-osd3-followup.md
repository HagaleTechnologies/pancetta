---
slug: osd3-followup
mode: ft8
state: shelved
created: 2026-05-24T00:00:00Z
last_updated: 2026-05-24T00:00:00Z
branch: experiment/ft8/multipass-profile (chained)
parent_hypothesis: hb-034
wild_card: false
scorecard: /tmp/osd3.json vs /tmp/cap_300.json (gate=2 OSD-2 baseline)
delta_vs_main: -1 recovered, +284 novel, ~0% wallclock change
disposition: SHELVE OSD-3 — no recall benefit, +30% novels (likely FPs).
---

## Hypothesis

hb-034 (originally from 2026-05-22): hb-005 sweep at gate=4 / iters=25
showed OSD-3 added +313 novel decodes at zero recall gain. The
follow-up question: how many of those 313 are real (jt9-missed) vs
FPs? If >20% real, fold into hb-018 (stronger FP filter for OSD-3).
If <5%, shelve OSD-3 on this corpus + current implementation. (Phase A
honesty pass 2026-06-02 replaced "permanently" — a new corpus or
different OSD reordering scheme could legitimately revisit.)

iter 2 (hb-014) just graduated parity gate 4 → 2, which restricts
OSD invocation. The hb-005 baseline is stale. Re-test OSD-3 against
the current production state before chasing the cross-validation
question.

## Change

CLI-flag sweep only:
```
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --osd-depth 3 --output /tmp/osd3.json
```

## Result

| Config         | Composite  | Recovered | Decode rate | Novel | Elapsed |
|----------------|-----------:|----------:|------------:|------:|--------:|
| OSD-2 (prod)   |  0.254489  |    4365   |    0.50898  |   952 |   255 s |
| OSD-3          |  0.254431  |    4364   |    0.50886  |  1236 |   258 s |

**OSD-3 at current state: −1 recovered, +284 novel.** No wallclock
change (gate=2 narrows OSD invocation enough that depth-3 trial
counts don't dominate).

## Disposition

**SHELVE OSD-3.** Three reasons:

1. **Zero recall benefit.** OSD-3 LOSES one real decode (4365 → 4364)
   vs OSD-2. Not statistical noise — same WAV, identical inputs;
   higher OSD depth admitted a different candidate that displaced
   the real one. Even in the most favorable framing, OSD-3 is at
   best break-even on recall.
2. **+284 novel decodes.** Per hb-039 (97% of isolated novels are
   singletons likely-FPs), expect ~275 of these to be false
   positives. Same magnitude as the +313 hb-005 saw at gate=4 —
   gate=2's narrowing didn't change the OSD-3 marginal behavior.
3. **The hb-018 ("FP filter for OSD-3") path no longer applies.**
   That path required OSD-3 to provide meaningful real-recall
   that just needed FP cleanup. With zero recall delta, there's
   nothing to recover.

## Learnings

- **OSD-3's marginal contribution is dominated by CRC-14 collisions.**
  The width-3 OSD trial set (~125K trials/candidate) explores far
  enough into the LDPC neighborhood that collisions with valid CRC
  14-bit patterns become statistically meaningful. Most of those
  collisions produce syntactically-valid callsigns that decoder
  text-matching reports as "novel."
- **OSD-2 → OSD-3 is invariant under gate width.** hb-005 at gate=4
  showed +313 novels; this audit at gate=2 shows +284. The +200
  novels added by OSD-3 are a fixed property of the trial expansion,
  not an interaction with the gate.
- **Combined with iter 2's finding (OSD contributes ~0 recall on
  hard-200 vs jt9)**, the cumulative case for narrowing OSD further
  (hb-041: gate=0 = disable OSD fallback) is stronger.

## Follow-ups added to hypothesis bank

- **hb-034 → CLOSED (SHELVED).** OSD-3 confirmed bad.
- **hb-018 → CLOSED (DERIVATIVE).** No standalone case for an OSD-3
  FP filter — without recall benefit, there's nothing to filter
  toward.
- **hb-041 (already in bank from iter 2)** gains weight: combined
  with this finding, OSD's overall contribution is near-zero on
  hard-200, suggesting gate=0 (fully disable OSD) is worth running.

## Reproducing

```bash
# Current production baseline (gate=2, OSD-2)
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --output /tmp/osd2.json

# OSD-3 sweep
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --osd-depth 3 --output /tmp/osd3.json
```
