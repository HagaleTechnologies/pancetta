---
slug: adaptive-ldpc-iters
mode: ft8
state: shelved
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T00:00:00Z
branch: experiment/ft8/adaptive-ldpc-iters
parent_hypothesis: hb-022
wild_card: true
scorecard: research/scorecards/sweep/hard200-adaptive-{off,on,asym}.json
delta_vs_main: 0 (production unchanged — flag default off)
disposition: SHELVED — both directions of adaptive scheduling are net-negative or zero
---

## Hypothesis

hb-022 (wild card): use sync_score as an SNR proxy to vary LDPC
iterations per candidate. High-SNR candidates (score > 8) get fewer
iters (25) — BP "converges in 5-10 iters" per the bank entry's
intuition. Low-SNR candidates (score < 4) get more iters (100) —
"give weak signals more BP time." Mid-SNR (4-8) gets the production
default (50, post-hb-005).

Expected delta per bank: "+0.01-0.02 throughput; neutral sensitivity."

## Change

Pure research infrastructure — production behavior unchanged.

- `pancetta-ft8/src/decoder.rs` — added `Ft8Config::adaptive_ldpc_iters: bool`
  field (default false). When true, the per-thread rayon init creates
  3 LdpcDecoder instances (low/mid/high iter counts), and the
  per-candidate closure dispatches by sync_score. When false, all 3
  decoders use ctx.ldpc_iterations (production default = 50), so
  dispatch is a no-op.
- `pancetta-research/src/decoder.rs` — `with_adaptive_ldpc_iters(bool)`
  builder.
- `pancetta-research/src/bin/eval.rs` — `--adaptive-ldpc-iters` CLI flag.

## Result

**Hard-200 A/B:**

| variant                         | rec  | novel | rate    | composite | time(s) |
|---------------------------------|------|-------|---------|-----------|---------|
| off (baseline, uniform 50)      | 4325 | 1020  | 0.5043  | 0.2522    | 67.9    |
| symmetric {25, 50, 100} by SNR  | 4306 | 1033  | 0.5021  | 0.2510    | 76.2    |
| asymmetric {50, 50, 100} by SNR | 4325 | 1020  | 0.5043  | 0.2522    | 78.1    |

**Symmetric variant is a LOSS:** -19 recovered, +13 novel, -0.0012
composite, +12% wall-clock. The "cut iters for high-SNR" half hurts.

**Asymmetric variant is exactly NEUTRAL on sensitivity** but +15%
wall-clock cost: bit-identical decode counts to baseline, just slower.

Both findings clean: adaptive iteration scheduling does not help.

## Disposition

**SHELVED.** Production stays at uniform `ldpc_iterations = 50` for
all candidates (hb-005 baseline). The infrastructure
(`adaptive_ldpc_iters` config flag + per-thread 3-decoder dispatch
+ CLI flag) lands as reusable, but the flag defaults to false and
production behavior is unchanged.

## Learnings

- **Sync score is not a reliable BP convergence predictor.** The bank
  entry's intuition ("high-SNR converges fast") was wrong at the
  thresholds tested. sync_score > 8 lost decodes when iters were cut
  to 25 — many score-8+ candidates need 50+ iters. Either the
  threshold needs to be much higher (>12? >15?) or sync_score isn't
  a strong enough proxy for "BP will converge fast" at all.

- **BP that doesn't converge by 50 iters doesn't converge by 100
  iters either.** The asymmetric variant (50/50/100) spent 15% more
  wall-clock on low-SNR candidates but found ZERO additional
  truth-matched decodes. Either those candidates fall into Tanner-
  graph cycles that don't terminate, or BP has converged on the
  wrong codeword and the extra iterations just re-confirm it.

- **The hb-005 sweep already captured the LDPC-iters elbow.**
  hb-005 tested {25, 50} and showed +14 recovered for the bump. Now
  hb-022's asymmetric variant tests {50, 100} and shows 0 additional
  recovered. The 25 → 50 → 100 curve is sharply diminishing: most of
  the achievable BP wins are at ≤50 iters.

- **OSD-2 is the real fallback "extra effort" knob, not iter count.**
  When BP doesn't converge at 50, the decoder falls through to
  OSD-2 with parity gate ≤ 4. OSD does the actual heavy lifting on
  hard codewords. Pushing BP iters higher doesn't help because OSD
  is what would catch the remaining decodes — and it's already
  catching what it can.

- **Wild-card audits can also produce decisive "stop trying this"
  outcomes.** hb-019 (wild card) graduated as the biggest win since
  hb-023. hb-022 (wild card) is a clean shelve that rules out an
  entire class of optimization. Both are valuable — the shelve saves
  future cycles from re-attempting the same dead end.

## Follow-ups added to hypothesis bank

None. The result is decisive; no obvious next-step variant beyond
sync_score-based scheduling is worth pursuing (we'd need a better
BP convergence predictor, which is a research problem on its own).

If the operator ever wants to tighten the OSD path (where the real
hard-cases get caught), that's hb-034 (audit OSD-3's unconfirmed
novels) and hb-014 (parity gate sweep).

## Reproducing

```bash
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --output research/scorecards/sweep/hard200-adaptive-off.json

cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --adaptive-ldpc-iters \
    --output research/scorecards/sweep/hard200-adaptive-on.json

# Asymmetric variant: edit ADAPTIVE_ITERS_LOW from 25 → 50 in
# pancetta-ft8/src/decoder.rs, then rebuild + rerun --adaptive-ldpc-iters.
```
