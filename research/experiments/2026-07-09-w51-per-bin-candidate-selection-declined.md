---
slug: w51-per-bin-candidate-selection
mode: ft8
state: shelved
created: 2026-07-09T00:00:00Z
last_updated: 2026-07-09T00:00:00Z
branch: worktree-decoder-tp-sensitivity
parent_hypothesis: decoder-tp-sensitivity plan Task W5.1 (spec — flat top-200
  candidate cap starves weak signals in quiet bins when a crowded neighboring
  bin dominates the global rank)
wild_card: false
delta_vs_main: curated-hard-200, n=200 WAVs — recall Δ=-22 (95% CI [-36,-8],
  significant), novel Δ=-50 (95% CI [-86,-12], significant), decode_rate
  0.6252 → 0.6142 (-0.0110). Both recall AND novel decodes drop together —
  not a precision/recall tradeoff, a net loss.
disposition: DECLINE the default flip. `Ft8Config::per_bin_candidate_selection`
  stays `false`. Mechanism is correctly implemented and unit-proven (TDD
  reproduces the exact crowded-cluster-vs-isolated-weak-cell bug it targets
  and shows the fix), but on the real corpus it costs more true positives
  than it recovers. Root cause (hypothesis, not exhaustively re-verified):
  a per-bin K=2 cap is strictly MORE aggressive than the flat
  top-`max_sync_candidates` cap in the common (non-crowded) case, where the
  flat cap rarely binds at all — so the mechanism actively discards
  legitimate near-threshold candidates (Costas correlation sidelobes /
  freq_sub splitting of one real signal) in ordinary, uncrowded bins for a
  win that only pays off in the specific pathological crowded-single-bin
  scenario the task brief was written to fix.
---

## Hypothesis

Task W5.1 (spec): the Costas sync search's candidate-collection sweep
(`costas_sync_search_with_threshold_and_partner`) collects every above-threshold
`(t0, f0, freq_sub)` cell into one flat list, sorts by `sync_score`, and
truncates to the global `max_sync_candidates` cap (200). This is a **flat
top-N** selection — a crowded frequency neighborhood packed with many strong
candidates can win enough of the global top-200 slots to starve out a weak,
genuine, isolated signal sitting in an otherwise-quiet bin, even though the
weak signal would trivially clear a *per-bin* selection cut. Fix: group
above-threshold cells by `freq_bin` first, keep only the top K=2 (by
`sync_score`) per bin, THEN apply the (unchanged) global cap.

## What I found

### Current selection logic (re-grepped; brief's `3853+/4144` line numbers were
stale — 30+ commits into `decoder.rs` this session before this task started)

`costas_sync_search_with_threshold_and_partner` (now at
`pancetta-ft8/src/decoder.rs:5514`, function body's main sweep loop ending
~5726): a triple-nested loop over `freq_sub × t0 × f0` computes
`compute_costas_score` (optionally `max(full, partial)` under hb-242's
`costas_partial_metric_enabled`), pushes every cell whose score exceeds the
(possibly near-partner-relaxed) threshold into a flat `Vec<CostasCandidate>`,
then (independent of this task) an **auxiliary tight/wide two-baseline
pathway** (`costas_two_baseline_enabled`, default off) optionally appends
extra per-bin-peak candidates, then the whole list is sorted best-first and
`candidates.truncate(self.config.max_sync_candidates)` applies the flat global
cap (line ~5744 pre-edit), followed by NMS.

### "Auxiliary pathway" reference (brief said this might be stale — verified,
it is NOT stale, it names a real, already-existing mechanism)

The brief's "keep top-1 per bin per auxiliary pathway" refers to
`Ft8Config::costas_two_baseline_enabled` (hb-242's wide-lag `tight`/`wide`
mechanism, ported from WSJT-X mainline `sync8`'s per-bin tight/wide lag
windows — `research/specs/spec-wsjtx-mainline-sync8.md` Phase 3/4). That
mechanism ALREADY performs its own top-1-per-`freq_bin`-per-pathway selection
via `tight_peaks`/`wide_peaks` arrays (one best-scoring `(score, t0)` pair per
bin per pathway) — it is structurally per-bin already, just gated behind a
separate, still-default-off flag, and orthogonal to the flat-vs-per-bin
question this task addresses for the MAIN sweep. No changes were made to
that pathway; this task's new per-bin selection runs on the main sweep's
output BEFORE that pathway appends its own (already-per-bin) candidates, so
the two compose additively rather than one undoing the other.

### `lid_of_band` — exists as a named eval tier, but its manifest is
STRUCTURALLY INCOMPATIBLE with the loader that tier dispatches to (verified,
not assumed)

`lid-of-band` IS a real, wired eval tier (`pancetta-research/src/bin/eval.rs`
dispatches `"lid-of-band" => "lid_of_band"` into `run_curated_tier`, same
code path as `curated-hard-200`/`curated-hard-1000`/`wild-50`/`wild-100`).
However, running it fails immediately:

```
Error: missing field `interest_score` at line 18 column 5
```

`run_curated_tier` deserializes via `CuratedManifest`/`CuratedEntry`
(`pancetta-research/src/curated.rs`), which requires a top-level
`scoring_decoder: String` field and, per entry, `interest_score: f64` +
`score_breakdown: ScoreBreakdown`. `research/corpus/curated/ft8/lid_of_band.manifest.json`
(294 entries, produced by the separate hb-156/Batch-29 `batch29_lid_of_band_ship.rs`
tool) has NEITHER of these — its schema is `{schema_version, label,
generated_at, snr_threshold_db, sources, entries: [{wav_path, wav_sha256,
source_tier, min_truth_snr_db, n_truths_at_or_below_threshold,
n_truths_total}]}`, structurally different from `hard_200.manifest.json` /
`hard_1000.manifest.json` / `wild_50.manifest.json` / `wild_100.manifest.json`
(all of which DO carry `scoring_decoder` + per-entry `interest_score` +
`score_breakdown`, confirmed by inspecting each file directly). This is a
genuine, pre-existing, out-of-scope-to-this-task corpus/loader mismatch —
same class of finding as W4.3's `synth_pair_200` cache issue and W4.4's
`chrono_replay`/`hard-200` corpus-structure findings this session, just a
harder failure (a hard error instead of a silent structural no-op). Fixing
it would mean either backfilling `scoring_decoder`/`interest_score`/
`score_breakdown` for all 294 entries or teaching the loader to accept the
legacy hb-156 schema — a nontrivial, separate undertaking, not attempted
here. Per the task's own escalation guidance ("use hard-200 alone if no
reasonable substitute exists quickly, and clearly document the gap"), I
proceeded with `curated-hard-200` alone as the authoritative A/B corpus.
The regression found there is unambiguous and decisive on its own (see
below); a working `lid-of-band` run would not have changed the decline
decision even if it had shown a local win, since the plan's success bar is a
net win without a hard-200 regression.

### Existing exact-duplicate dedup — verified preserved unchanged

The only "exact duplicate" guard inside this function is the two-baseline
auxiliary block's absolute/normalized-score gate (`tight_pass && tight_score
<= min_score`, `wide_pass && (wide_score <= min_score || wide_t0 !=
tight_t0)`) that avoids re-emitting a candidate the main sweep already
pushed. This gate is untouched and is score/threshold-based, not
list-membership-based, so it behaves identically regardless of whether the
new per-bin thinning ran on the main list beforehand. NMS (near-duplicate
suppression by score-relative + spatial radius) is also untouched, and runs
AFTER the global cap exactly as before.

## TDD evidence

Five new unit tests in `pancetta-ft8/src/decoder.rs::tests`, all built around
a new pure function `select_top_k_per_bin(candidates: Vec<CostasCandidate>,
k_per_bin: usize) -> Vec<CostasCandidate>` (module-scope, no `&self`, no
spectrogram — genuinely "list in, list out"):

- **`test_per_bin_selection_saves_weak_isolated_cell_from_crowded_band`**
  (the brief's required test): builds a 30-cell crowded cluster all sharing
  `freq_bin=100` (scores 25.00–25.29) plus one weak, isolated cell at
  `freq_bin=900` (score 10.0).
  - **RED** (reproduces the bug): flat top-20 (sort + truncate, the legacy
    behavior) keeps exactly 20 cells, ALL from the crowded cluster — the
    weak isolated cell is completely starved out, asserted directly.
  - **GREEN** (proves the fix): `select_top_k_per_bin(candidates, 2)` keeps
    the weak isolated cell (asserted present with its original score),
    thins the crowded bin to exactly its top 2 (25.29, 25.28), for a total
    of 3 output candidates.
- `test_select_top_k_per_bin_general_grouping`: three bins with 1/2/3
  candidates each against K=2 — under-K bin kept whole, at-K bin kept
  whole, over-K bin thinned to its top 2 by score.
- `test_select_top_k_per_bin_edge_cases`: empty input and `k_per_bin=0` both
  return empty (literal "keep zero per bin" semantics, no pass-through
  special case).
- `test_per_bin_candidate_selection_default_off`: `Ft8Config::default()`
  has the flag `false`.
- `test_per_bin_candidate_selection_wiring_flag_off_is_noop_flag_on_still_decodes`:
  wiring-level smoke test against a real (not purely synthetic-list) single-
  signal `Spectrogram` via a local `w51_build_synthetic_costas` helper
  (mirroring the pattern already duplicated per-test-module elsewhere in
  this file). Confirms (a) `Ft8Config::default()` vs. an explicit
  `per_bin_candidate_selection = false` produce byte-identical
  `costas_sync_search_with_threshold_and_partner` output (freq_bin, time_step,
  freq_sub, sync_score all equal, element-by-element), and (b) with the flag
  `true`, the search still completes and still finds the real synthetic
  signal near its true `freq_bin` (no crash, no silent total loss on an
  uncrowded scene).

All 5 pass; full `pancetta-ft8` lib-test count went from 506 → 511.

## A/B workflow

Binaries: `cargo build --release -p pancetta-research --bin eval --bin
compare --features research-eval`.

```
./target/release/eval --tier curated-hard-200 --mode ft8 \
    --output hard200_ctrl.json
./target/release/eval --tier curated-hard-200 --mode ft8 \
    --output hard200_treat.json --per-bin-candidate-selection
./target/release/compare hard200_ctrl.json hard200_treat.json
```

(`lid-of-band` attempted with the same pattern, failed with the manifest
schema error documented above before any decode work ran — see "What I
found".)

### Result (curated-hard-200, n=200 WAVs, git sha 6e6d87d9 for both runs)

```
A: hard200_ctrl.json  (score 0.3126)
B: hard200_treat.json (score 0.3071, Δ=-0.0055)

REGRESSIONS:
  curated-hard-200      decode_rate   0.6252 → 0.6142  (-0.0110)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200          rec Δ=-22  (95% CI [-36.0, -8.0], n_bootstrap=1000) — significant
  curated-hard-200          novel Δ=-50  (95% CI [-86.0, -12.0], n_bootstrap=1000) — significant

CONFIG DIFF:
  decoder.decoder.per_bin_candidate_selection      false → true
```

Raw counts (n=200 WAVs, 2001 truths total):
- Control: `truth_decodes_recovered=1251`, `novel_decodes=4956`
  (`novels_verified=3161`, `novels_unverified=1795`)
- Treatment: `truth_decodes_recovered=1229` (-22), `novel_decodes=4906` (-50)
  (`novels_verified=3122` (-39), `novels_unverified=1784` (-11))

Both `rec` (true-positive recovery) AND `novel` (new decodes vs. the jt9
baseline) drop together, at both the 95% CI level — this is not a
precision/recall tradeoff or a redistribution of which candidates get
through; it is a straightforward net loss of decodes under the flag.

## Decision: DECLINE the default flip

`Ft8Config::per_bin_candidate_selection` stays `false`. The regression is
statistically significant on the plan's own gating corpus (hard-200), in
both directions the plan cares about (recall and novel decodes), with no
compensating win found anywhere in this run. Per this session's established
pattern (many prior W-tasks correctly declined on real regressions or unmet
criteria), this is a clean, honest negative result, not a corner cut.

### Hypothesis for the root cause (not exhaustively re-verified — flagged as
such, not asserted as fact)

The flat top-`max_sync_candidates` (200) cap is, empirically, a *rare*
constraint on ordinary hard-200 WAVs — most single-recording decode windows
never come close to accumulating 200 above-threshold cells, so the flat cap
is usually a complete no-op and every above-threshold candidate reaches NMS
unfiltered. A per-bin K=2 cap, by contrast, is **not** a no-op even in the
uncrowded case: any bin with 3+ above-threshold cells gets thinned
regardless of whether the global 200-cap would ever have bound. Real Costas
correlation naturally produces this in ordinary, non-crowded scenes — a
genuine signal's own sync peak typically has 1-2 immediate `t0` neighbors
that also clear `min_score` (correlation sidelobes), and `freq_sub`
splitting (FREQ_OSR=2) means the *same* nominal frequency neighborhood can
carry two genuinely distinct signals a half-bin apart that this task's
selection groups by `freq_bin` alone (per the brief's literal spec),
putting them in direct competition for the same 2-slot budget. The net
effect: the mechanism trades a rare, pathological win (the specific
crowded-single-bin starvation scenario the brief was written to fix) for a
common, everyday cost (discarding legitimate near-threshold candidates in
completely ordinary, uncrowded bins) — and on this corpus the everyday cost
dominates. A future revisit could try a larger K (4-6) or grouping by
`(freq_bin, freq_sub)` pairs instead of `freq_bin` alone to see if the
common-case cost shrinks while the crowded-band fix is retained; not
attempted here since the brief specified K=2 grouped by `freq_bin` and a
single clean measurement against that exact spec already gives an
unambiguous decline.

The mechanism ships correctly implemented, unit-proven, and fully wired
(config field + eval CLI flag + research builder) for any future retest
with a different K or grouping key — no further plumbing needed, just a
new sweep.

## Full test suite

`cargo test --features transmit -p pancetta-ft8`: all green (511 lib tests,
up from 506; all integration test files and doctests pass, 0 failed anywhere).

`cargo test --workspace --features transmit`: all green — 93 `test result:
ok` blocks across every workspace crate/binary/doctest, zero `FAILED` lines,
zero non-zero `failed` counts anywhere in the full run log.

`cargo fmt -p pancetta-ft8 -p pancetta-research -- --check`: clean.
`cargo clippy -p pancetta-ft8 --features transmit --tests -- -D warnings`: clean.
`cargo clippy -p pancetta-research --lib --bins --tests --features research-eval -- -D warnings`: clean.

## Files changed

- `pancetta-ft8/src/decoder.rs`:
  - `PER_BIN_CANDIDATE_TOP_K: usize = 2` module constant.
  - `Ft8Config::per_bin_candidate_selection: bool` field (doc'd, default
    `false`).
  - `select_top_k_per_bin(candidates, k_per_bin)` pure module-scope
    function (grouping + per-bin top-K thinning).
  - Wired into `costas_sync_search_with_threshold_and_partner`: called on
    the main sweep's output, gated on the new flag, BEFORE the two-baseline
    auxiliary block runs (so the two compose additively) and BEFORE the
    existing global `max_sync_candidates` truncation.
  - 5 new unit tests (see TDD evidence above) + a local
    `w51_build_synthetic_costas` test helper (module-scoped copy of the
    pattern already duplicated in `hb230_relaxed_sync_tests` /
    `auto_passband_tests`).
- `pancetta-research/src/decoder.rs`: `with_per_bin_candidate_selection`
  builder.
- `pancetta-research/src/bin/eval.rs`: `--per-bin-candidate-selection` /
  `--no-per-bin-candidate-selection` CLI flags, struct field, wiring.

## Learnings / follow-ups

- **A per-bin cap is not automatically "safer" than a flat global cap just
  because it's less globally aggressive in the crowded case** — it can be
  MORE aggressive than the mechanism it replaces in the (much more common)
  uncrowded case, if the flat cap rarely binds there in the first place.
  Any future "make the cap smarter/fairer" mechanism in this decoder should
  be measured against how often the ORIGINAL cap actually constrains real
  corpora, not just against the specific pathological scenario that
  motivated the change.
- **`lid_of_band`'s eval-tier wiring and its manifest file have drifted out
  of sync** — the tier dispatch code assumes the standard
  `CuratedManifest`/`CuratedEntry` schema (`scoring_decoder` +
  `interest_score` + `score_breakdown`), but
  `research/corpus/curated/ft8/lid_of_band.manifest.json` was produced by an
  older, separate tool (`batch29_lid_of_band_ship.rs`, hb-156/Batch 29) with
  a structurally different schema. This corpus has therefore been
  UNRUNNABLE via `eval --tier lid-of-band` since whenever `scoring_decoder`/
  `interest_score`/`score_breakdown` became required fields — worth a
  dedicated fix (regenerate the manifest through the standard `curate`
  pipeline, or teach the loader to accept the legacy schema) before any
  future task cites `lid-of-band` as a usable corpus.
- If a future hypothesis wants to revisit per-bin candidate selection: try
  a larger K (4-6) and/or group by `(freq_bin, freq_sub)` rather than
  `freq_bin` alone, and re-measure on a WORKING `lid-of-band` (after the
  manifest-schema fix above) in addition to hard-200, since a weak-signal-
  dense corpus is exactly where the mechanism's intended benefit should be
  easiest to see if it exists at any K.
