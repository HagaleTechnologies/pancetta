---
slug: sync-cap-saturation
mode: ft8
state: shelved
created: 2026-05-24T00:00:00Z
last_updated: 2026-05-24T00:00:00Z
branch: experiment/ft8/multipass-profile (chained)
parent_hypothesis: hb-033
wild_card: false
scorecard: /tmp/cap_{300,400,500}.json
delta_vs_main: +7 decodes at +160% wallclock — not worth it
disposition: SHELVE hb-033 — cap=300 is at the elbow; higher caps add
  marginal recall at large wallclock cost and degraded precision.
---

## Hypothesis

hb-033: after hb-038 graduated `max_sync_candidates: 200 → 300` for
+0.0023 composite, the natural follow-up is "is 300 still the elbow,
or is there more headroom at cap=400 / 500?" Diagnostic sweep on
curated-hard-200 to see saturation behavior.

(This is the simpler version of hb-033 — the original bank entry
called for per-candidate-rank instrumentation. The user's batch
plan asked for the saturation check first, before deeper
instrumentation.)

## Change

CLI-flag sweep only. No code changes — `--max-sync-candidates N`
already existed. Ran cap ∈ {300, 400, 500} on curated-hard-200 with
all other knobs at production defaults (including the new gate=2
from iter 2).

## Result

| cap | composite | recovered | rate | novel | elapsed | Δrec vs 300 | Δnov vs 300 |
|----:|----------:|----------:|------:|------:|--------:|------------:|------------:|
| 300 |  0.254489 |     4365  | 0.5090 |   952 |  254.8s |          0  |          0  |
| 400 |  0.254839 |     4371  | 0.5097 |  1026 |  408.9s |         +6  |        +74  |
| 500 |  0.254897 |     4372  | 0.5098 |  1076 |  661.6s |         +7  |       +124  |

**Marginal real/FP ratio:**
- Candidates 301..400 → 6 real / 74 novel = 1 real per 12 FPs
- Candidates 401..500 → 1 real / 50 novel = 1 real per 50 FPs
- Compared to candidates 1..300: ~4365 real / 952 novel = ~5 real per 1 FP

The marginal candidates past rank 300 are dramatically worse than the
ones before — exactly the saturation pattern hb-038 observed at the
200→300 transition, now reproduced at 300→400→500.

**Wallclock:**
- 300 → 400: +60% wallclock for +6 decodes (~0.07% recall)
- 300 → 500: +160% wallclock for +7 decodes (~0.08% recall)

## Disposition

**SHELVE hb-033.** cap=300 is at the recall/wallclock elbow.

Reasons not to raise the cap:
1. **Recall gain is trivially small.** +7/8576 = 0.08% additional real
   decodes vs the +160% wallclock cost. We blew past the operational
   budget (3000ms/WAV) on individual WAVs at cap=500.
2. **Precision degrades.** Novel-decode count grew faster than real
   (74 novels per 6 reals at cap=400; 124 novels per 7 reals at
   cap=500). Per hb-039, most novels are likely FPs.
3. **Per the iter-2 (hb-014) finding,** novel-decode count proxies
   on-air bad behavior (fake QSO attempts). Adding +124 novels for
   +7 real decodes is a precision regression.

## Learnings

- **The cap=200 → 300 elbow that hb-038 found is reproducible at
  every subsequent threshold.** Candidates ranked 200-300 are
  qualitatively similar to 300-400 are qualitatively similar to
  400-500: low-confidence Costas detections that mostly produce noise
  decodes. The "real elbow" isn't at a specific cap value — it's at
  the boundary where Costas sync score crosses a noise threshold.
- **A score-based cap (rather than count-based) would be principled.**
  Setting `min_sync_score` instead of `max_sync_candidates` would
  let the actual signal quality determine the cutoff. hb-007 already
  shelved a min_sync_score variant ("the knob is dead at the current
  cap"); worth revisiting now that we're saturating on the count.
- **Real/FP ratio falls off a cliff past rank ~300.** Going from 1:0.2
  (in the top 300) to 1:12 (in 300-400) to 1:18 (in 400-500). The
  signal-vs-noise floor on hard-200's busy bands is somewhere in the
  rank 200-300 range.

## Follow-ups added to hypothesis bank

- **hb-033 → CLOSED (SHELVED).** Cap=300 stays.
- **hb-042 (new)**: re-investigate `min_sync_score` as a principled
  cap. hb-007 shelved it at cap=200; the picture might be different
  at cap=300 with NMS off and gate=2.

## Reproducing

```bash
for n in 300 400 500; do
  cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --max-sync-candidates $n \
    --output /tmp/cap_$n.json
done
```
