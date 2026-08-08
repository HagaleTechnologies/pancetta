---
slug: ft8lib-sync-seed-union
mode: ft8
state: shelved
created: 2026-08-08T00:00:00Z
last_updated: 2026-08-08T00:00:00Z
branch: PAN-7
parent_hypothesis: ft8_lib's candidate finder may contribute Costas positions that pancetta's native sync search misses
wild_card: false
scorecard: research/scorecards/pan7/
delta_vs_main: fixed cap 200 and raised cap 400 both produced exact seeded-vs-control nulls on all four tiers; curated ΔTP=0 and Δunverified-novels=0 with 95% CI [0,0], noise remained 1/1000 in both arms, and synth recovery was unchanged
disposition: DECLINE. Keep Ft8Config::ft8lib_sync_seeds_enabled false.
---

# ft8_lib candidate-seed union — declined

## Hypothesis

Run ft8_lib's MIT-licensed `monitor_*` → `ftx_find_candidates` pipeline over pass-0 audio, translate its positions into pancetta's spectrogram coordinates, re-score them with pancetta's Costas metric, and union them before the candidate cap. The production default remains off unless a neutral-truth A/B gate shows a real recall gain without a false-positive cost.

## What I found

The implementation works, but the mechanism is structurally inert. At cap 200 and cap 400, every seeded scorecard is result-identical to its cap-matched control:

| Tier | Cap | Control TP | Seeded TP | ΔTP | Control unverified | Seeded unverified | Δunverified |
|---|---:|---:|---:|---:|---:|---:|---:|
| hard-jt9-rich-200 | 200 | 1485/2919 | 1485/2919 | 0 | 0 | 0 | 0 |
| hard-jt9-rich-200 | 400 | 1502/2919 | 1502/2919 | 0 | 0 | 0 | 0 |
| curated-hard-200 | 200 | 1251/2001 | 1251/2001 | 0 | 225 | 225 | 0 |
| curated-hard-200 | 400 | 1269/2001 | 1269/2001 | 0 | 228 | 228 | 0 |
| synth-pair-200 | 200 | 275/360 | 275/360 | 0 | — | — | — |
| synth-pair-200 | 400 | 276/360 | 276/360 | 0 | — | — | — |

On `noise_1000`, all four arms decoded the same known false positive in 1/1000 WAVs. Thus seeded-vs-control ΔFP is 0 at both caps (the absolute baseline is 1, not a healthy zero).

The seed-survival tests and `S0-ft8lib-seed` counters explain the null: native exhaustive Costas search already contains every representable translated position, and exact-key deduplication removes the duplicate. Doubling the cap does not admit a ft8_lib-only position on the busy fixture either. This is “nothing admitted,” not evidence that admitted seeds failed downstream.

Four ceilings bound the result:

1. At both measured caps the native sweep saturates the same lattice, so translated seeds do not survive deduplication as novel positions.
2. Negative-dt candidates are dropped, never clamped. One third of ft8_lib's time range is therefore outside this experiment; the slot-edge hypothesis remains untested.
3. Seeds below the native threshold can reach AP0, but `MIN_SYNC_SCORE_FOR_AP` excludes them from AP1–AP4.
4. The novel classifier loads `*.ft8lib.json` alongside jt9 baselines, biasing seeded-arm novel verification in the mechanism's favour. The binding jt9 ΔTP result is unaffected.

## TDD evidence

Phase 1 pins FFI seed ordering/budget, coordinate identity, negative-row rejection, scope/envelope filtering, grid compatibility, and real-signal alignment. Phase 2 pins default-off byte identity, stub parity, and scorecard plumbing. Phase 3 pins below-threshold admission, cap enforcement, pass-0-only attribution, scope safety, seed counters, and budget-stage observability. `cargo test --features transmit -p pancetta-ft8` passed 535 unit tests plus all integration suites; `pancetta --test loopback_qso` passed 14/14.

## A/B workflow

Each tier was evaluated independently with `eval`, using explicit `--ft8lib-sync-seeds` / `--no-ft8lib-sync-seeds` arms at `max_sync_candidates` 200 and 400. All sixteen scorecards are under `research/scorecards/pan7/`; the eight tier-for-tier `compare --bootstrap` transcripts and exit codes are in `compare-results.txt`.

### Results (git sha `0574f727`, all arms)

- Fixed cap: all four comparisons exited 0. Curated recall and novel bootstrap deltas were exactly 0 with 95% CI `[0,0]`.
- Raised cap: all four comparisons exited 0, again with exact `[0,0]` curated deltas.
- `curated-hard-200` control reproduced 2001 truths as required.
- Scorecards record `git.dirty=true` because a resumed phase worker and this worker wrote the same intended, untracked `research/scorecards/pan7/` directory concurrently. All arms identify the same committed SHA, configs match their filenames, and no source file was dirty during measurement; the provenance blemish is retained rather than rewritten.

## Decision

The standing gate is: **“ΔTP ≥ threshold with bootstrap CI excluding zero, ΔFP-on-noise = 0, Δunverified-novels ≤ 2× ΔTP.”**

- **FAIL — TP gain:** ΔTP is 0 at both caps; CI is `[0,0]` and does not exclude zero.
- **PASS — incremental noise FP:** seeded and control both produce 1/1000, so ΔFP-on-noise is 0.
- **PASS — unverified novels:** Δunverified is 0, equal to the `2 × ΔTP` allowance.

The first binding clause fails. `ft8lib_sync_seeds_enabled` stays `false` unconditionally.

## Full test suite

- `cargo test --features transmit -p pancetta-ft8`: passed before measurement.
- `cargo test -p pancetta --test loopback_qso`: 14 passed, 0 failed.
- `cargo fmt --check`: passed after formatting the phase-3 test.
- Final workspace, clippy, and formatting gates are recorded in the phase completion transcript.

## Files changed

- `pancetta-ft8/src/ft8_lib_ffi.rs`, `pancetta-ft8/src/decoder.rs`, and `pancetta-ft8/tests/ft8lib_seed_tests.rs`: seed extraction, translation, gated union, attribution, counters, and tests.
- `pancetta-research/src/decoder.rs`, `pancetta-research/src/bin/eval.rs`, and `pancetta-research/tests/decoder_smoke.rs`: measurement plumbing.
- `research/scorecards/pan7/`: sixteen scorecards plus eight comparison transcripts.
- This experiment record, the companion design spec, and the config/platform decision digest.

## Learnings / follow-ups

Candidate-set union cannot recover positions already exhaustively searched on the same lattice. A credible revisit must target the genuinely different negative-dt/slot-edge region with representable time padding, partial-Costas scoring, a slot-edge-specific corpus, and a paired cap arm. That is separate scope.
