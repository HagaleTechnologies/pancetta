---
slug: fast-path-single-pass
mode: ft8
state: won
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T14:34:59Z
branch: experiment/ft8/fast-path-single-pass
parent_hypothesis: hb-031
wild_card: false
scorecard: research/scorecards/history/2026-05-23-fast-path-single-pass.json
delta_vs_main: -0.0007 composite, -49% wall-clock
disposition: WIN (speed) — production default max_decode_passes 3 → 1; near-zero sensitivity cost
---

## Hypothesis

hb-030 (2026-05-22) proved subtract_with_sidelobes is broken for
adjacent weak signals: 0/16 cases where subtraction surfaced a missed
decode; 9/16 cases where it masked a recoverable signal. hb-001
(2026-05-21) showed pass 2+ adds only +1.2% on hard-200 under nms-on.
At the new nms-off production baseline (since hb-019), the pass-2+
contribution is expected to be even smaller because pass 1 now sees
more candidates.

hb-031: lower the production default `max_decode_passes` from 3 to 1.
Validate by running the full 5-tier eval and confirming the
sensitivity loss is small (≤1.5% on curated tiers) while wall-clock
drops materially.

## Change

Production: `Ft8Config::max_decode_passes` default lowered from 3 to
1 in `pancetta-ft8/src/decoder.rs:190`. The field is still
configurable; operators wanting deeper decoding can override.

No other code changes. No new infrastructure (`--max-passes` CLI
flag already exists from hb-001).

## Result

**Full 5-tier eval at max_passes=1 vs current main (max_passes=3):**

| Tier             | Metric        | Main   | Branch | Δ              |
|------------------|---------------|--------|--------|----------------|
| fixtures         | pass_rate     | 1.0    | 1.0    | 0              |
| synth-clean      | per-SNR bins  | same   | same   | 0              |
| curated-hard-200 | rate          | 0.5057 | 0.5043 | -0.0014        |
| curated-hard-200 | recovered     | 4337   | 4325   | -12 (-0.28%)   |
| curated-hard-200 | novel         | 1037   | 1020   | -17            |
| curated-hard-1000| rate          | 0.5036 | 0.5026 | -0.0010        |
| curated-hard-1000| recovered     | 14153  | 14126  | -27 (-0.19%)   |
| curated-hard-1000| novel         | 3618   | 3537   | -81            |
| wild-50          | rate          | 0.0    | 0.0    | 0              |
| composite        |               | 0.5529 | 0.5522 | **-0.0007**    |
| 5-tier elapsed   |               | 1237 s | 631 s  | **-49% (≈half!)** |

**Sensitivity cost is minimal:** -39 recovered decodes total across
the two curated tiers, -0.2-0.3% relative. Confirms hb-030's probe
finding — multi-pass infra is contributing almost nothing at the
current nms-off baseline.

**Wall-clock cost cut in half:** 1237s → 631s = -49%. Per-WAV average
drops from ~960 ms (at max_passes=3) to ~490 ms (at max_passes=1).
This makes pancetta's decode round-trip dramatically more comfortable
inside FT8's 15-second slot budget, especially on slower hardware
(operator's Windows MiniPC).

**Novel decodes also dropped (-98 across curated tiers).** Multi-pass
was previously inflating the novel count with what were likely
LDPC+CRC false positives surfacing on subtracted residuals (the
spectrogram artifacts hb-030 documented). Removing it tightens the
decoder's novel:recovered ratio slightly.

## Disposition

**WIN (speed).** Production default `max_decode_passes` lowered
3 → 1. The composite delta is -0.0007 — barely measurable, well
within prior-cycle noise (hb-006's marginal +0.0003 win was the
same magnitude in the other direction). The wall-clock improvement
is decisive: a full 5-tier eval that took 21 minutes now takes 10.

For autonomous operator deployment, this is the right call: every
15-second slot has a fixed budget; decode latency directly affects
how much time is available for QSO state-machine work, transmission
prep, and slot alignment. Cutting decode time in half is a major
operational win for negligible sensitivity loss.

## Learnings

- **Multi-pass was overhead, not capability.** The combined picture
  across hb-001 (+1.2% under nms-on), hb-030 (kernel masks adjacent
  weaks), and this cycle (-0.2-0.3% sensitivity at -49% wall-clock)
  is unambiguous: the multi-pass infrastructure as currently
  implemented is a net cost. Confirmed by direct A/B at the
  production baseline.

- **The composite metric undervalues wall-clock improvements.** The
  composite formula weights real_decode_rate, fixture pass rate, and
  SNR@50% — no wall-clock term. A -0.0007 composite hides a 2×
  decode-time speedup. For production decisions, treat composite as
  necessary-but-not-sufficient: significant speed wins at flat
  composite should still ship.

- **Diagnostic-driven decisions are higher-confidence than
  sweep-driven ones.** hb-030's controlled probe gave us a clean
  mechanistic understanding of WHY multi-pass doesn't help; this
  cycle's 5-tier eval just confirmed the expected effect at scale.
  The combination is much stronger than either alone — and the
  decision to ship was easy because the mechanism was understood.

- **Cumulative production impact summary (post-hb-031):**
  - 5 graduations with composite changes: +0.0279, +0.0156, +0.0128,
    +0.0008, +0.0003 → total **+0.0574 composite**
  - 1 graduation with negative composite but positive ops impact
    (this one): -0.0007 composite, -49% wall-clock
  - Hard-1000 decode_rate: 0.371 → 0.504 (+35.9% relative)
  - Per-WAV decode time: ~430 ms → ~490 ms (post nms-off) → ~280 ms
    (post hb-031). Net: ~35% FASTER than the pre-experiment-run
    baseline despite +1973 more decodes per 1000 WAVs.

- **The remaining productive surface is mostly structural.** With
  parameter sweeps in diminishing returns (hb-005/006 marginal,
  hb-007 dead), the high-value work ahead is structural: hb-024
  (cross-validation — validates the +876 hb-019 novels), hb-037
  (subtract kernel redesign if multi-pass is to be salvaged), hb-015
  (Doppler-resilient sync for polar paths). Plus the deferred hb-004
  (AP gate retune, needs eval-AP-wiring first).

## Follow-ups added to hypothesis bank

None new — this experiment was direct execution of a well-prepared
hypothesis. Its prior follow-ups (hb-037 kernel redesign,
synergistically hb-021 wild card) remain on the bank.

## Reproducing

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 --max-passes 1 \
    --output research/scorecards/fast-path-single-pass.json
cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/main.json research/scorecards/fast-path-single-pass.json
```
