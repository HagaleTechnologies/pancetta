---
slug: block-score-ranking
mode: ft8
state: shelved
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T00:00:00Z
branch: experiment/ft8/block-score-ranking
parent_hypothesis: hb-009
wild_card: false
scorecard: research/scorecards/sweep/hard200-blockscore-{on,off}.json
delta_vs_main: 0 (production unchanged; ranking is bit-identical for decoding)
disposition: SHELVED — re-rank ordering is irrelevant given parallel decode + no biting cap
---

## Hypothesis

Per hb-009: after Costas sync search, candidates are sorted by sync
score, truncated to `max_sync_candidates`, then RE-RANKED by
`block_score` before LDPC. Alternate ranking strategies (e.g., adding
`correlation_energy` as a weight) might prioritize "most likely to
yield a real decode" candidates better, particularly when budget
expires and lower-ranked candidates get skipped.

## Change

Pure research infrastructure — no production behavior changed.

- `pancetta-ft8/src/decoder.rs` — added `Ft8Config::block_score_rerank: bool`
  field (default true, matching historical). Gated the re-rank block.
- `pancetta-research/src/decoder.rs` — `with_block_score_rerank(bool)` builder.
- `pancetta-research/src/bin/eval.rs` — `--no-block-score-rerank` CLI flag.

## Result

**Hard-200 A/B:**

| variant         | rec  | novel | rate   | composite | time(s) |
|-----------------|------|-------|--------|-----------|---------|
| on (production) | 4365 | 1210  | 0.5090 | 0.2545    | 205.3   |
| off             | 4365 | 1210  | 0.5090 | 0.2545    | 217.8   |

**Bit-identical decode counts.** Block-score re-ranking has ZERO
impact on which candidates decode at the current production
configuration.

## Disposition

**SHELVED.** Re-ranking is dead infrastructure given (a) parallel
candidate decoding (rayon's order-of-completion doesn't depend on
input order) and (b) the budget timer + max_candidates dedup cap
don't bite at the current scale. Ranking changes WHICH 300 candidates
get tried first, but all 300 get tried; the unique-message dedup
hashset isn't filled (we get ~30-50 unique decodes per WAV from 300
candidates, far below max_candidates=100).

Production stays at `block_score_rerank: true` (status quo). The
field + CLI flag land as reusable infrastructure for any future
re-evaluation (e.g., if multi-pass infra is restored via hb-037 and
the budget timer becomes a binding constraint).

## Learnings

- **Ranking knobs are pointless when the consumer is parallel and
  unfiltered.** The hb-009 hypothesis pre-dated the parallel decode
  path; under serial decoding with a hard candidate cap, ranking
  would matter (top-N gets tried, bottom-(M-N) doesn't). Under rayon
  + N=M, ranking only affects completion order — which doesn't
  affect WHICH decodes succeed.

- **The block_score function isn't wasted.** It still runs (the
  truncation step uses sync_score). The re-rank just doesn't affect
  outcomes. Removing the re-rank would save a small amount of CPU
  per WAV but isn't a meaningful win — the timing A/B was within
  noise (205 vs 217 s).

- **Future hypothesis reviews should check the decode pipeline shape
  before proposing ranking changes.** A hypothesis that assumes
  "ordering matters" needs a binding consumer cap to be meaningful.

## Follow-ups added to hypothesis bank

None. The result is decisive. If a future hb-037 (subtract kernel
redesign) restores multi-pass and re-introduces a binding decode
budget, this hypothesis would be worth revisiting.

## Reproducing

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/sweep/hard200-blockscore-on.json
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --no-block-score-rerank \
    --output research/scorecards/sweep/hard200-blockscore-off.json
```
