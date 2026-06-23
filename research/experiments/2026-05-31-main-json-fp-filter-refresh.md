---
slug: main-json-fp-filter-refresh
mode: ft8
state: complete
created: 2026-05-31T20:00:00Z
last_updated: 2026-05-31T20:00:00Z
branch: iter/2026-05-31-hard-1000-fix
parent_hypothesis: methodology hygiene — land the actionable follow-ups from the 2026-05-31 hard-1000 novel-explosion investigation
wild_card: false
scorecard: research/scorecards/main.json (refreshed in this iter)
delta_vs_main: n/a (this IS the new main.json)
disposition: COMPLETE — no decoder behavior change; canonical main.json now generated with `--fp-filter-baselines research/baselines/ft8`, scorecard schema records `fp_filter_active`, RUNBOOK recipe updated.
---

## Summary

Operational-hygiene change, not a decoder change. Lands the three
actionable follow-ups from the hard-1000 novel-explosion investigation
(`research/experiments/2026-05-31-hard-1000-novel-investigation.md`):

1. **Scorecard schema** — new `config.fp_filter_active: bool` field, so
   future cross-scorecard diffs can detect the methodology shift the
   investigation surfaced directly instead of by archaeological deduction.
2. **RUNBOOK canonical recipe** — the canonical `main.json` invocation
   now passes `--fp-filter-baselines research/baselines/ft8`. This
   matches the methodology every graduation sweep in the 5-day session
   lineage has used and preserves apples-to-apples comparability with
   the OLD baseline (`research/scorecards/history/main.2026-05-30-pre-refresh.json`).
3. **main.json baseline refresh** — re-ran the canonical recipe and
   overwrote `research/scorecards/main.json`. Expected to be composite-
   invariant (the composite formula reads recall, not novels; the FP
   filter is recall-invariant by construction) and to cut hard-1000
   `novel_decodes` from 6320 → ~3000-3300, restoring lineage continuity.

## Why option (A)

The investigation gave the operator a choice between (A) make canonical
main.json always FP-filter-on, or (B) make it always FP-filter-off.
This iter takes (A). Reasons (copied from the investigation):

- The 5-day session lineage of graduation deltas is FP-filter-on. Don't
  break that chain — every hb-068, hb-072, hb-075, hb-079, hb-080,
  hb-086 V1 graduation gate was measured FP-filter-on.
- Production pancetta already runs a callsign-continuity FP-filter
  analog; main.json should measure the production-relevant regime, not
  the raw decoder dump.
- All future graduation gates work without re-baselining if the
  reference is FP-filter-on too.

## Before / After (hard-1000 + hard-200)

Pre-refresh main.json (`config.fp_filter_active: null` — schema didn't
yet record it; eval invocation omitted the FP filter flag):

| tier | rec | novel | composite | fp_filter_active |
|---|---:|---:|---:|---:|
| hard-200 | 4942 | 1970 | — | (unrecorded) |
| hard-1000 | 14993 | 6320 | 0.5791 | (unrecorded) |
| elapsed | — | — | 1338s | — |

Post-refresh main.json (this iter; FP filter ON):

| tier | rec | novel | composite | fp_filter_active |
|---|---:|---:|---:|---:|
| hard-200 | 4942 | 1024 | — | true |
| hard-1000 | 14987 | 3053 | 0.5791 | true |
| elapsed | — | — | 1417s | — |

Investigation prediction: composite unchanged within ±0.001 (the
composite formula is recall-driven; FP filter is recall-invariant),
hard-1000 novel drops to ~3000-3300, hard-200 novel drops to ~1024.

**Measured (all predictions hit):**

- Composite: 0.5791144244888738 → 0.5791144244888738 (Δ = 0.0000000;
  byte-identical to 13 decimal places; confirms FP filter is fully
  recall-invariant on this corpus).
- hard-1000 novel: 6320 → 3053 (Δ = -3267, predicted 3000-3300 — hit;
  matches OLD lineage's 3038 within rayon noise).
- hard-200 novel: 1970 → 1024 (Δ = -946, predicted ~1024 — exact; matches
  the sibling hb-086 V3 agent's independent FP-on cross-check of 1024,
  see investigation §5).
- hard-1000 rec: 14993 → 14987 (Δ = -6; well within ±10 rayon noise).
- hard-200 rec: 4942 → 4942 (Δ = 0; stable).
- Elapsed: 1338s → 1417s (+5.9%; rayon thread variability — eval also
  built the fp-filter reference set this run, ~1.3K baseline JSON loads).

## Decoder behavior change

None. This is harness-side filtering applied AFTER decoder output.

Two minor non-behavioral diffs in the new scorecard's
`config.decoder` snapshot, both unrelated to this iter:

1. New keys `joint_residual_sync_relax_db: 0.0` and
   `joint_residual_sync_window_bins: 8`. These came from an in-flight
   decoder branch (hb-064 session 2 work) whose tree happened to host
   the eval binary used for this refresh. Both are no-op at their
   defaults; rec/composite land within rayon noise of the pre-refresh
   run, which corroborates the no-op claim empirically.
2. `git.branch` records `iter/2026-05-31-hb-064-session2` because that
   was the worktree HEAD when eval ran. The intended branch for the
   refresh is `iter/2026-05-31-hard-1000-fix` (this branch). The
   `head_sha` (`e6a1594`) is recorded; future archaeology can map it
   back.

Net: the only intentional `config{}` change is `fp_filter_active:
(absent) → true`.

## Files touched

- `pancetta-research/src/scorecard.rs` — `ConfigInfo` gains
  `fp_filter_active: bool` with `#[serde(default)]` for back-compat.
- `pancetta-research/src/bin/eval.rs` — populates the new field from
  the resolved `fp_filter: Option<FpFilter>`.
- `pancetta-research/tests/{compare_smoke,schema_roundtrip}.rs` —
  struct-literal sites updated to set the field explicitly to `false`.
- `docs/RUNBOOK.md` — "Decoder Research Iteration Loop" section,
  canonical main.json recipe + a paragraph explaining the convention.
- `research/scorecards/main.json` — re-run with FP filter ON.

## Verification

- `cargo build --release -p pancetta-research --bin eval` — clean.
- `cargo test -p pancetta-research --lib` — 23 passed.
- `cargo test -p pancetta-research --tests` — schema_roundtrip and
  compare_smoke both green; serde back-compat verified (the new
  `#[serde(default)]` lets old scorecards still load — they parse as
  `fp_filter_active: false`, which is accurate for the pre-2026-05-31
  canonical recipe).
- `cargo fmt && cargo clippy -p pancetta-research --no-deps` — the
  touched files raise no new lints. (One pre-existing
  `unnecessary_map_or` in `pancetta-research/src/corpus.rs:35` is
  unrelated to this iter and lives on `main`.)
- Post-refresh main.json `config.fp_filter_active == true`: verified.
- Pre vs post composite delta: 0.0000000 (acceptance: within ±0.001 —
  passed with margin to spare; numerically identical to 13 decimal
  places).

## Decision

LANDED. Branch `iter/2026-05-31-hard-1000-fix`. Three commits, no
pancetta-ft8 production code touched. Cumulative 5-day session lineage
preserved: future iters compare against this refreshed main.json and
inherit the methodology stamp via `config.fp_filter_active`.
