# Matched-demod fine-sync re-measured under a real, bounded DecodeBudget (Task W3.3b)

**Date**: 2026-07-08
**Branch**: `worktree-decoder-tp-sensitivity`
**Cross-references**: `research/experiments/2026-07-08-w33-matched-demod-fine-sync.md` (Task
W3.3 — the unbudgeted `DecodeBudget::unlimited()` measurement this task re-runs under real
production effort presets).
**Status**: [HARNESS] shipped (new `--effort` flag, default-unchanged verified). [A/B]
re-measured under `--effort standard` (primary) and `--effort eco` (secondary). **Decision
unchanged from W3.3: `Ft8Config::fine_sync_enabled` stays `false`.** The primary (Standard,
250ms) re-measurement is a clear DECLINE — recall actually *regresses* under a realistic bounded
budget, not merely a smaller gain. The secondary (Eco, 1ms) re-measurement is a genuine, clean
WIN on every gate criterion, but the flag is a single global default and Standard represents the
more common effort configuration, so the global flip is still declined. See "Decision" below for
the full reasoning and the resulting new, budget-regime-dependent picture.

## What this is

Task W3.3 measured the matched-demod fine-sync stage's cost/benefit under
`DecodeBudget::unlimited()` — the harness convention used for every A/B in this plan — and found
a strong, clean recall win (+54 TP on hard-200) but declined to flip the default because elapsed
cost was ~2.9x/2.1x. W3.3's own "DecodeBudget integration" analysis argued (but did not measure)
that a REAL bounded `DecodeBudget` should self-limit this cost automatically, since the
pre-existing S1-floor/S2-rest candidate split already gates per-candidate dispatch on
`current_budget.has_time()`.

This task builds the missing harness capability (a `--effort` flag that constructs a real,
bounded `DecodeBudget`) and re-runs the exact same hard-200/noise_1000 A/B under it, to test that
hypothesis for real instead of leaving it as an untested architectural argument.

## Harness change [HARNESS]

**Where `DecodeBudget` was constructed today**: every eval-harness entry point
(`pancetta-research/src/decoder.rs::Ft8Decoder::decode_wav`) called the plain
`decode_window`/`decode_window_with_ap` methods, which internally set
`self.current_budget = DecodeBudget::unlimited()` before decoding (see
`pancetta-ft8/src/decoder.rs::decode_window`/`decode_window_with_ap_scoped_partner`). There was no
CLI-reachable way to pass a real, bounded budget. `pancetta-ft8` already exposes the budgeted
entry points needed (`decode_window_budgeted`, `decode_window_with_ap_scoped_partner_budgeted`,
both returning `(Vec<DecodedMessage>, DecodeBudgetReport)`) — these were built in the
decoder-speed-overhaul plan (Task 8/12) and were simply never wired into the research harness.
`acceptance_calibration.rs` (the other binary named in this task's brief as worth checking) does
not construct a `DecodeBudget` at all — it uses `Ft8Decoder::with_default_config()` directly and
is unaffected by/unrelated to this change.

**Production preset→ms mapping — reuse vs. re-derivation**: the production mapping lives at
`pancetta/src/coordinator/effort.rs::preset_budget_ms(effort: DecodeEffort, tier: HardwareTier) ->
u64` (`Eco`=1, `Standard`=250, `Deep`=1000, `Max`=0/unlimited, `Auto`=tier-derived). This function
is `pub(crate)` inside the `pancetta` binary crate, and its `coordinator` module tree (and the
`effort` submodule specifically) is private (`mod effort;`, not `pub mod`) — not reachable from
outside that crate at all today. `pancetta-research`'s `Cargo.toml` depends only on
`pancetta-ft8`, `pancetta-qso`, and a handful of small utility crates; adding a dependency on the
full `pancetta` crate to reach this one 4-arm match statement would also pull in axum, tokio,
tokio-tungstenite, cpal, the hamlib FFI bindings, the TUI crate, etc. — a materially larger
dependency-graph change than a harness-only CLI flag warrants, and exactly the "genuinely cannot
reuse without a much larger restructuring" case the task brief names as grounds for the documented
fallback. Per that fallback: `pancetta-research/src/bin/eval.rs::effort_preset_budget_ms` is a
small, explicitly-documented LOCAL RE-DERIVATION of just the four preset constants, with a doc
comment citing the production source and a "KEEP IN SYNC" note. `auto` is mapped to the `Fast`
assumption (1000ms) — mirroring the coordinator's own pre-probe "innocent until proven otherwise"
startup convention (`coordinator/tier.rs`) — since the eval harness has no live hardware-tier
probe and probing would make scorecards host-dependent/non-reproducible.

**New surface**:
- `pancetta-research/src/decoder.rs`: `Ft8Decoder::budget_ms: Option<u64>` (default `None`),
  builder `with_effort_budget_ms(Option<u64>)`, private `decode_budget()` helper (`None`/`Some(0)`
  → `DecodeBudget::unlimited()`; `Some(ms)` with `ms > 0` → a FRESH `DecodeBudget::until(Instant::now()
  + Duration::from_millis(ms))`, built at each `decode_wav` call — a budget is a wall-clock
  deadline, not a fixed instant computed once at CLI-parse time, so it must be recomputed per WAV).
  All four decode call sites in `decode_wav` (chrono-replay, rolling-window, explicit-AP-context,
  and the default no-AP path) now route through the budgeted entry points, discarding the
  `DecodeBudgetReport` (not needed by this task; a natural follow-up if per-stage budget telemetry
  is ever wanted in scorecards).
- `pancetta-research/src/bin/eval.rs`: `--effort <eco|standard|deep|auto|unlimited>` CLI flag
  (default: flag omitted → `Args::effort_budget_ms = None` → `with_effort_budget_ms` is never even
  called → `Ft8Decoder::budget_ms` stays at its own default of `None`, byte-for-byte the same as
  every existing invocation), full plumbing (struct field, parse arm, help text, apply site in
  `build_decoder_from_args`).

### Regression check — default (flag omitted) reproduces prior results exactly

Re-ran the exact command from W3.3's control (`--tier curated-hard-200 --mode ft8`, flag omitted)
and compared against the previously-committed `research/scorecards/w33-fine-sync-hard200-control.json`
via `compare`:

```
A: research/scorecards/w33-fine-sync-hard200-control.json (sha c37e7f80, score 0.3126)
B: <fresh re-run on this worktree, sha e137b1de, score 0.3126 +0.0000>

REGRESSIONS:
  (none)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200          rec Δ=+0  (95% CI [+0.0, +0.0], n_bootstrap=1000) — NOT significant
  curated-hard-200          novel Δ=+0  (95% CI [+0.0, +0.0], n_bootstrap=1000) — NOT significant
```

`Δ=+0` with a collapsed `[+0.0, +0.0]` CI is the strongest possible confirmation: not just the
same aggregate score, but literally identical per-WAV decode sets. (The `sha` differs only because
the committed baseline predates this task's commits; the score/composite/decode counts are
unaffected by this task's code changes when the flag is omitted, which is what's being verified.)
This is decisive: the new flag adds a code path that is provably inert unless explicitly invoked.

## Re-measurement — same corpus/methodology as W3.3

Exact same tiers, same `curated-hard-200` truth manifest, same `noise_1000` manifest, same
`fine_sync_enabled` on/off toggle, same `compare` bootstrap-CI tooling (n=1000, seed 0xb007) as
W3.3 — only the added `--effort <preset>` flag differs.

### Primary: `--effort standard` (250ms) — matches production's `Auto` preset on a Moderate-tier host

**Commands**:
```bash
./target/release/eval --tier curated-hard-200 --mode ft8 --effort standard \
    --output research/scorecards/w33b-standard-hard200-control.json
./target/release/eval --tier curated-hard-200 --mode ft8 --effort standard --fine-sync-enabled \
    --output research/scorecards/w33b-standard-hard200-variant.json
./target/release/compare research/scorecards/w33b-standard-hard200-control.json \
    research/scorecards/w33b-standard-hard200-variant.json

./target/release/eval --tier noise_1000 --mode ft8 --effort standard \
    --output research/scorecards/w33b-standard-noise-control.json
./target/release/eval --tier noise_1000 --mode ft8 --effort standard --fine-sync-enabled \
    --output research/scorecards/w33b-standard-noise-variant.json
./target/release/compare research/scorecards/w33b-standard-noise-control.json \
    research/scorecards/w33b-standard-noise-variant.json
```

**hard-200 headline**:

| | control | variant (fine_sync_enabled) | Δ |
|---|---:|---:|---:|
| composite (saturation-aware) | 0.3147 | 0.3089 | **-0.0058** |
| decode_rate | 0.6252 | 0.6137 | **-0.0115 (flagged REGRESSION by `compare`)** |
| truth_decodes_recovered (of 2001) | 1251 | 1228 | **-23** |
| novels_verified | 3130 | 3104 | -26 |
| novels_unverified | 1773 | 1768 | -5 |
| elapsed (wall, 200 WAVs) | 45.7s | 67.3s | +21.6s (~1.47x) |

Bootstrap CI (n=1000, seed 0xb007):
- `rec` Δ=**-23**, 95% CI **[-48, +1]** — NOT significant, but **trending negative** (nearly
  excludes zero on the unfavorable side)
- `novel` Δ=-31 (aggregate verified+unverified), 95% CI [-67, +8] — NOT significant, also
  trending negative

`compare`'s own regression scan flags `decode_rate -0.0115` as a REGRESSION (WINS: none).

**noise_1000**: control 0 false positives (elapsed 278.2s), variant 0 false positives (elapsed
617.5s, ~2.22x). FP-on-noise gate: **clean** (0→0, no per-wav bootstrap CI on this tier — same
harness characteristic noted in W3.3, `noise_1000` scorecards carry no `per_wav_records`).

**Gate-by-gate under Standard**:
1. Bootstrap-CI recall delta excludes zero, in favor — ❌ **NOT MET**. Worse than "doesn't clear":
   the point estimate is **negative** (Δ=-23), not just an insignificant positive trend. This is
   the opposite of what the "self-limiting" hypothesis predicted.
2. Δunverified-novels ≤ 2×ΔTP — N/A / moot: ΔTP itself is negative, so there is no favorable
   recall gain for this term to bound.
3. FP-on-noise = 0 new decodes — ✅ **MET** (0→0, actually better than W3.3's unbudgeted 1→1).
4. Reasonable elapsed cost — mixed: 1.47x on hard-200 is far more reasonable than W3.3's 2.9x, but
   this is moot given criterion 1 fails outright.

**Why the hypothesis was wrong, not just overstated**: W3.3's architectural argument was that a
real budget would cause *fewer* S2-rest candidates to be attempted (each one now more expensive),
trading some of the recall gain for a smaller cost multiplier — i.e., a smaller but still-positive
number. The measured mechanism is harsher: `DecodeBudget::has_time()` is checked only *between*
candidate dispatches, not mid-candidate (by design — see `pancetta-ft8/src/budget.rs` and W3.3's
own note that neither the legacy fallback nor the new stage has an internal mid-candidate budget
check). So when `fine_sync_enabled` is on, ONE expensive matched-demod attempt (FIR + grid search +
BP, the ~2.9M-multiply-add-per-candidate cost W3.3's "Cost accounting" section measured) can by
itself consume most or all of a 250ms per-window budget, which then starves EVERY subsequent
S2-rest candidate in that window of any attempt at all — including candidates that would have
decoded quickly and cheaply via the legacy path's fast-reject behavior. The net effect is fewer
total candidates get ANY decode attempt in the window, not just a smaller number of expensive ones
— an actual reduction in decode opportunities relative to control, which explains the observed
recall regression. This is a genuine, structural interaction between "one stage's cost is very
lumpy per-candidate" and "the budget gate is coarse-grained (checked only between candidates)" —
not a bug in this task's harness wiring.

### Secondary: `--effort eco` (1ms, floor-only) — matches production's `Auto`/`Eco` preset on a Slow-tier host

**Commands**: identical shape, `--effort eco` in place of `--effort standard`.

**hard-200 headline**:

| | control | variant (fine_sync_enabled) | Δ |
|---|---:|---:|---:|
| composite (saturation-aware) | 0.1735 | 0.1802 | **+0.0067** |
| decode_rate | 0.3428 | 0.3563 | **+0.0135 (WIN)** |
| truth_decodes_recovered (of 2001) | 686 | 713 | **+27** |
| novels_verified | 1612 | 1710 | +98 |
| novels_unverified | 925 | 969 | +44 |
| elapsed (wall, 200 WAVs) | 8.1s | 6.5s | **-1.6s (variant is FASTER, ~0.80x)** |

Bootstrap CI (n=1000, seed 0xb007):
- `rec` Δ=**+27**, 95% CI **[+15, +40]** — **significant**, excludes zero, in favor
- `novel` Δ=+142 (aggregate), 95% CI [+116, +169] — significant

Unverified-novel term: Δunverified=+44, ΔTP=+27, allowance=2×27=54 → **44 ≤ 54, passes.**

**noise_1000**: control 0 false positives (elapsed 191.1s), variant 0 false positives (elapsed
**60.0s — 3.2x FASTER**, not slower). FP-on-noise gate: clean (0→0).

**Gate-by-gate under Eco — every criterion clears**:
1. Bootstrap-CI recall delta excludes zero, in favor — ✅ **MET**: [+15, +40].
2. Δunverified-novels ≤ 2×ΔTP — ✅ **MET**: 44 ≤ 54.
3. FP-on-noise = 0 new decodes — ✅ **MET**: 0→0.
4. Reasonable elapsed cost — ✅ **MET, trivially**: variant is measurably FASTER than control on
   both tiers, not just "acceptable."

**Why Eco behaves so differently from Standard**: under a 1ms (floor-only) budget, S2-rest never
runs *at all* (the floor is unconditional by design — Task 9's guarantee that recall never drops
below the always-decoded top-ranked candidates) — so the ENTIRE Standard-preset failure mode above
(one expensive candidate starving the rest of the window) cannot happen: there is no "rest" to
starve. The only candidates in play are the floor set, which both control and variant process
identically except for what happens when a floor candidate's cheap first try fails. There, the
comparison is legacy's up-to-21-trial LDPC fallback vs. fine_sync's single FIR+grid-search+BP
attempt. On the `noise_1000` tier specifically, nearly every floor candidate is spurious (noise, no
real signal), so the legacy fallback exhausts all 21 trials without ever succeeding — the single
most expensive case for the legacy path — while fine_sync spends one (large but singular) FIR
application and gives up, which is measurably cheaper in aggregate over ~1000 failing WAVs (60.0s
vs 191.1s). On `curated-hard-200`, the same floor-only subset also nets a real, significant recall
gain (+27 of the full unbudgeted +54 — plausible given W3.3's own recall mechanism is real and
some of it lives in the floor-candidate population).

## Decision

**`Ft8Config::fine_sync_enabled` stays `false` in production.** No change from W3.3.

The critical fact governing this decision: `fine_sync_enabled` is a single GLOBAL boolean default,
not conditioned on the active effort preset or hardware tier. This task's own brief named
`--effort standard` (250ms) as the PRIMARY re-measurement — deliberately, because `Standard` is
what production's `Auto` preset resolves to on a `Moderate`-tier host, and is also the preset an
operator would pick explicitly for a mid-range machine. Under that primary, representative
configuration, the flag does not merely fail to clear the gate by a small margin — it makes recall
**worse** (Δ=-23, and `compare`'s own regression scanner flags `decode_rate` as a regression). A
global flip would therefore make the common/likely production configuration measurably worse,
even though it would help the `Eco`/`Slow`-tier configuration measurably (a genuine, clean win
that clears every gate criterion, `Δ=+27` and *faster*, not slower).

This resolves the exact measurement gap W3.3 flagged as its most promising follow-up ("measure
under a REAL bounded DecodeBudget... might reveal the practical tradeoff is much better (or much
worse) than the unlimited-budget numbers suggest") — and the honest answer is: **both**,
depending on which budget regime is active, and in the opposite direction from what the
architectural hypothesis predicted for the regime that matters most for a global default.

**This is not a dead end — it reframes the mechanism's real shape**: the recall win is real (W3.3
proved the end-to-end TDD case, and both the unbudgeted and Eco-budgeted corpus runs confirm a
significant, sizeable gain), but its cost is not uniformly "expensive," it is *lumpy per-candidate
and budget-regime-dependent*: harmless-to-beneficial when the S2-rest split never engages (Eco) or
never runs out of time (unlimited, modulo the raw 2.9x multiplier), and actively harmful under a
mid-size budget where one expensive candidate can consume the entire per-window allowance and
starve everything after it. Two concrete paths forward, NOT built here (both are W3.1/W3.2/budget-
architecture design changes, out of this task's scope per its own brief and W3.3's before it):
1. **Condition `fine_sync_enabled` (or an equivalent per-candidate policy) on the ACTIVE
   `DecodeEffort`/budget regime** — e.g. only enable it when the budget is `Eco` or `unlimited`,
   or expose a per-candidate time cap so one candidate can never consume the WHOLE window's
   remaining budget. This is the natural next design step and directly informed by this task's
   data (not present in W3.3, which only had the unlimited number).
2. **A cheaper per-candidate fine_sync implementation** (narrower FIR / smaller margin / coarser
   grid — W3.3's already-identified path 2, still out of scope here) would reduce the "one
   candidate eats the whole budget" failure mode directly, independent of path 1.
3. Tying back to the S3 escalation-ladder's known, already-flagged gap (BP escalation escalates in
   candidate-rank order, not by promise/likely-success) — the SAME class of problem (a coarse,
   rank-order-only budget-consumption policy penalizing whichever candidate happens to be tried
   first) shows up here too. A future budget-allocation redesign addressing one may be able to
   address both.

## Full test results

- `cargo test --workspace --features transmit`: **93 `test result: ok` blocks, 0 `FAILED`, 0
  panics** across the full workspace (lib + integration + doctests, every crate).
- `cargo fmt -p pancetta-research -- --check`: clean (no diff).
- `cargo clippy -p pancetta-research --release --bin eval --bin compare -- -D warnings`: clean.
- `cargo clippy -p pancetta-research --release --lib -- -D warnings`: clean.
- `cargo clippy -p pancetta-research --release --all-targets`: pre-existing warnings only, in
  unrelated example binaries (`batch35_coverage_combined.rs`, `batch49_sync_tuning.rs`,
  `batch84_ship_validation.rs`, `batch42_wild.rs`, `batch66_hb244_real_corpus.rs`,
  `batch91_hb250_failed_candidates.rs`) — none in `decoder.rs`/`eval.rs`, none introduced by this
  task.

## Files changed

- `pancetta-research/src/decoder.rs`: `Ft8Decoder::budget_ms: Option<u64>` field (+ `None`
  default in `with_default_config`), `with_effort_budget_ms` builder, private `decode_budget()`
  helper, all four `decode_wav` decode call sites routed through the budgeted
  `pancetta_ft8::Ft8Decoder` entry points (`decode_window_budgeted`,
  `decode_window_with_ap_scoped_partner_budgeted`).
- `pancetta-research/src/bin/eval.rs`: `Args::effort_budget_ms: Option<u64>` field, `--effort
  <eco|standard|deep|auto|unlimited>` parse arm + help text, `effort_preset_budget_ms` (the
  documented local re-derivation of the production preset→ms mapping), apply site in
  `build_decoder_from_args`.
- `research/scorecards/w33b-standard-hard200-{control,variant}.json`,
  `research/scorecards/w33b-standard-noise-{control,variant}.json`,
  `research/scorecards/w33b-eco-hard200-{control,variant}.json`,
  `research/scorecards/w33b-eco-noise-{control,variant}.json` (new) — full corpus data.
- `research/experiments/2026-07-08-w33b-budgeted-fine-sync-remeasurement.md` (this file, new).

## Self-review

- **Verified (not assumed) the new flag's default is byte/metric-identical to today's
  `unlimited()` convention?** Yes — re-ran W3.3's exact control command with the flag omitted and
  diffed against the previously-committed scorecard via `compare`: `Δ=+0` with a collapsed `[+0.0,
  +0.0]` bootstrap CI (identical per-WAV decode sets, not just identical aggregate score).
- **Bounded-budget re-measurement using the SAME corpus/methodology as W3.3?** Yes — same
  `curated-hard-200` tier, same `noise_1000` tier/manifest, same `fine_sync_enabled` on/off
  toggle, same `compare` tool with the same bootstrap seed (0xb007, n=1000). Only the new
  `--effort` flag differs from W3.3's exact commands.
- **Flip/decline decision honestly driven by the actual new numbers?** Yes — the primary (Standard)
  re-measurement shows an outright recall regression, which is reported plainly as such (not
  softened), and the decision explains exactly why a genuinely positive secondary (Eco) result
  does not override it (single global default vs. per-regime data).
- **Full test suite green?** Yes — confirmed above, 93 test-result blocks all `ok`, 0 failures.

## Issues / concerns for whoever picks this up next

- The `DecodeBudgetReport` returned by the budgeted entry points is discarded in the harness
  wrapper (`_report` bindings). Surfacing per-stage budget telemetry (candidates attempted vs.
  skipped, `budget_exhausted`) into the scorecard would make the "one candidate starves the rest
  of the window" mechanism directly observable per-WAV rather than inferred from aggregate
  elapsed/recall deltas as done here. Not built — out of this task's stated scope (build the flag,
  re-measure, report), flagged as a natural harness enhancement.
- `--effort deep` (1000ms) was not measured — the brief named Standard as primary and Eco as the
  time-permitting secondary; Deep (and the tier-derived `auto` on a `Fast` host, which resolves to
  the same 1000ms) sit between Standard and unlimited and were not empirically checked. Given the
  clear negative trend already visible at Standard, a Deep measurement seems unlikely to change
  the global decision, but this is inference, not measurement — flagged for anyone who wants the
  complete cross-preset picture before acting on path 1 in the "Decision" section above.
- The "one candidate eats the whole window's budget" mechanism (this task's central finding) was
  inferred from the measured aggregate numbers plus the pre-existing architectural facts
  documented in W3.3 and `pancetta-ft8/src/budget.rs` (budget checked only between candidates, no
  per-candidate internal check) — it was not verified via a controlled, mid-candidate-timing
  micro-benchmark. The inference is well-supported (it's the only mechanism consistent with both
  the Standard regression AND the Eco win/speedup existing simultaneously) but a direct
  instrumentation-level confirmation was not built in this task.
