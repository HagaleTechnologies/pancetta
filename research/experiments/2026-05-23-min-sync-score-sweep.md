---
slug: min-sync-score-sweep
mode: ft8
state: shelved
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T00:00:00Z
branch: experiment/ft8/min-sync-score-sweep
parent_hypothesis: hb-007
wild_card: false
scorecard: research/scorecards/sweep/ (hard200-msync-* + hard200-msync*-cap* sets)
delta_vs_main: 0 (production unchanged)
disposition: SHELVED — threshold knob is dead at current cap; cap is the only meaningful lever
---

## Hypothesis

`MIN_SYNC_SCORE = 3.0` in decoder.rs:74 is the Costas correlation
threshold below which a candidate is silently dropped before LDPC.
Per hb-007: on the busy hard-200 corpus, some real signals may have
sync scores just below 3.0; lowering the threshold could surface
marginal candidates that LDPC + CRC would then validate.

Expected delta: +0.01 to +0.04 real decode rate; FP risk if too low.

## Change

Pure research infrastructure — no production behavior changed.

- `pancetta-ft8/src/decoder.rs` — promoted `MIN_SYNC_SCORE` const to
  `Ft8Config::min_sync_score: f64` field. Default 3.0 matches the
  historical value. The Costas search loop reads from
  `self.config.min_sync_score`.
- `pancetta-research/src/decoder.rs` — `with_min_sync_score(f64)`
  builder.
- `pancetta-research/src/bin/eval.rs` — `--min-sync-score V` CLI flag.

## Result

**Single-knob sweep on hard-200, MIN_SYNC_SCORE ∈ {1.5, 2.0, 2.5, 3.0, 3.5, 4.0}:**

| threshold | rec  | novel | composite | time(s) |
|-----------|------|-------|-----------|---------|
| 1.5       | 4337 | 1037  | 0.2529    | 165.5   |
| 2.0       | 4337 | 1037  | 0.2529    | 194.1   |
| 2.5       | 4337 | 1037  | 0.2529    | 202.9   |
| 3.0 (def) | 4337 | 1037  | 0.2529    | 209.3   |
| 3.5       | 4337 | 1037  | 0.2529    | 213.5   |
| 4.0       | 4337 | 1037  | 0.2529    | 215.6   |

**All six settings produce IDENTICAL decode counts.** Only wall-clock
varies. The threshold knob is fully dead at the current production
`max_sync_candidates = 200`.

**Mechanism:** the threshold check runs BEFORE the
sort-by-score-and-truncate-to-200 step. With 200+ candidates per WAV
exceeding score=4.0 already, the truncate (not the threshold) is the
binding gate — the top 200 by score survive regardless of threshold.

**Combined sweep: (threshold ∈ {1.5, 2.0}) × (cap ∈ {300, 500, 800}):**

| thresh | cap | rec       | novel      | composite | time(s) |
|--------|-----|-----------|------------|-----------|---------|
| 3.0    | 200 | 4337 (--) | 1037 (--)  | 0.2529    | 209.3   | (baseline)
| 1.5    | 300 | 4376 (+39)| 1228 (+191)| 0.2551    | 288.3   |
| 1.5    | 500 | 4378 (+41)| 1479 (+442)| 0.2552    | 546.6   |
| 1.5    | 800 | 4375 (+38)| 1507 (+470)| 0.2551    | 998.8   |
| 2.0    | 300 | 4376 (+39)| 1228 (+191)| 0.2551    | 336.5   |
| 2.0    | 500 | 4377 (+40)| 1479 (+442)| 0.2552    | 563.9   |
| 2.0    | 800 | 4375 (+38)| 1507 (+470)| 0.2551    | 949.2   |

**threshold doesn't matter at any cap** (1.5 and 2.0 produce
bit-identical results). **cap=300 captures all the recovered gain**
(+39 rec); going 300 → 500 → 800 adds 0 net recovered but inflates
novel from 1228 → 1507 (likely FPs slipping past the parity gate).

## Disposition

**SHELVED.** Production stays at `min_sync_score = 3.0`. The
threshold knob is dead at the current cap=200 and remains dead at
caps up to 800 — the Costas search produces plenty of high-scoring
candidates on busy bands, so the threshold never bites.

The combined sweep DID reveal a possible cap-bump win (cap=300
yields +39 recovered, +0.0022 composite), but that's hb-003 territory
re-evaluated at the new nms-off baseline, not a hb-007 conclusion.
Spawned as hb-038.

## Learnings

- **Threshold-first hypotheses are misleading when an upstream
  truncate exists.** The conceptual model "raise/lower the threshold,
  see what surfaces" only works if the threshold is the binding
  constraint. Here `max_sync_candidates = 200` truncates BEFORE
  anything downstream sees the candidates, so the threshold is
  inert. Future "this filter is dropping signals" hypotheses should
  first establish that the filter is actually binding.

- **The novel-count saturation at cap=800 (1507) is informative.**
  Going from cap=200 to cap=800 added +470 novel decodes but only
  +38 recovered. Most of those +470 novels are very likely LDPC+CRC
  false positives on noise candidates that score just above noise.
  The current parity gate (MAX_PARITY_ERRORS_FOR_OSD=4) catches some
  but not all. This is corroborating evidence for the hb-024
  cross-validation hypothesis being increasingly urgent — the
  "novel" pool is growing in ways that aren't obviously real.

- **Lowering threshold WITHOUT raising cap is fully a no-op.** Save
  any future operator from trying this — the threshold field looks
  like a knob but isn't one.

- **The hb-003 elbow may have moved.** hb-003 (graduated 2026-05-22)
  set cap=200 based on a sweep under nms-on. Under nms-off (current
  production), the +39 decodes at cap=300 are real and weren't
  visible in the original hb-003 sweep. A re-sweep at the new
  baseline (hb-038) is justified.

## Follow-ups added to hypothesis bank

- **hb-038 (new)** — Re-sweep `max_sync_candidates` at the new
  nms-off baseline. hb-003 graduated under nms-on; the elbow may
  have shifted. Test cap ∈ {200, 250, 300, 400}. Expected delta:
  +30-40 recovered decodes at cap=300 on hard-200 (+0.001 composite)
  with +200-500 novel and +40-100% wall-clock cost. Decision: is
  the small composite gain worth the FP-bloat + CPU? Priority 0.50.
  Estimated effort: 0.5 sessions (5-tier eval at cap=300, compare).

## Reproducing

```bash
# Single-knob sweep:
for V in 1.5 2.0 2.5 3.0 3.5 4.0; do
    cargo run --release -p pancetta-research --bin eval -- \
        --tier curated-hard-200 --mode ft8 \
        --min-sync-score $V \
        --output research/scorecards/sweep/hard200-msync-$V.json
done

# Combined sweep:
for V in 1.5 2.0; do
    for C in 300 500 800; do
        cargo run --release -p pancetta-research --bin eval -- \
            --tier curated-hard-200 --mode ft8 \
            --min-sync-score $V --max-sync-candidates $C \
            --output research/scorecards/sweep/hard200-msync${V}-cap${C}.json
    done
done
```
