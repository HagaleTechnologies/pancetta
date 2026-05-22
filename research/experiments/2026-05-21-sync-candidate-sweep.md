---
slug: sync-candidate-sweep
mode: ft8
state: won
created: 2026-05-21T00:00:00Z
last_updated: 2026-05-22T00:11:50Z
branch: experiment/ft8/sync-candidate-sweep
parent_hypothesis: hb-003
wild_card: false
scorecard: research/scorecards/history/2026-05-22-sync-candidate-sweep.json
delta_vs_main: +0.0128 composite
disposition: WIN — production default raised from 100 to 200, graduate to main
---

## Hypothesis

After hb-001 ruled out pass count as the WSJT-X gap source, the next
sweep targeted candidate count. `MAX_SYNC_CANDIDATES = 100`
(decoder.rs:63, formerly a hard const) caps the Costas sync candidates
that survive into NMS + LDPC. If the 101st-ranked candidate is a real
signal on a busy band, it's silently dropped before LDPC even runs.

Per hb-003, two sub-experiments:
- (a) Raise `max_sync_candidates` only.
- (b) Raise both `max_sync_candidates` and `max_candidates`.

Expected: +0.02 to +0.06 real decode rate on hard-200.

## Change

**Production change:** raised `MAX_SYNC_CANDIDATES` from 100 to 200 in
`pancetta-ft8/src/decoder.rs:63`. This is the new production default.

Supporting plumbing (research infra):
- `pancetta-ft8/src/decoder.rs` — promoted `MAX_SYNC_CANDIDATES` from a
  hard const to `Ft8Config::max_sync_candidates: usize`. The const is
  still the documented source of truth for the default. costas_sync_search
  reads from config.
- `pancetta-research/src/decoder.rs` — added `with_max_sync_candidates(n)`
  and `with_max_candidates(n)` builder methods.
- `pancetta-research/src/bin/eval.rs` — added `--max-sync-candidates`
  and `--max-candidates` CLI flags (mirrors the hb-001 `--max-passes`
  pattern).
- `pancetta-ft8/examples/enhanced_spectral_analysis.rs` — switched from
  exhaustive struct literal to `..Ft8Config::default()` so future
  field additions don't break the example.

## Result

**hard-200 sub-experiment (a) — max_sync_candidates only:**

| sync_cap | recovered | novel | rate    | composite | wall-clock |
|----------|-----------|-------|---------|-----------|------------|
| 50       | 3140      | 460   | 0.3661  | 0.1831    | 65.7 s     |
| 100      | 3832      | 676   | 0.4468  | 0.2234    | 87.7 s     |
| 150      | 4004      | 802   | 0.4669  | 0.2334    | 121.2 s    |
| 200      | 4051      | 868   | 0.4724  | 0.2362    | 134.4 s    |
| 250      | 4068      | 897   | 0.4743  | 0.2372    | 147.7 s    |
| 300      | 4072      | 929   | 0.4748  | 0.2374    | 161.0 s    |

Sharp diminishing returns past 200: +17 decodes from 200 → 250 (+12 s
wall-clock), +4 decodes from 250 → 300 (+13 s). **sync_cap=200 is the
right operating point** — captures 92% of the headroom (vs 300) for
83% of the wall-clock cost.

**Sub-experiment (b) — raise both caps:** identical to (a) at every
matching sync value. `max_candidates=100` was never the binding
constraint; the sync cap was. Sub-experiment (b) added no new
information.

**Full 5-tier eval at sync_cap=200 vs main:**

| Tier             | Metric        | Main   | Branch | Δ          |
|------------------|---------------|--------|--------|------------|
| fixtures         | pass_rate     | 1.0    | 1.0    | 0          |
| synth-clean      | SNR@50% (dB)  | -20.0  | -20.0  | 0          |
| synth-clean      | each bin      | identical | identical | 0       |
| curated-hard-200 | decode_rate   | 0.4468 | 0.4724 | +0.0255    |
| curated-hard-200 | recovered     | 3832   | 4051   | +219       |
| curated-hard-200 | novel         | 676    | 868    | +192       |
| curated-hard-1000| decode_rate   | 0.4214 | 0.4402 | +0.0188    |
| curated-hard-1000| recovered     | 11843  | 12372  | +529       |
| curated-hard-1000| novel         | 2314   | 2779   | +465       |
| wild-50          | decode_rate   | 0.0    | 0.0    | 0          |
| composite        |               | 0.5234 | 0.5362 | **+0.0128** |

**+748 additional real decodes** across the two curated tiers; +657
additional novel decodes. **No regressions** — fixtures pass identically,
synth-clean is bit-identical at every SNR bin, no new false positives
in either of those guard tiers. Wall-clock cost: ~+52% per WAV on the
hard tiers, well within the 3-second per-WAV budget (decoder budget at
osd_depth=2 is 2000 + 2*500 = 3000 ms, observed average 672 ms/WAV).

## Disposition

**WIN. Production default raised from 100 to 200.** All five tiers
either improved or were unchanged. No regression flags. Composite
delta +0.0128 puts the post-hb-003 baseline at 0.5362.

Sweep tooling (`--max-sync-candidates`, `--max-candidates` flags +
builder methods) lands as reusable infrastructure for future hypotheses.

## Learnings

- **hb-003 was right where hb-001 was wrong.** Same sweep shape, different
  parameter — and this one delivered +5.7% real decode rate on hard-200
  (vs hb-001's +1.2%). The "5-10% of WSJT-X" gap is partly a sync-cap
  issue: the 101st-300th-ranked Costas candidates contained ~6% of the
  real decodes pancetta was missing.

- **Past sync_cap=200 there are very real candidates but they're not
  recoverable.** Sync=300 finds 240 more decodes than sync=100, but only
  21 of those come from raising the cap past 200. Either NMS is merging
  them with stronger neighbors before LDPC sees them, or LDPC + OSD
  can't converge on candidates ranked 201-300 within the time budget.
  Both worth a follow-up.

- **The "decoder budget" is generous.** Wall-clock cost rose 52% on hard
  tiers but the per-WAV budget tracker (3000 ms at osd_depth=2) was
  never close to firing — average per-WAV time was ~672 ms at sync=200.
  This means there's room to raise OTHER expensive knobs (OSD depth,
  LDPC iterations) without budget concerns.

- **Sub-experiment (b) was redundant.** `max_candidates=100` was never
  binding — the sync cap was. The bank entry's hypothesis (a) vs (b)
  framing assumed both could be limits independently; in practice (a)
  subsumes (b) because the sync cap gates everything downstream.

- **Novel/recovered split shifts toward recovered as decode_rate rises.**
  Default: 676 novel / 3832 recovered = 17.6% novel ratio. At sync=200:
  868/4051 = 21.4%. More candidates → more "found" messages, but the
  novel ratio increases slightly. Some of the +192 novel on hard-200 may
  be FPs not caught by jt9 — strengthens the case for hb-024 (cross-
  validation against JTDX) now that there's a bigger pool of novel
  decodes to vet.

- **`enhanced_spectral_analysis.rs` is now `..Default::default()`-shaped.**
  Caught it via the hb-003 compile error — the exhaustive struct literal
  was a maintenance pothole. The same pattern likely applies to other
  example/test/bench Ft8Config construction sites; hb-032 already
  covers cleanup for the dead aggressive_decoding field that runs through
  several of them, so leaving the rest for that follow-up.

## Follow-ups added to hypothesis bank

- **hb-033 (new)** — Audit why sync_cap=300 only adds 21 decodes over
  sync_cap=200. Are the candidates ranked 201-300 mostly merged by NMS
  before LDPC, or do they fail LDPC + OSD at higher rates than the
  top-200? Spectrogram/candidate-rank dumps on a busy-band WAV would
  answer. Priority ~0.45. Estimated effort: 1 session.

## Reproducing

```bash
# Sub-experiment (a):
for N in 50 100 150 200 250 300; do
    cargo run --release -p pancetta-research --bin eval -- \
        --tier curated-hard-200 --mode ft8 \
        --max-sync-candidates $N \
        --output research/scorecards/sweep/hard200-sync-$N.json
done

# Full 5-tier confirmation at winner:
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 --max-sync-candidates 200 \
    --output research/scorecards/sync-candidate-sweep.json

cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/main.json research/scorecards/sync-candidate-sweep.json
```
