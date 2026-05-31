---
slug: corpus-survey-2026-05-30
mode: ft8
state: survey
created: 2026-05-30T20:00:00Z
last_updated: 2026-05-30T20:10:00Z
branch: iter/2026-05-30-corpus-survey
parent_hypothesis: corpus-refresh (decoder iters saturating against hard-200/1000; new wild data needed)
wild_card: false
scorecard: n/a (survey, not an experiment)
delta_vs_main: n/a
disposition: INGEST — refresh-mix hard-200 (top-100 today + top-100 existing) + create wild-100 from today's full session for production tracking. Today's WAVs are ~20% denser (more decodes/slot) than existing hard-200 and exhibit marginal-SNR + cross-band-spanning structure the existing corpus lacks. See "Recommendation" below.
---

## Why this survey

The decoder has had 11 graduations in 6 days against the existing
hard-200/1000 corpus. Wins are getting smaller — today's hb-068 was
+0.000292 composite, hb-016 was a shelve. The recall ceiling is
saturating. Fresh real-world WAVs from a different operating context
(K5ARH 20m late-May 2026, ~8.6 h continuous capture) should either:

- refresh hard-N with harder/different cases (resets the recall ceiling
  → future iters earn meaningful composite movement), or
- reveal the existing corpus is representative (decoder is at a real
  ceiling on this signal class), or
- surface marginal-SNR / fading / multi-station structure (the V2
  sweet spot the existing hard-200 lacked per V2 diagnostic 0%
  marginal-neighbor finding).

This entry is the survey result, not an experiment in itself. The
ingestion work (rebuild hard-200 with mixed source set, create wild-100)
is follow-up.

## Step 1 — Metadata pass

- **Total WAVs today**: 2066
- **Total size**: 710 MB (~360 KB each, all 15s × 12 kHz mono i16
  slot-aligned)
- **Time span**: 15:23 → 23:59 UTC (8.6 hours continuous; hour 15
  partial at 146 files, hours 16-23 all exactly 240 files each →
  perfectly continuous, no gaps > 60 s)
- **File size distribution**: 99.95% are full 360 044 bytes; the
  first capture (15:23:56) was 303 404 bytes (initial partial slot,
  pancetta startup mid-slot). No clipped or anomalous files.
- **All passed the >300 KB threshold** — no partials filtered out.

## Step 2 — Sampled jt9 + pancetta baseline

Sampled **20 WAVs** (16 evenly-spaced through the 8.6 h + 4 random
extras), ran both pancetta-ft8@HEAD (main) and jt9 (WSJT-X `-8` mode)
on each. Total wall-clock: **59.3 s** (jt9 is fast on 15 s slot
inputs without slot-cut overhead).

Raw per-WAV output: `research/corpus/surveys/2026-05-30/per_wav.json`.
Aggregate summary: `research/corpus/surveys/2026-05-30/summary.json`.

| slot HHMMSS | panc | jt9 | int | p-only | jt9-only | calls | snr_min | snr_med | freq_span |
|-------------|------|-----|-----|--------|----------|-------|---------|---------|-----------|
| 152356      | 17   | 16  |  9  |  8     |  7       | 45    | -24     | -9      | 502-2752  |
| 155758      | 28   | 27  | 21  |  7     |  6       | 72    | -25     | -12     | 368-2978  |
| 163228      | 29   | 35  | 24  |  5     | 11       | 88    | -25     | -12     | 473-2888  |
| 170658      | 26   | 27  | 18  |  8     |  9       | 78    | -19     | -5      | 247-2489  |
| 173828      | 27   | 32  | 23  |  4     |  9       | 73    | -25     | -11     | 327-2789  |
| 174113      | 18   | 17  | 13  |  5     |  4       | 48    | -24     | -9      | 445-2578  |
| 175458      | 26   | 32  | 21  |  5     | 11       | 80    | -25     | -13     | 316-2846  |
| 181543      | 20   | 23  | 17  |  3     |  6       | 56    | -24     | -8      | 240-2687  |
| 185013      | 35   | 37  | 25  | 10     | 12       | 91    | -25     | -14     | 316-2936  |
| 192428      | 26   | 29  | 18  |  8     | 11       | 80    | -25     | -5      | 327-2828  |
| 194858      | 27   | 27  | 18  |  9     |  9       | 76    | -18     | -1      | 327-2508  |
| 195858      | 35   | 31  | 26  |  9     |  5       | 90    | -17     | -4      | 236-2983  |
| 203328      | 26   | 32  | 23  |  3     |  9       | 73    | -25     | -4      | 327-2843  |
| 210743      | 28   | 27  | 20  |  8     |  7       | 76    | -25     | -9      | 202-2787  |
| 214213      | 30   | 32  | 22  |  8     | 10       | 84    | -20     | -8      | 352-3009  |
| 221643      | 27   | 30  | 20  |  7     | 10       | 74    | -21     | -5      | 287-2770  |
| 224528      | 22   | 24  | 18  |  4     |  6       | 62    | -25     | -15     | 256-2941  |
| 225058      | 24   | 24  | 17  |  7     |  7       | 72    | -25     | -5      | 287-2589  |
| 232528      | 27   | 38  | 21  |  6     | 17       | 91    | -19     | -5      | 223-2768  |
| 235958      | 29   | 26  | 18  | 11     |  8       | 79    | -21     | -5      | 327-3046  |

Notes:
- `int` = decodes whose message text appears in BOTH pancetta and jt9.
- `p-only` / `jt9-only` = symmetric differences (after message-text dedup).
- `calls` = unique callsigns recovered across both decoders' decodes in
  that slot (per a naive 3-7 char A-Z0-9 regex; over-counts ~10% on
  contest serials but stable for comparison).

## Step 3 — Characterization

### Density comparison: today vs existing hard-200

| metric                       | hard-200 main  | today (20-WAV sample) | delta            |
|------------------------------|----------------|-----------------------|------------------|
| pancetta_decode_count min    | 19             | 17                    | –                |
| pancetta_decode_count p10    | 20             | 18                    | –                |
| pancetta_decode_count median | 22             | 27                    | **+5 (+23%)**    |
| pancetta_decode_count p90    | 26             | 30                    | **+4 (+15%)**    |
| pancetta_decode_count max    | 36             | 35                    | ≈                |
| pancetta_decode_count mean   | 22.5           | 26.4                  | **+3.9 (+17%)**  |

**Today's WAVs are meaningfully denser** at the 50th and 90th percentile.
The max overlaps (existing hard-200 already has its densest slots).
The new corpus moves the **median density** up significantly, which is
the regime where decoder iterations live (most slots, not extremes).

Density matters because:
- More decodes per slot = more candidates competing in NMS + LDPC
- More mutual masking (a strong nearby signal can corrupt a weaker
  one's LLRs)
- This is exactly the regime hb-086 V2 (soft-cancellation) is designed
  to attack, and the regime that hb-064 (TEP pruning) needs to be
  evaluated against

### Pancetta-vs-jt9 agreement

- pancetta total decodes: **527** across 20 slots (mean 26.4/slot)
- jt9 total decodes: **566** across 20 slots (mean 28.3/slot)
- intersection: **392** (69.3% agreement against max)
- pancetta-only: 135 (likely pancetta-true-positives jt9 missed, plus
  some pancetta FPs — without ground truth we can't separate, but
  given pancetta's 0-FP track record on hard-200, mostly TPs)
- **jt9-only: 174 across 20 slots = 8.7/slot avg** — these are jt9
  decodes pancetta missed. **Real recall headroom of ~30%** (174/566)
  on this signal class.

For comparison, on existing hard-200, pancetta hits ~123.7% of
ft8_lib's recall (project status); against jt9 on TODAY's WAVs the
ratio is 93% (527/566). The decoder is well-tuned to the
existing hard-200 (which was curated _by_ pancetta) and proportionally
weaker on this new operator-specific capture.

### Callsign diversity

- **1488 unique-callsign mentions** across the 20 sampled slots
  (~74/slot avg, peak 91 at 18:50 and 23:25 UTC)
- Naive callsign regex inflates this slightly (counts callsign tokens
  twice when a station appears in both pancetta and jt9 with slightly
  different surrounding messages) — but the relative diversity is
  high. Existing hard-200's per-WAV callsign diversity is not tracked,
  but observed sample callsigns include `KZ4GN`, `KE2DC`, `AE1MV`,
  `W1AW/0`, `KC1VXF`, `AC9HP`, `K0DTM`, `W5RY`, `KK7O`, `N5VAN`,
  `NC7C`, `KB3DQI`, `W3LR`, `KA1VDZ`, `KF8EBV`, `KN6KBS`, `AC6SC`,
  `KE8EOQ`, `N4OHI`, `WY0V`, `KE9END`, `NG7E`, `W3MTN`, `KC1LMY`,
  `N0SMX`, `VE3XN`, `SP6FME`, `KC8EHR`, `VA5KEN`, `A61CK`, `IK4LZH`,
  ... — heavy NA + scattered DX (UAE, Italy, Poland, Canada). One
  `W1AW/0` SES (special event station) spotted. No obvious POTA/SOTA
  in this sample (would need to grep `P/`, `MOTA/`, etc.).

### Message-type mix

- **CQ: 151** (21.5% of unique-text messages)
- **Report (R-NN / -NN / +NN): 166** (23.7%)
- **RR73 / RRR / 73: 68** (9.7%)
- **Other (callsign-callsign exchanges, grid responses): 316** (45.1%)

Healthy QSO mix — operators are completing exchanges, not just spamming
CQs. The 23.7% report and 9.7% RR73 fractions indicate the band is
**productive**, not just busy.

### Temporal variability (early vs late session)

| half             | panc/slot | jt9/slot | inter/slot | calls/slot | snr_median |
|------------------|-----------|----------|------------|------------|------------|
| early (15-19 UTC) | 25.2      | 27.5     | 18.9       | 71         | -9 dB      |
| late  (19-24 UTC) | 27.5      | 29.1     | 20.3       | 78         | -5 dB      |

**Classic 20 m evening band improvement signature** — density up
~10%, callsign diversity up ~10%, median SNR up 4 dB late-session.
Greyline opening signature would be a localized spike at one specific
time-of-day, which this sample doesn't show (it's broader band
improvement). Curating a "k5arh-greyline" tier is **not** justified
from this data.

### SNR characteristics

- **Lowest jt9 SNR seen: -25 dB** (jt9's FT8 floor) — observed in
  9 of 20 slots
- **Median-of-medians SNR: -8 dB** (vs hard-200's mean-decoded-SNR
  p50 = +0.7 dB)
- Today's WAVs reach the marginal-SNR floor far more often than
  existing hard-200 average

This is significant: hb-086 V2's residual-wall diagnostic (2026-05-27)
showed **0% marginal-neighbor decodes in the top-20 hard-200 hardest
WAVs** — the residual wall there is dominated by clean isolated
decodes, not marginal cases. Today's WAVs DO have marginal cases.
V2 (soft cancellation) and hb-064 (TEP pruning) both expect to win
in the marginal regime. **Ingesting today's WAVs would create the
correct test environment for V2 and hb-064.**

## Step 4 — Curation recommendation

**RECOMMEND: ingest as refresh-mix hard-200 + wild-100.**

### What to do

1. **Rebuild hard-200 as a refresh-mix** (preserves existing best
   cases, adds today's hardest):
   - Score all 2066 today's WAVs with the same `interest_score`
     formula as the existing manifest (pancetta_decode_count +
     mean_decoded_snr_db component + noise_floor_db component)
   - Pick today's top-100
   - Pick existing hard-200's top-100 (by existing interest_score)
   - Merge → new `hard_200.manifest.json` with `generated_at = today`
   - **Old manifest preserved** under
     `history/hard_200.2026-05-20.manifest.json` for diff-vs-baseline
     accountability in journal entries
2. **Create `wild_100.manifest.json`** from today's full 2066 WAVs:
   - Random sample of 100 (stratified by hour to avoid time-of-day
     bias)
   - Tracks production performance on K5ARH-typical conditions
     across a full operating session (the wild-50 was a small sample
     from much earlier captures; wild-100 is bigger and more recent)
3. **Do NOT** create a "k5arh-greyline-NN" specialty tier — the
   temporal variability is too smooth to justify a separate phenomenon
   tier.

### Why refresh-mix instead of full replacement

If we replaced hard-200 outright with today-only, the **6 days of
graduations would have their scorecard baselines invalidated** —
"composite +0.013993 cumulative" loses meaning. The 50/50 mix:
- Keeps half the existing baseline → composite numbers stay comparable
  with a tracked offset
- Brings in the harder/denser/marginal-SNR cases V2 and hb-064 need
- One-time recall ceiling reset (the next iter against new hard-200
  will show a step-change, which is the goal)

### Cost estimate

- **Score 2066 WAVs with pancetta-ft8 to compute interest_score**:
  pancetta-ft8 takes ~1-3 s per WAV in release mode → **~30-100 min**
  serial. Existing `pancetta-research --bin curate` should be the
  vehicle (need to verify it accepts a recordings-dir scan; if not,
  a small follow-up script).
- **jt9 baseline for the top-100 today's selection** (needed for
  novel-decode scoring per the harness convention):
  ~jt9 takes ~3-15 s per slot; 100 WAVs serial = **~5-25 min**.
  Plus another 100 for wild-100 = another 5-25 min.
- **Total compute budget for ingestion**: ~40-150 min. Reasonable
  for a one-shot corpus refresh.
- **Storage budget**: WAVs stay in `~/.pancetta/recordings/` (NOT
  in repo). Manifest JSON adds ~50 KB. Baselines JSON adds ~100
  WAVs × ~5 KB = 500 KB. Negligible.

### Follow-up plan

A separate iter (`iter/2026-05-31-corpus-refresh-ingestion` or
similar) executes the ingestion:
1. Build (or extend) a `curate --source-dir ~/.pancetta/recordings
   --date 20260530 --top-n 100 --output today_top100.manifest.json`
2. Merge today_top100 + existing_top100 → new hard_200 manifest
3. Archive old hard_200 manifest under `history/`
4. Generate jt9 baselines for new manifest entries
   (`baseline` binary, batch-mode)
5. Re-run `eval` on main to establish the new scorecard baseline
6. Update `research/scorecards/main.json` with new composite (will be
   different from current 0.569415 — that's the point)
7. Commit + journal

Then resume normal hypothesis iteration with the refreshed corpus.
hb-086 V2 and hb-064 should both be evaluated FIRST against the
refreshed corpus to see whether V2's diagnostic kill-switch
(78.3% pair-likely on existing hard-200) shows the same value, and
whether the marginal-neighbor 0% finding holds.

## Blockers encountered

None. Survey ran clean (no jt9 errors, no panicking WAVs, no
file-not-found from rolling-cap deletion). Today's recording session
appears to have been a stable continuous capture.

## Artifacts

- `pancetta-research/examples/corpus_survey_2026_05_30.rs` — one-shot
  survey tool, ~330 LOC
- `research/corpus/surveys/2026-05-30/summary.json` — aggregate
- `research/corpus/surveys/2026-05-30/per_wav.json` — per-WAV detail
