---
slug: nms-disable-audit
mode: ft8
state: won
created: 2026-05-22T00:00:00Z
last_updated: 2026-05-22T14:01:56Z
branch: experiment/ft8/nms-disable-audit
parent_hypothesis: hb-019
wild_card: true
scorecard: research/scorecards/history/2026-05-22-nms-disable-audit.json
delta_vs_main: +0.0156 composite
disposition: WIN — production default flipped to nms_enabled=false; biggest gain since hb-023
---

## Hypothesis

hb-019 was a wild-card audit: disable non-maximum suppression of
Costas sync candidates and see what happens. NMS is a heuristic that
drops candidates within `NMS_TIME_RADIUS = 4 * TIME_OSR = 8` time
steps and `NMS_FREQ_RADIUS = 2` frequency bins of any stronger
candidate, before LDPC sees them. The bank entry was pre-disposed to
LOSS: "Very likely a regression; purely exploratory."

The actual question: are NMS radii too aggressive, merging real
adjacent signals on busy bands?

## Change

Production change: `Ft8Config::nms_enabled` defaults to `false` (was
`true`). The `nms_candidates(&mut candidates)` call in
`costas_sync_search` is now gated by this flag.

Research infrastructure:
- `pancetta-ft8/src/decoder.rs` — added `Ft8Config::nms_enabled: bool`
  field; gated the call in `costas_sync_search`; default changed
  `true → false`.
- `pancetta-research/src/decoder.rs` — `with_nms_enabled(bool)`
  builder method.
- `pancetta-research/src/bin/eval.rs` — `--no-nms` CLI flag.

## Result

**Hard-200 A/B:**

| NMS state | rate    | rec  | novel | composite | time(s) |
|-----------|---------|------|-------|-----------|---------|
| Enabled (default) | 0.4746 | 4070 | 875  | 0.2373    | 107.3   |
| Disabled  | 0.5057  | 4337 | 1037  | 0.2529    | 191.2   |

**Δ: +267 recovered (+6.6%), +162 novel.** Wall-clock nearly doubles
(+78%) — expected (redundant candidates of strong signals now compete
in LDPC), but still well within the 3000 ms per-WAV budget (the
slowest setting averages ~960 ms/WAV).

**Full 5-tier eval at nms_enabled=false vs main:**

| Tier             | Metric        | Main    | Branch  | Δ        |
|------------------|---------------|---------|---------|----------|
| fixtures         | pass_rate     | 1.0     | 1.0     | 0        |
| synth-clean      | per-SNR bins  | same    | same    | 0        |
| curated-hard-200 | recovered     | 4070    | 4337    | **+267 (+6.6%)** |
| curated-hard-200 | novel         | 875     | 1037    | +162     |
| curated-hard-200 | decode_rate   | 0.4746  | 0.5057  | +0.0311  |
| curated-hard-1000| recovered     | 12447   | **14153**| **+1706 (+13.7%)** |
| curated-hard-1000| novel         | 2742    | 3618    | +876     |
| curated-hard-1000| decode_rate   | 0.4429  | 0.5036  | +0.0607  |
| wild-50          | recovered     | 0       | 0       | 0        |
| wild-50          | novel         | 3       | 4       | +1       |
| composite        |               | 0.5373  | **0.5529** | **+0.0156** |
| 5-tier elapsed   |               | 783.5 s | 1237.0 s| +454 s (+58%) |

**+1973 total recovered decodes** across the two curated tiers.
Hard-1000's +13.7% relative gain is exceptional — bigger than hb-003
(+5.0%) on the same tier. **Fixtures and synth-clean unchanged at
every measurement** — strongest guard signal that the new decodes are
real (the FP guard tiers see no new false positives).

## Disposition

**WIN.** Production default `Ft8Config::nms_enabled` flipped to
`false`. Second-biggest composite delta of the run (+0.0156, after
hb-023's +0.0279). All five tiers either improved or were unchanged.
No regression flags.

Wall-clock cost is significant (+58%). On the dev box (M-series), the
5-tier eval went from 783s to 1237s. On the operator's Windows MiniPC
this will be slower in absolute terms but still well within FT8's
15-second slot budget. hb-008 (NMS radius sweep, priority bumped from
0.52 to 0.65) is the natural follow-up: tightening the radii rather
than fully disabling could keep ~80%+ of the gain at ~50% of the
wall-clock cost.

## Learnings

- **The bank entry's prior was wrong.** hb-019 predicted "very likely
  a regression; purely exploratory." Reality: +1973 recovered decodes.
  The historical NMS radii (time=8, freq=2) were silently merging
  real adjacent signals — not weak duplicates of strong ones. The
  conventional wisdom about NMS being a pure efficiency optimization
  was wrong for FT8's typical signal density (50+ simultaneous
  stations on a busy band, ~12.5 Hz tone spacing).

- **Wild-card audits can produce the biggest wins.** Three of the
  recent six cycles were exploitation sweeps (hb-005 +0.0008, hb-006
  +0.0003), all marginal. The wild-card audit hb-019 yielded
  +0.0156 — bigger than every cycle since hb-023. The bank's
  wild-card ratio (0.20 target) was set with this kind of asymmetric
  outcome in mind; this cycle vindicates the policy.

- **NMS radii are too coarse for FT8 signal density.** With
  NMS_FREQ_RADIUS=2 bins, signals 25 Hz apart get merged. Common QSO
  patterns place stations within 100 Hz of each other, often within
  25 Hz on a crowded band. The historical radii were probably
  inherited from VHF FM-equivalent settings or carried over from a
  reference implementation without retuning for FT8's density.

- **Fixtures + synth-clean as FP guard worked exactly as designed.**
  +0.0156 composite gain on the busy curated tiers with ZERO change
  on the AWGN guard tier is the cleanest signal the harness has
  produced. The +876 novel decodes on hard-1000 are most likely real
  (jt9 missed them); they should be revisited by hb-024 once that
  lands.

- **Diminishing returns trend is broken.** The cycle composite trend
  was 0.0279 → 0.0128 → 0.0008 → 0.0003, suggesting parameter sweeps
  were running out of room. This wild-card structural change snapped
  back to +0.0156. Suggests the right next moves are *not* more
  parameter sweeps but more structural audits (hb-030 subtraction
  quality, hb-024 cross-validation, hb-015 Doppler — all of which
  challenge an assumption rather than tune a knob).

## Follow-ups added to hypothesis bank

- **hb-008 priority bumped 0.52 → 0.65** (now a high-priority direct
  follow-up). The radius sweep can recover the +58% wall-clock cost
  while keeping most of the +0.0156 composite gain. If
  NMS_FREQ_RADIUS=1 captures 80%+ of the recovered decodes with
  ~30% time cost (vs the +58% of fully disabled NMS), that's the
  better long-term production setting.

- **hb-024 (cross-validate novel) becomes more urgent.** The +876
  novel decodes on hard-1000 are a much larger validation target
  than before. If a meaningful fraction are real (jt9 missed them),
  pancetta's true vs_wsjtx_pct on hard-1000 is now well above 50%.

## Reproducing

```bash
# Hard-200 A/B:
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/sweep/hard200-nms-on.json
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --no-nms \
    --output research/scorecards/sweep/hard200-nms-off.json

# Full 5-tier at nms_enabled=false:
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 --no-nms \
    --output research/scorecards/nms-disable-audit.json
```
