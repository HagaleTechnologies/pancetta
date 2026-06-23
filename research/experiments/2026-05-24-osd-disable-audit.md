---
slug: osd-disable-audit
mode: ft8
state: shelved
created: 2026-05-24T01:00:00Z
last_updated: 2026-05-24T01:00:00Z
branch: experiment/ft8/multipass-profile
parent_hypothesis: hb-041
wild_card: false
scorecard: /tmp/gate_0_full.json
delta_vs_main: -0.0188 composite (entirely from 1 fixture failure); -10% novels; 0 hard-corpus recall change
disposition: SHELVE hb-041 — gate=0 loses 1 fixture-tested real decode. Gate=2 stays as production default.
---

## Hypothesis

hb-041 (spawned 2026-05-23 from hb-014): the iter-2 sweep showed
recall is FLAT on hard-200 from gate=0 through gate=4. If gate=0
also preserves synth-clean and fixtures, fully disabling OSD
fallback would be a precision + simplicity win (no OSD branch in
decode_soft).

The risk noted in the spawn: hard-200/1000 are derived from jt9
which doesn't use OSD; OSD may help on signals jt9 misses too,
which we'd want for true on-air sensitivity.

## Change

CLI-flag-only sweep. Ran full 5-tier eval at
`--max-parity-errors-for-osd 0` (OSD fallback never fires because
parity_errors > 0 is required to enter the OSD branch, but
parity_errors == 0 means BP already converged and OSD is skipped
anyway).

No code change yet — pending decision.

## Result

| Tier              | Metric              | gate=2 (main) | gate=0 (test) | Δ          |
|-------------------|---------------------|--------------:|--------------:|-----------:|
| **fixtures**      | pass_rate           |     1.000     |     0.875     |   **−1/8** |
| **synth-clean**   | SNR@50% (dB)        |    −20.0      |    −20.0      |          = |
|                   | SNR@90% (dB)        |    −18.0      |    −18.0      |          = |
|                   | each SNR bucket     |   (preserved) |   (preserved) |          = |
| curated-hard-200  | recovered           |     4365      |     4365      |          0 |
|                   | novel               |      952      |      860      |   −92 (−10%)|
| curated-hard-1000 | recovered           |    14219      |    14219      |          0 |
|                   | novel               |     3172      |     2836      |  −336 (−11%)|
| wild-50           | recovered           |        0      |        0      |          0 |
|                   | novel               |        4      |        1      |         −3 |
| **composite**     |                     |   0.554489    |   0.535739    | **−0.0188**|

**The composite drop is ENTIRELY from the fixture failure.**
Composite weight: fixtures_pass_rate = 0.15; 1.0 → 0.875 contributes
−0.0188 to composite. The remaining tiers contribute zero composite
delta.

### The failing fixture

```
basicft8/170923_082015.wav
  expected: ["any-decode"]   (at-least-one-message)
  got:      []
  notes: "ft8_lib basicft8 set. Current decoder finds 1 message."
```

This is a real off-air recording from the ft8_lib reference corpus.
At gate=2 (current production), pancetta decodes exactly 1 message.
At gate=0, pancetta decodes 0 messages.

**This is the ground-truth case where OSD provides the only path to
decode.** That ONE message survives only because OSD recovers it
from a BP-non-converged candidate with 1 or 2 parity errors.

## Disposition

**SHELVE hb-041.** Gate=2 stays as production default.

Reasons:
1. **Fixture corpus catches what curated corpora miss.** The hard-200
   and hard-1000 truth is jt9-derived. jt9 doesn't use OSD. So
   anything OSD recovers vs jt9 truth registers as "novel" (not
   in jt9's set). The fixture corpus, by contrast, gates on
   "did the decoder produce ANY decode" — which catches the cases
   where OSD provides the only decode path.
2. **OSD's value is small but non-zero.** On the entire hard-200
   /1000 corpora (~30k truth decodes), OSD contributes zero
   recall. On fixtures, it contributes 1 decode out of 13 WAVs.
   That's a tiny absolute contribution, but it's real, and the
   alternative is losing a real-world signal.
3. **Gate=2 is at the actual elbow.** The iter-2 sweep showed
   gate ∈ {0,1,2,3,4} all give the same hard-200 recall (4365).
   This audit shows gate=0 loses fixture-tested decodes that
   gate=2 catches. So gate=2 sits at the tightest setting that
   doesn't sacrifice fixture-tested recall.

## Learnings

- **The composite weighting matters when interpreting "OSD contributes
  zero recall."** Iter 2's claim ("OSD contributes ~0 recall vs jt9")
  was true but incomplete — it measured against jt9-derived truth,
  which by construction can't credit OSD with anything jt9 doesn't
  also find. The fixture corpus is the natural complement: it asks
  "given a real-world WAV, does the decoder find at least one
  message" — a recall metric that doesn't depend on jt9.
- **The 1-decode fixture margin is exactly right for OSD's role.**
  hb-014's "OSD is essentially dead for recall" finding holds in
  aggregate, but OSD's job isn't to drive bulk recall — it's to
  catch the marginal BP-near-miss that no other path recovers.
  Gate=2 keeps that role, and the iter-2 sweep showed gate=2 is
  also as precision-positive as gate=0 (only +10% extra novels,
  most of which are tolerable).
- **Future "disable OSD" hypotheses are now structurally closed.**
  Any further "tighten OSD even further" idea would need to first
  show that the fixture losing 1 decode is worth the FP reduction.
  Given that the fixture-decode-loss is structural (we'd lose 100%
  of that specific WAV's decode capability), this is essentially
  ruled out.

## Follow-ups added to hypothesis bank

- **hb-041 → CLOSED (SHELVED).** Gate=2 confirmed at the elbow.
- **No new hypothesis spawned.** The path is closed — OSD's role
  is narrowed but real, and gate=2 is the right value.

## Wild card ratio note

This iter was an exploitation pick despite the wild-card-ratio rule
(0.174 < 0.20 → next pick should be a wild card). Available wild
cards (hb-026 5+ sessions, hb-027 blocked by hb-004 AP-wiring,
hb-028 morally fuzzy + heavy) all unsuitable for a single iter
cycle. Picked hb-041 (priority 0.50, direct natural follow-up to
iter 2). Wild card ratio remains at 0.174 (4 wild / 23 total =
0.174 → 4 wild / 24 total = 0.167); next iter should TRY HARDER
to find a workable wild card pick, possibly a scoping cycle for
hb-027 (start with fixing hb-004 AP-wiring).

## Reproducing

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 --max-parity-errors-for-osd 0 \
    --output /tmp/gate_0_full.json
```
