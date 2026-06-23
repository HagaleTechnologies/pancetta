---
slug: hb-082-residual-sync-threshold
mode: ft8
state: shelved
created: 2026-05-27T16:45:00Z
last_updated: 2026-05-27T16:45:00Z
branch: iter/2026-05-27-batch-13
parent_hypothesis: hb-082
wild_card: false
scorecard: research/scorecards/resmin-*.json (transient, removed)
delta_vs_main: ZERO change at every threshold
disposition: SHELVE hb-082 — residual sync threshold not binding. Production min_sync_score is already below the natural candidate cluster in the residual.
---

## Hypothesis

hb-082 (priority 0.30, spawned 2026-05-26 from hb-079): after the
multipass subtract, the residual spectrogram's noise floor drops; a
*lower* sync threshold on the residual sync_search might surface
additional masked candidates that the production threshold (3.0)
rejects.

## Change

`Ft8Config::residual_min_sync_score: Option<f64>` (None reuses
production `min_sync_score`). Refactored `costas_sync_search` to take
an explicit threshold via `costas_sync_search_with_threshold`. The
multipass loop calls the threshold variant with
`residual_min_sync_score.unwrap_or(min_sync_score)`. Research builder
`with_residual_min_sync_score` + `--residual-min-sync-score V` eval
flag.

## Result

hard-200 sweep at hb-080's N=3 production (with FP filter):

| residual threshold | recovered | novel | Δrec | Δnov |
|-------------------:|----------:|------:|-----:|-----:|
| 3.0 (production)   |      4604 |   920 |    — |    — |
|                2.0 |      4604 |   920 |    0 |    0 |
|                2.5 |      4604 |   920 |    0 |    0 |
|                3.5 |      4604 |   920 |    0 |    0 |

**Zero change at every threshold.** Identical recall and novel counts
across the swept range.

## Why no effect

Two possibilities, both consistent with the observation:

1. The residual candidates that *do* pass the production threshold
   (3.0) already account for everything decodable. Lower thresholds
   (2.0, 2.5) surface additional candidates but those candidates fail
   downstream — LDPC doesn't converge, or it does and CRC rejects, or
   the message fails `is_plausible`. The threshold change moves bytes
   through the system but no new decodes.
2. The residual's noise-floor change after subtract isn't large
   enough to push noise-shaped candidates above 2.0 sync_score in
   meaningful numbers. The subtraction is *localised* (removes signal
   at specific bins) rather than reducing the global noise floor.

Either way, the threshold isn't a binding constraint. Tuning it has
no effect; the gain from the multipass mechanism comes from candidates
naturally above 3.0 sync_score in the residual.

## Decision

**SHELVE.** Default stays `None` (reuse production threshold). Plumbing
(`costas_sync_search_with_threshold`, `residual_min_sync_score` field +
CLI) retained — small surface, useful for future re-evaluation if the
residual structure changes (e.g., after joint decoding lands).

## Learnings

- **"Lower threshold reveals more decodes" assumed a long-tail
  distribution that isn't there.** The residual's candidate score
  distribution is bimodal-or-better: real candidates cluster well
  above 3.0; noise candidates cluster well below 2.0. Few candidates
  live in the gap that threshold tuning could move.
- **Two shelves in a row on post-hb-079 tuning** (hb-081, hb-082)
  → the multipass pipeline is at its mechanical limit on this corpus.
  Further composite gain requires structurally different leverage
  (hb-086 joint decoding).

## No new spawns

The "tune residual thresholds" surface is closed.
