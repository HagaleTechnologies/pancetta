---
slug: multipass-profile
mode: ft8
state: profile-only
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T00:00:00Z
branch: experiment/ft8/multipass-profile
parent_hypothesis: hb-021 (wild card)
wild_card: true
scorecard: (n/a — profile-only, no production change)
delta_vs_main: 0
disposition: SHELVE hb-021 — profile shows the optimization target it
  was framed around (spectrogram + sync re-computation in pass 1) is
  <1% of runtime. The real multi-pass cost is time-domain subtract on
  pass 0 (43%), but pass 2's recall yield is so small (0.5%) that
  speeding subtract up still wouldn't justify keeping multi-pass on.
---

## Hypothesis

hb-021 (wild card from the original 2026-05-13 bank): "Rewrite
`subtract_with_sidelobes` in frequency domain. Avoid recomputing the
spectrogram between passes by mutating the spectrogram directly. Could
significantly speed up multi-pass."

This profile-only variant: measure where pass time actually goes
(preprocess, spectrogram, sync, LDPC, subtract) so we can decide
whether the freq-domain rewrite is worth pursuing — BEFORE writing
the code.

Motivation: hb-031 already disabled multi-pass by default
(`max_decode_passes = 1`) because pass 2 added very few decodes per
unit time. hb-021 might revive multi-pass IF subtract is the
bottleneck AND freq-domain implementation would help. Profile-first.

## Change

Test-only — instrumented `Decoder::decode_window` with `eprintln!`
timing for each stage of the multi-pass loop. Ran eval on
curated-hard-200 with `--max-passes 2`, captured stderr, aggregated.
Instrumentation reverted before commit (verified with `git diff`).

Instrumentation captured per-pass:
- preprocess_audio
- compute_spectrogram
- costas_sync_search
- LDPC parallel decode (par_iter)
- subtract_with_sidelobes (only when pass+1 < max_passes)

## Result

### Per-pass time breakdown (200 WAVs, max_passes=2)

| Stage              | Pass 0 avg | Pass 1 avg | Pass 0 share |
|--------------------|-----------:|-----------:|-------------:|
| preprocess         |       0 ms |       0 ms |          0 % |
| spectrogram        |     5.5 ms |     5.3 ms |        0.4 % |
| sync_search        |    11.8 ms |    11.4 ms |        0.9 % |
| LDPC (parallel)    |   699   ms |    31.6 ms |       55   % |
| subtract           |   547   ms |       0 ms |       43   % |
| **pass total**     | **1266 ms**|    53   ms |              |

Pass 0 ran on every WAV (200/200, 5575 total new decodes).
Pass 1 pre-decode ran on every WAV (200/200), but only 27 of those WAVs
produced any new candidates that survived dedup against pass-0 results.
Total new decodes from pass 1: **28** (out of 5575 from pass 0 → 0.5% recall lift).

### Wallclock totals

- Pass 0 across all WAVs:               253.2 s
- Pass 1 pre-decode (always paid):        3.3 s
- Pass 1 post-decode work (27 WAVs):      1.4 s
- **Pass-0 subtract (multi-pass overhead):  109 s** (43% of pass 0)

In other words, the time-domain `subtract_with_sidelobes` call on
pass 0 — which only exists to prepare residual_samples for pass 1 —
eats 43% of pass-0 wallclock.

### What hb-021 would actually save

hb-021's framing: "freq-domain subtraction avoids recomputing the
spectrogram between passes."

The spectrogram + sync_search cost on pass 1 is **16.7 ms × 200 WAVs
= 3.3 seconds** — about **1.3% of total runtime**. Even if a
freq-domain subtract made pass-1 pre-decode entirely free, the upper
bound speedup is ~1.3%.

The actual bottleneck is the **pass-0 time-domain subtract** (~547 ms
per WAV, 109 s total). hb-021 framed as "freq-domain subtract" could
plausibly attack this, but only if the operation is genuinely cheaper
in freq domain. Sidelobe-aware subtraction in freq domain still
requires convolving with sidelobe coefficients across a width-3 freq
window over 79 symbols × N decoded messages per WAV. Not free.

## Disposition

**SHELVE hb-021.** Three reasons:

1. **The framing was wrong.** "Avoid recomputing the spectrogram" is a
   1.3% optimization. The real hot spot is the time-domain subtract.
2. **The recall headroom is tiny.** Pass 2 adds 0.5% recall (28 of
   5575 decodes). Even if subtract were free, the cost/benefit of
   keeping `max_passes >= 2` doesn't change much — pass 0 alone
   accounts for 99.5% of the decode yield.
3. **hb-031 already made the right call.** Production runs
   `max_decode_passes = 1` and skips the subtract entirely. Pass 0 on
   200 WAVs with max_passes=1 would take ~720 ms × 200 = **144 s
   instead of 253 s** — a 43% speedup vs the (now-disabled) multi-pass
   configuration. We already harvested that speedup.

## Learnings

- **Profile before rewriting.** Two cycles of bank entries (hb-021,
  hb-037) assumed multi-pass speedups would come from avoiding
  spectrogram recompute. The actual hot spot is somewhere else
  entirely.
- **Pass-0 subtract is a multi-pass tax.** When `max_passes=1`, no
  subtract runs; when `max_passes=2`, pass 0 pays 547 ms of subtract
  even though pass 1 yields only 28 new decodes. This is the single
  largest argument for keeping `max_passes=1` (hb-031).
- **LDPC parallel decode dominates pass 0** (55%, 699 ms/WAV). If
  there is *any* future speedup target, this is it — but parallel
  candidate evaluation already uses rayon, so the remaining cost is
  per-candidate LDPC BP iteration. That's hb-034's territory (OSD
  audit) and the historical hb-014 (parity gate tightening).
- **Pre-decode (preprocess + spectrogram + sync_search) is ~17 ms total
  per pass.** Tiny. No optimization target here.

## Follow-ups added to hypothesis bank

- **hb-021 → CLOSED (SHELVED).** Update bank: original framing was
  wrong, opportunity is small.
- **hb-037 → CLOSED (SHELVED).** Same root cause — hb-037 was
  "share spectrogram between passes," another 1.3% optimization.
- **No new hypothesis spawned.** Pass-0 LDPC is the dominant cost
  but is already parallel; further LDPC speedups should come from
  the existing hb-014 / hb-034 / OSD-tuning track, not from
  subtract optimization.

## Reproducing

(Instrumentation removed; reproduce by re-instrumenting decoder.rs
with the snippets recorded above, then:)

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --max-passes 2 \
    --output /tmp/profile.json 2> /tmp/profile.log
grep -c hb021_profile /tmp/profile.log  # ~628
```
