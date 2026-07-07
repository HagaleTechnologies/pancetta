# Decoder speed overhaul — final report (Tasks 1-16)

**Date**: 2026-07-06 (docs+gate task landed 2026-07-07)
**Branch**: `worktree-decoder-speed-overhaul` (subagent-driven-development), base `main @ eaf2beab`, final `HEAD @ 0e79cb24` (this task's docs commit lands on top)
**Spec**: `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md`
**Plan**: `docs/superpowers/plans/2026-07-06-decoder-speed-overhaul.md`
**Ledger**: `.superpowers/sdd/progress.md` (task-by-task detail; this file is the consolidated summary)

This extends `research/experiments/2026-07-06-phase1-checkpoint.md` (Phase 1
only) with Phase 2/3 and the final full-suite gate. It does not repeat that
file's methodology notes — see it for the profiling-harness measurement
protocol.

## What shipped, by phase

### Phase 1 (Tasks 1-7) — mechanical/A/B micro-optimizations

| Task | Fix | Gating | Landed default | Per-task benchmark (this repo's harness fixture unless noted) |
|---|---|---|---|---|
| 1 | Profiling harness (F7) | docs | n/a | n/a — infra only |
| 2 | Lazy BP trajectory + array returns (F2) | BIT-EXACT | always on (pure refactor) | multi-thread 148.99→147.56-148.12 ms/window; single-thread 243.61→253.44 ms/window (noise-level; this fixture's BP mostly converges quickly, so the eliminated 17.4KB alloc rarely fires here) |
| 3 | OSD sort-key precompute + alloc trims (F6) | BIT-EXACT | always on (pure refactor) | single-thread avg 255.6→251.5 ms/window (**~1.6%**), tighter run-to-run variance |
| 4 | Flatten spectrogram to contiguous storage (F3) | BIT-EXACT | always on (pure refactor) | multi-thread ~146.3→~140.5 ms/window (**~4%**); single-thread ~256.3→~243.6 ms/window (**~5%**) |
| 5 | Padé `fast_atanh` (F1) | A/B | `pade_atanh=true` (piecewise: Padé for `\|x\|≤0.95`, exact `ln` above — the *raw* unconditional Padé failed its own hard-200 gate first, rec Δ=-12 CI[-20,-5]) | hard-200: rec Δ=+0 (CI[-4,+4]), novel Δ=-11 (CI[-22,-2], fewer FPs), elapsed 58.3s→49.0s (**-16.0%**) |
| 6 | `costas_half_loop_disabled` flip attempt (F5) | A/B | **stays `false`** — re-confirmed Batch 92's original finding; the plan's own text asserting Batch 92 supported the flip was factually wrong (Batch 92 said the opposite) | hard-200 re-check: rec Δ=-58 (CI[-80,-39]) — real recall regression, correctly declined |
| 7 | f32 real-input FFT + f32 spectrogram (F4) | A/B | `SpecScalar=f32` w/ `realfft::RealFftPlanner`; coherent-subtract path kept f64 arithmetic over f32 storage | hard-200: rec Δ=-8 (CI[-22,+3], not significant), novel Δ=-35 (improvement), elapsed 51.96s→49.96s (**-3.8%**); this-fixture: multi-thread 114.0→109.9 ms/window (-3.6%), single-thread 214.5→212.3 ms/window (-1.0%, noise-level) |

**Phase 1 checkpoint benchmark** (both binaries built at their respective
commits, A/B'd back-to-back on the same loaded shared dev machine to cancel
ambient noise — see `2026-07-06-phase1-checkpoint.md` for full methodology):

| Config | Baseline (`main@eaf2beab`) | Phase 1 complete | Δ |
|---|---:|---:|---:|
| Multi-thread | 144.01 ms/window | 107.18 ms/window | **-25.6%** |
| Single-thread (`RAYON_NUM_THREADS=1`) | 246.43 ms/window | 189.63 ms/window | **-23.1%** |

This is *less* than the plan's originally-hoped ~60-100ms (≈60-75% reduction)
estimate — that estimate assumed the stacked effect of every optimization
under a workload that stresses BP/OSD/FFT harder than this harness's single
8-message fixture does. The corpus-level (hard-200) per-task deltas above
(Padé -16.0% elapsed, f32 FFT -3.8% elapsed) are consistent with, and a lower
bound relative to, the measured single-fixture number — Phase 2's anytime
staging is where the larger remaining win (skip low-value work outright,
rather than only making each unit of work cheaper) was expected to land.

### Phase 2 (Tasks 8-12) — budget-governed anytime decoder

Not [A/B]-gated for a speed number the way Phase 1 was (Tasks 8, 9, 11, 12
are [BIT-EXACT] infrastructure — a `DecodeBudget` type, floor/rest candidate
split S1/S2, budget checkpoints on stages S4-S7, and the coordinator-side
`decode_effort_budget_ms` atomic wiring — none of which change production
decode behavior while the budget stays at its `0`/unlimited default). The one
perf-relevant Phase 2 measurement is Task 10's BP escalation ladder (S3) A/B
gate:

| | control (flat 100 iters) | variant (floor 25 / deep 100, `escalation_parity_max=30`) |
|---|---:|---:|
| composite (saturation-aware) | 0.3021 | 0.3019 |
| decode_rate | 0.6237 | 0.6232 |
| wall time (curated-hard-200, repeated runs) | 54.8-55.0s | 52.2-52.4s (**-4.9%**) |

Bootstrap CI (n=1000): `rec` Δ=-1 CI[-3,0] (not significant, borderline
pass), `novel` Δ=+2 CI[0,+5] (not significant). Recall gate passes; elapsed
gate is real but weak (-4.9%, well short of Tasks 5/7's double-digit wins).
Root-caused via a `--escalation-parity-max=0` ceiling probe: cutting BP's
iteration cap alone (no escalation at all) already buys the same ~5%, meaning
BP iteration count is a small fraction of total decode cost for this decoder
(Costas sync + FFT dominate) — escalating smarter on top of the floor cut
adds almost nothing. **`escalation_enabled` correctly stays `false`** —
data-driven non-flip, same pattern as Task 6. See
`research/experiments/2026-07-06-bp-escalation-ladder.md` for the full
histogram/threshold analysis.

**Known limitation carried forward** (inert while `escalation_enabled=false`,
flagged for whoever revisits this): the escalation ladder escalates *inline,
in candidate sync-score-rank order*, not via the plan's originally-envisioned
global collect-all-failures/sort-by-promise/escalate-most-likely-first
design. If ever enabled under real time pressure, this could spend budget on
an early low-promise candidate while skipping a later near-certain one.

### Phase 3 (Tasks 13-15) — operator control surface

`[decoder]` config section (`effort: DecodeEffort`, `budget_ms: Option<u64>`),
`preset_budget_ms(effort, tier)` mapping (Eco=1ms, Standard=250ms, Deep=1000ms,
Max=0/unlimited, Auto=tier-derived), tier-probe seeding at coordinator startup
+ probe completion (subsuming the old ad-hoc per-tier `Ft8Config` rewrite
hack), and the live `e` TUI keybinding (`CycleDecodeEffort`, cycle order
Eco→Standard→Deep→Max→Auto→Eco) with a status chip. No new perf numbers here —
this phase makes the Phase 2 budget mechanism operator-controllable, it
doesn't change the decode algorithm itself.

**Known limitation**: config hot-reload does not re-seed the effort budget —
verified genuinely infeasible today (the coordinator's `ConfigHotReload` file
watcher is a documented, deliberate no-op — never constructed anywhere in
`pancetta`'s coordinator crate, so a config edit to `[decoder]` only takes
effect on restart). The `e` key is the only *live* control.

## Final full-suite gate (Task 16, this task)

Run 2026-07-07 on `HEAD @ 0e79cb24` (no code changes since — docs only):

```
cargo test --workspace --features transmit
```
Every crate's `test result: ok` line showed `0 failed` (workspace lib/bin/integration/doc-test suites, ~90 result blocks in total). Largest individual suites: `pancetta-ft8` lib 441 passed, `pancetta-qso` lib 318 passed (plus 10 integration-test binaries totaling ~97 more), `pancetta-tui` lib 267 passed, `pancetta` lib 329 passed, `pancetta-config` lib 140 passed, `pancetta-agent` lib 94 passed (plus 3 integration-test binaries totaling 53 more), `pancetta-dx` lib 84 passed, `pancetta-core` lib 87 passed. No linker-cache issue was hit this run (Task 15's `cargo clean -p pancetta-config -p pancetta-ft8` workaround was not needed).

```
cargo test -p pancetta --test loopback_qso
```
`test result: ok. 14 passed; 0 failed; 0 ignored`

```
cargo test -p pancetta-hamlib --lib -- --test-threads=1
```
`test result: ok. 27 passed; 0 failed; 0 ignored`

No regressions. Decode counts on every WAV fixture have been unchanged from
the pre-Phase-1 baseline at every BIT-EXACT task's own verification point
(see the ledger); this final run doesn't re-diff fixture-by-fixture decode
counts since no decode-affecting code changed since Task 15 — this task is
docs + a final regression gate on the already-verified state.

## Remaining before declaring the plan's success criteria fully met

**On-air soak** (operator-gated, per spec §7.5): run `deep` vs `auto` effort
sessions on live traffic and compare decode counts + telemetry (`decode.budget`
log lines, `DecodeBudgetReport.stages` skip counts) before calling this done
end-to-end. Everything else in the plan (Tasks 1-16) is complete and merged
to this branch.
