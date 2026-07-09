---
slug: w52-costas-two-baseline-retest
mode: ft8
state: shelved
created: 2026-07-09T00:00:00Z
last_updated: 2026-07-09T00:00:00Z
branch: worktree-decoder-tp-sensitivity
parent_hypothesis: decoder-tp-sensitivity plan Task W5.2 (spec — retest the
  historically-shelved percentile-normalized wide-lag `costas_two_baseline`
  sync mechanism now that Task W5.1 built a per-bin candidate-selection
  alternative theorized to fix the flat-cap interaction that shelved it)
wild_card: false
delta_vs_main: |
  Measurement #2 (shippable config, costas_two_baseline_enabled=true alone
  against the TRUE current default per_bin_candidate_selection=false),
  curated-hard-200, n=200 WAVs: recall Δ=-49 (95% CI [-69,-30], significant),
  novel Δ=-200 (95% CI [-253,-152], significant), decode_rate 0.6252 → 0.6007
  (-0.0245). noise_1000 FP: 1 → 1 (no change, passes the FP-on-noise leg in
  isolation). Decisive regression on recall/novel; this is the basis for the
  decline decision.

  Measurement #1 (diagnostic, W5.1's per_bin_candidate_selection FORCED true
  alongside costas_two_baseline_enabled=true), same corpora: recall Δ=-19
  (95% CI [-34,-5], significant), novel Δ=-101 (95% CI [-144,-60],
  significant), decode_rate 0.6252 → 0.6157 (-0.0095). noise_1000 FP: 1 → 2
  (+1, hard-gate fail on this leg alone). Smaller-magnitude regression than
  measurement #2, but still significant in both directions and additionally
  fails the FP-on-noise hard gate — moot regardless since W5.1's own default
  stays false (declined 2026-07-09, see
  `2026-07-09-w51-per-bin-candidate-selection-declined.md`), so this
  configuration could never ship on its own either way.
disposition: DECLINE the default flip. `Ft8Config::costas_two_baseline_enabled`
  stays `false`. The mechanism is fully implemented, working, and was already
  wired end-to-end (config fields, decode-loop consumption, prior batch-49/75
  research tooling) before this task started — nothing needed repair. Both
  the shippable configuration (measurement #2) and the W5.1-assisted
  diagnostic configuration (measurement #1) show statistically significant
  hard-200 recall AND novel-decode regressions. The per-bin fix mitigates
  roughly half the damage (rec Δ -49 → -19, novel Δ -200 → -101) but does not
  eliminate it and does not clear the standing gate either way — and since
  W5.1 itself is declined as a default, that assisted configuration is
  academic: it could never actually ship. The historical shelving-under-the-
  flat-cap theory is therefore only partially supported: the interaction
  with the flat cap explains SOME but not all of this mechanism's cost.
---

## Task

Task W5.2 of the decoder-true-positive-sensitivity plan
(`docs/superpowers/plans/2026-07-06-decoder-tp-sensitivity.md`): retest the
existing, historically-shelved `costas_two_baseline_enabled` mechanism (a
percentile-normalized wide-lag two-pathway sync candidate emission, ported
from WSJT-X mainline `sync8.f90`'s `red2` baseline — see
`research/specs/spec-wsjtr-sync-norm.md`) now that Task W5.1 landed a
per-bin candidate-selection alternative theorized to fix the flat top-N
cap interaction that caused the original shelving.

**Context that changes what "retest under W5.1" means**: W5.1
(`per_bin_candidate_selection`) was itself measured and DECLINED as a
default on 2026-07-09 (commit `99e562df`) — real hard-200 regression
(recall Δ=-22, novel Δ=-50, both significant). So "costas_two_baseline
retested under W5.1" cannot mean "retested under the new production
default," because there is no new production default; W5.1's flag stays
`false`. This task therefore runs TWO separate measurements to give the
retest real meaning:

1. **Diagnostic** — `costas_two_baseline_enabled=true` WITH
   `per_bin_candidate_selection` FORCED `true`: tests the brief's literal
   hypothesis (does the per-bin fix rescue costas_two_baseline from its
   historical flat-cap-interaction failure mode), even though this exact
   combination can never ship (W5.1's flag is off by default).
2. **Shippable** — `costas_two_baseline_enabled=true` alone, against
   TODAY's actual production default (`per_bin_candidate_selection=false`):
   tests the only configuration in which flipping
   `costas_two_baseline_enabled` could ever actually ship.

The flip decision is based on #2. #1 is reported as supporting/diagnostic
context about the interaction hypothesis.

## What I found: mechanism implementation state

`costas_two_baseline_enabled` (re-grepped; the brief's `3899-4136` line
numbers were stale — 30+ commits into `decoder.rs` this session before this
task started) is a complete, working, already-wired mechanism, NOT
something needing repair:

- `Ft8Config` fields (`pancetta-ft8/src/decoder.rs:742,751,757,763`):
  `costas_two_baseline_enabled: bool` (default `false`),
  `costas_two_baseline_tight_steps: usize` (default 20),
  `costas_two_baseline_percentile: f64` (default 0.40),
  `costas_two_baseline_norm_threshold: f64` (default 1.2).
- Consumed in `costas_sync_search_with_threshold_and_partner`
  (`decoder.rs:5560-5561, 5757-5758` at current HEAD): the auxiliary
  tight/wide two-pathway percentile-normalized candidate emission,
  appending its own already-per-bin-per-pathway candidates to the main
  sweep's output (verified unchanged in this task — see W5.1's log for the
  compositional relationship between the two mechanisms).
- Already exercised by prior research tooling
  (`pancetta-research/examples/batch48_measure.rs`,
  `batch49_{widelag,sync}_tuning.rs`, `batch49_combined.rs`,
  `batch75_shelved_mechanisms_sweep.rs`) — this is a previously-tuned,
  previously-shelved mechanism, not new code.
- Multiple existing decoder unit tests toggle it (`decoder.rs:16509` et
  seq.) as part of other features' regression coverage; none of those
  needed changes.

No implementation work was needed for this task beyond wiring it into the
`eval`/`compare` research harness (it had never been exposed as a CLI flag
there — prior batch-49/75 tooling used ad-hoc example binaries instead).

## `lid_of_band` manifest status — VERIFIED still broken, not fixed

Per the task's instruction to verify rather than assume, I ran
`./target/release/eval --tier lid-of-band --mode ft8 --output
/tmp/lid_check.json` directly:

```
Error: missing field `interest_score` at line 18 column 5
```

Confirmed unchanged from W5.1's finding two commits ago:
`research/corpus/curated/ft8/lid_of_band.manifest.json` (produced by the
older `batch29_lid_of_band_ship.rs` / hb-156 tool) lacks the
`scoring_decoder`/`interest_score`/`score_breakdown` fields the current
`CuratedManifest`/`CuratedEntry` loader (`pancetta-research/src/curated.rs`)
requires. This is the same pre-existing, out-of-scope-to-this-task
corpus/loader schema mismatch W5.1 found and correctly declined to fix.
Per the task's own escalation guidance and W5.1's precedent, I proceeded
with `curated-hard-200` + `noise_1000` as the A/B corpora, honestly
documenting the gap rather than attempting a fix.

## Harness changes made (wiring, not decoder behavior)

`costas_two_baseline_enabled` had no `eval`/`compare` CLI exposure before
this task (prior measurements used one-off example binaries). Added,
mirroring the existing `per_bin_candidate_selection` wiring exactly:

- `pancetta-research/src/decoder.rs`: `with_costas_two_baseline_enabled(on:
  bool)` builder method.
- `pancetta-research/src/bin/eval.rs`: `costas_two_baseline_enabled:
  Option<bool>` field, `--costas-two-baseline-enabled` /
  `--no-costas-two-baseline-enabled` CLI flags, help text, struct-literal
  wiring, and the `d = d.with_costas_two_baseline_enabled(on)` builder
  application. Purely additive; every existing flag/behavior unchanged
  (verified by the byte-identical `hard200_ctrl.json` baseline score
  matching prior runs' composite 0.3126).

Only `costas_two_baseline_percentile`/`_norm_threshold`/`_tight_steps` were
left at their compiled-in defaults (0.40 / 1.2 / 20) — the task only asks to
retest the `_enabled` master switch, and those values were already tuned in
Batch 49's prior sweep (see `batch49_sync_tuning.rs`).

## A/B workflow

Binaries: `cargo build --release -p pancetta-research --bin eval --bin
compare --features research-eval`.

```
# Baselines (production default: both flags false)
./target/release/eval --tier curated-hard-200 --mode ft8 --output /tmp/w52/hard200_ctrl.json
./target/release/eval --tier noise_1000       --mode ft8 --output /tmp/w52/noise_ctrl.json

# Measurement #1 (diagnostic — W5.1's per-bin selection forced on)
./target/release/eval --tier curated-hard-200 --mode ft8 \
    --output /tmp/w52/hard200_m1_treat.json \
    --costas-two-baseline-enabled --per-bin-candidate-selection
./target/release/eval --tier noise_1000 --mode ft8 \
    --output /tmp/w52/noise_m1_treat.json \
    --costas-two-baseline-enabled --per-bin-candidate-selection

# Measurement #2 (shippable — costas_two_baseline alone, true default)
./target/release/eval --tier curated-hard-200 --mode ft8 \
    --output /tmp/w52/hard200_m2_treat.json --costas-two-baseline-enabled
./target/release/eval --tier noise_1000 --mode ft8 \
    --output /tmp/w52/noise_m2_treat.json --costas-two-baseline-enabled

./target/release/compare /tmp/w52/hard200_ctrl.json /tmp/w52/hard200_m1_treat.json
./target/release/compare /tmp/w52/hard200_ctrl.json /tmp/w52/hard200_m2_treat.json
./target/release/compare /tmp/w52/noise_ctrl.json   /tmp/w52/noise_m1_treat.json
./target/release/compare /tmp/w52/noise_ctrl.json   /tmp/w52/noise_m2_treat.json
```

(`lid-of-band` attempted with the same pattern, failed with the manifest
schema error documented above before any decode work ran.)

### Measurement #1 result (diagnostic — W5.1 forced on)

git sha 99e562df for all four runs.

```
=== hard-200 ===
A: hard200_ctrl.json  (score 0.3126)
B: hard200_m1_treat.json (score 0.3078, Δ=-0.0047)

REGRESSIONS:
  curated-hard-200      decode_rate   0.6252 → 0.6157  (-0.0095)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200          rec Δ=-19  (95% CI [-34.0, -5.0]) — significant
  curated-hard-200          novel Δ=-101  (95% CI [-144.0, -60.0]) — significant

=== noise_1000 ===
A: noise_ctrl.json  (false_positives_total=1)
B: noise_m1_treat.json (false_positives_total=2)

HARD GATE FAILURE — FALSE POSITIVES INCREASED ON NOISE TIER
  noise_1000   false_positives_total   1 → 2  (+1)
  noise_1000   noise_files_decoded     1 → 2  (+1)
```

Raw counts (n=200 WAVs, 2001 truths total): control
`truth_decodes_recovered=1251`/`novel_decodes=4956`; measurement #1
`truth_decodes_recovered=1232` (-19)/`novel_decodes=4855` (-101).

### Measurement #2 result (shippable — true current default)

```
=== hard-200 ===
A: hard200_ctrl.json  (score 0.3126)
B: hard200_m2_treat.json (score 0.3003, Δ=-0.0122)

REGRESSIONS:
  curated-hard-200      decode_rate   0.6252 → 0.6007  (-0.0245)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200          rec Δ=-49  (95% CI [-69.0, -30.0]) — significant
  curated-hard-200          novel Δ=-200  (95% CI [-253.0, -152.0]) — significant

=== noise_1000 ===
A: noise_ctrl.json  (false_positives_total=1)
B: noise_m2_treat.json (false_positives_total=1)

REGRESSIONS: (none) — FP-on-noise leg passes in isolation.
```

Raw counts (n=200 WAVs, 2001 truths total): control
`truth_decodes_recovered=1251`/`novel_decodes=4956`; measurement #2
`truth_decodes_recovered=1202` (-49)/`novel_decodes=4756` (-200).

## Decision: DECLINE the default flip (based on measurement #2)

`Ft8Config::costas_two_baseline_enabled` stays `false`. Measurement #2 —
the only configuration in which this flag could ever actually ship
standalone — shows a statistically significant, sizeable regression in
BOTH true-positive recall (-49, ~4% of the control's 1251 recovered
truths) and novel decodes (-200), with no compensating win anywhere in
this run. It does clear the FP-on-noise leg cleanly (1→1, no change), but
that alone is not sufficient under the plan's standing gate (recall gain
required; here there's a loss).

Measurement #1 is reported as supporting/diagnostic context per the task's
explicit instruction, not as a basis for the flip: the per-bin fix cuts the
regression roughly in half (rec Δ -49→-19, novel Δ -200→-101) — genuine
partial support for the historical "shelved under the flat cap" theory —
but the regression remains significant in both directions AND this
configuration additionally fails the FP-on-noise hard gate (+1 FP), so it
does not clear the standing gate either. It is also moot as a shippable
option regardless of its own numbers, since W5.1's `per_bin_candidate_selection`
is itself declined as a default (a genuine regression on its own, unrelated
to this mechanism).

### Interpretation

The flat-top-N-cap interaction explains SOME but not ALL of
`costas_two_baseline`'s historical cost. Even with that interaction
neutralized (measurement #1), the mechanism still costs real recall and
novel decodes on hard-200 — consistent with a genuine, independent FP/TP
tradeoff cost from the percentile-normalization itself (lowering the
effective sync-score bar via the 40th-percentile baseline admits weaker,
more numerous candidates that compete with and sometimes displace stronger
ones downstream in NMS/OSD, not solely a candidate-cap artifact). A future
revisit, if ever attempted, should treat the percentile/norm-threshold
tuning itself (not just the candidate-cap interaction) as the primary lever
— Batch 49's prior sweep already explored this space somewhat
(`batch49_sync_tuning.rs`) without finding a win; nothing in this retest
changes that conclusion.

## Full test suite

`cargo test --workspace --features transmit`: all green, 0 failed
anywhere in the full run.

`cargo fmt -p pancetta-ft8 -p pancetta-research -- --check`: clean.
`cargo clippy -p pancetta-ft8 --features transmit --tests -- -D warnings`: clean.
`cargo clippy -p pancetta-research --lib --bins --tests --features research-eval -- -D warnings`: clean.

## Files changed

- `pancetta-ft8/src/decoder.rs`: doc-comment update on
  `costas_two_baseline_enabled` recording this retest's result and
  disposition (no behavior change — field stays `false`).
- `pancetta-research/src/decoder.rs`: `with_costas_two_baseline_enabled`
  builder.
- `pancetta-research/src/bin/eval.rs`: `--costas-two-baseline-enabled` /
  `--no-costas-two-baseline-enabled` CLI flags, struct field, help text,
  wiring.

## Learnings / follow-ups

- **A mitigating fix for one specific failure mode (the flat candidate
  cap) does not automatically clear a mechanism's overall regression** —
  worth checking whether a mechanism's cost is entirely explained by the
  interaction theory before concluding a fix rescues it. Here it explained
  roughly half.
- **`lid_of_band`'s eval-tier manifest schema mismatch remains unfixed** —
  same finding as W5.1, re-verified directly rather than assumed. Still
  worth a dedicated fix (regenerate through the standard `curate` pipeline,
  or teach the loader to accept the legacy hb-156 schema) before any future
  task cites `lid-of-band` as a usable corpus.
- If a future hypothesis wants to revisit `costas_two_baseline`, the next
  lever to try is the percentile/norm-threshold values themselves (not just
  the candidate-cap interaction), on a corpus where `lid-of-band` is
  actually runnable (weak-signal-dense, closer to where this mechanism's
  intended benefit — slot-edge negative-dt signals — should be easiest to
  see).
