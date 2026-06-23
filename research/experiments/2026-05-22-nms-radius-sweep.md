---
slug: nms-radius-sweep
mode: ft8
state: shelved
created: 2026-05-22T00:00:00Z
last_updated: 2026-05-22T00:00:00Z
branch: experiment/ft8/nms-radius-sweep
parent_hypothesis: hb-008
wild_card: false
scorecard: research/scorecards/sweep/ (hard200-nms-* set)
delta_vs_main: 0 (production unchanged)
disposition: SHELVED — radius tightening can't recover hb-019's win at meaningful wall-clock savings
---

## Hypothesis

hb-019 (graduated 2026-05-22) found that disabling NMS entirely
yielded +1973 recovered decodes across the curated tiers at the cost
of +58% wall-clock. hb-008's premise (priority bumped 0.52 → 0.65):
tightening NMS radii rather than fully disabling could keep most of
the sensitivity gain at lower wall-clock cost. Expected: a (time,
freq) pair that recovers ≥90% of hb-019's +267 hard-200 decodes at
≤50% of the +58% wall-clock cost.

## Change

Pure research infrastructure — no production behavior changed.

- `pancetta-ft8/src/decoder.rs` — promoted `NMS_TIME_RADIUS` (was
  `4 * TIME_OSR = 8`) and `NMS_FREQ_RADIUS` (was `2`) from hard
  consts to `Ft8Config::nms_time_radius` and `nms_freq_radius`
  fields. Default values reflect the historical consts; production
  behavior unchanged (`nms_enabled = false` from hb-019).
- `pancetta-research/src/decoder.rs` — `with_nms_time_radius(n)` and
  `with_nms_freq_radius(n)` builder methods.
- `pancetta-research/src/bin/eval.rs` — `--nms-on`, `--nms-time-radius`,
  `--nms-freq-radius` CLI flags. The radius flags implicitly enable
  NMS unless `--no-nms` is also passed.

## Result

**Hard-200 sweep (current production = nms_enabled=false):**

| config            | rec  | novel | composite | time(s) |
|-------------------|------|-------|-----------|---------|
| off (current)     | 4337 | 1037  | 0.2529    | 166.5   |
| t=0 f=0           | 4339 | 1022  | 0.2530    | 211.2   |
| t=1 f=0           | 4098 |  924  | 0.2389    | 148.4   |
| t=2 f=1           | 4082 |  894  | 0.2380    | 144.3   |
| t=2 f=2           | 4079 |  886  | 0.2378    | 144.8   |
| t=4 f=1           | 4077 |  892  | 0.2377    | 146.9   |
| t=4 f=2           | 4073 |  881  | 0.2375    | 145.9   |
| t=8 f=2 (old)     | 4070 |  875  | 0.2373    | 135.9   |

**Key reads:**

1. **t=0 f=0 ≈ nms-off** (within 2 decodes, identical composite).
   Confirms the algorithm's lower bound is effectively no-suppression.
2. **t=0 f=0 is 27% SLOWER than nms-off** (211s vs 166s) — the
   O(n²) NMS loop adds overhead with no benefit. nms-off truly is the
   right way to disable: skip the function entirely.
3. **Sharp cliff between "no NMS" and "any NMS"**: t=0 f=0 (no real
   suppression) → t=1 f=0 (suppress only if same freq bin AND time
   difference ≤ 1 step) drops from 4339 to 4098 — **a loss of 241
   decodes for the most permissive non-trivial NMS setting**.
4. **The radius doesn't matter much past that cliff.** Tightening from
   t=8 f=2 to t=1 f=0 only recovers +28 decodes (4070 → 4098).
5. **Wall-clock savings vs nms-off are modest** (15-20%) — NMS is fast
   enough that the savings don't justify losing 5-6% of decodes.

## Disposition

**SHELVED.** Production stays at `nms_enabled = false`. The radius
sweep confirms hb-019's finding from a different angle: the problem
isn't radius tuning, it's that **any non-trivial NMS suppresses real
signals on busy FT8 bands** because real adjacent stations commonly
share the same TF cell within a few steps (time-sharing a frequency,
or just close enough that Costas search produces overlapping peaks).

The hb-008 prediction (tighten radii instead of fully disabling) was
wrong: even the tightest meaningful setting loses 239 of the +267
decodes that hb-019 recovered.

The infrastructure (`nms_time_radius`, `nms_freq_radius` config
fields; CLI flags) lands as reusable. Useful for any future redesign
of the suppression algorithm.

## Learnings

- **NMS based on TF-distance is fundamentally too coarse for FT8.**
  The historical radii (t=8, f=2) suppressed too aggressively. But
  even the tightest possible non-trivial radii (t=1, f=0) suppress
  +239 real decodes vs no-NMS. The conclusion is that on busy FT8
  bands, real distinct signals frequently land in the same
  Costas-candidate TF cell — and TF-distance alone can't distinguish
  "duplicate of strong signal" from "distinct weaker signal."

- **There IS still a duplicate-suppression value to extract.** The
  novel count is highest at nms-off (1037) and lowest at the
  tightest historical NMS (875). Some of that novel-count increase
  is likely strong signals being decoded multiple times via
  near-duplicate candidates — but LDPC + CRC + dedup (via
  `seen_messages` HashSet in decoder.rs) catches those eventually.
  The wall-clock cost of letting LDPC handle dedup is small enough
  to be worth it.

- **The 27% SLOWER result for t=0 f=0 vs nms-off is a useful
  invariant.** Skipping the NMS function entirely beats setting
  radii to 0 — the function has measurable per-call overhead even
  when its body becomes a no-op. The `if self.config.nms_enabled`
  gate is the right shape.

- **Future NMS-redesign candidates:** a score-based suppression
  (drop candidates that are within N dB of a stronger candidate AND
  share a TF region) could distinguish duplicate-strong-signal
  candidates (low Δscore) from distinct-weaker-signal candidates
  (high Δscore). The current pure-TF-distance approach can't make
  that distinction. Worth a separate hypothesis if/when wall-clock
  on the operator's MiniPC becomes a real concern.

## Follow-ups added to hypothesis bank

- **hb-036 (new)** — Score-relative NMS suppression. Replace the
  TF-distance condition with: suppress candidate j if it's within
  TF radius AND its score is within N dB of candidate i's. The
  current algorithm conflates "duplicate of strong" and "distinct
  weaker"; a score-relative threshold would let only the duplicates
  through to suppression. Priority ~0.40. Estimated effort:
  1-2 sessions. Re-opens the NMS question without throwing away
  hb-019's win.

## Reproducing

```bash
for T in 0 1 2 4 8; do
    for F in 0 1 2; do
        cargo run --release -p pancetta-research --bin eval -- \
            --tier curated-hard-200 --mode ft8 \
            --nms-on --nms-time-radius $T --nms-freq-radius $F \
            --output research/scorecards/sweep/hard200-nms-t$T-f$F.json
    done
done
# Plus a baseline:
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/sweep/hard200-nms-off-baseline.json
```
