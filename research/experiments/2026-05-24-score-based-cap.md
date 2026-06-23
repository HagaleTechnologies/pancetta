---
slug: score-based-cap
mode: ft8
state: shelved
created: 2026-05-24T03:00:00Z
last_updated: 2026-05-24T03:00:00Z
branch: iter/2026-05-24-batch-3
parent_hypothesis: hb-042
wild_card: false
scorecard: /tmp/sb_500_{4.0,4.5,5.0}.json
delta_vs_main: +7 rec / +113-124 novel — equivalent to hb-033's cap=500 finding
disposition: SHELVE hb-042 — score-based cap is just count-based cap in disguise. Same real/FP tradeoff as hb-033.
---

## Hypothesis

hb-042 (spawned from hb-033 2026-05-24): the cap=300 saturation
finding showed the marginal real/FP ratio collapses past rank 300.
A score-based cap (replace `max_sync_candidates` with
`min_sync_score`) might be more principled — let the noise floor
determine the cutoff, not a fixed count. hb-007 shelved
min_sync_score in an older state (cap=200, NMS on); the picture
might be different now (cap=300, NMS off, gate=2).

## Change

CLI-only sweep. Set cap=500 (effectively unbounded for hard-200
at current state) and sweep min_sync_score ∈ {4.0, 4.5, 5.0}
against the production baseline (cap=300, min_sync=3.0).

## Result

| config                    | composite  | rec  | novel | Δrec | Δnov |
|---------------------------|-----------:|-----:|------:|-----:|-----:|
| cap=300 min=3.0 (prod)    |  0.254489  | 4365 |   952 |    — |    — |
| cap=500 min=4.0           |  0.254897  | 4372 |  1076 |   +7 | +124 |
| cap=500 min=4.5           |  0.254897  | 4372 |  1076 |   +7 | +124 |
| cap=500 min=5.0           |  0.254897  | 4372 |  1065 |   +7 | +113 |

### Two stunningly informative redundancies

1. **cap=500 min=4.0 ≡ cap=500 min=4.5 (bit-identical).** No candidate
   in the top-500 has sync_score ∈ [4.0, 4.5]. Score distribution
   past rank 300 is concentrated above ~4.5.

2. **cap=500 + min=anywhere ≈ cap=500 alone** (compare hb-033 cap=500
   = 4372 rec / 1076 novel). The score floor doesn't bite within
   any range where the cap is still binding.

### What the score floor actually does

Going from min=4.5 → min=5.0 trims 11 novels at zero recall change.
That's the candidates with sync_score ∈ [4.5, 5.0] being filtered out
(11 of them yield novel decodes; 0 yield real decodes). Going higher
would continue smoothly trimming — no sharp elbow.

## Disposition

**SHELVE hb-042.** A score-based cap is just a count-based cap in
disguise — they parameterize the same tradeoff. The bottleneck isn't
the cap mechanism; it's the **precision wall** (per the cumulative
finding across hb-014 / hb-034 / hb-035 / hb-041). Past the elbow,
every additional candidate admitted is a worse real/FP bet than the
ones before it, and no parameterization of "where to cut" changes
that.

## Learnings

- **hb-007's "min_sync_score is dead" finding HOLDS at the new cap.**
  Even at cap=500 with NMS off and gate=2 (vs hb-007's cap=200 NMS-on
  gate=4 state), the score threshold doesn't bite below ~5.0. The
  candidate-score distribution shape didn't change meaningfully under
  the new production state.
- **Score and count caps are dual parameterizations of the same
  pruning rule.** Both are "cut the candidate list at some criterion."
  A meaningful difference would only emerge if the criterion's shape
  changed corpus-by-corpus — but on hard-200, the score distribution
  is monotonic and smooth, so any cap (by count or by score) just
  picks a point on the same Pareto frontier.
- **Production stays at cap=300 / min_sync=3.0.** Composite is unchanged
  vs cap=500 in the marginal-decode regime — the +7 real decodes don't
  outweigh the +124 novels.

## Follow-ups added to hypothesis bank

- **hb-042 → CLOSED (SHELVED).** Score-based cap is not a different
  knob.
- **No new hypothesis spawned.** The precision wall is structural;
  changing how we PARAMETERIZE the cap doesn't address it. The path
  forward remains hb-044/045/046/047/048 (mr-001 sources) and hb-024
  derivatives (FP filter infra).

## Reproducing

```bash
for ms in 4.0 4.5 5.0; do
  cargo run --release -p pancetta-research --bin eval -- \
      --tier curated-hard-200 --mode ft8 \
      --max-sync-candidates 500 --min-sync-score $ms \
      --output /tmp/sb_500_$ms.json
done
```
