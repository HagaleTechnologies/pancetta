---
slug: w35-linear-power-substep-averaging
mode: ft8
state: declined
created: 2026-07-08
last_updated: 2026-07-08
parent_plan: docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md (Workstream 3)
scorecard: research/scorecards/w35-linpow-{control,variant}.json
delta_vs_control: composite -0.0142; hard-200 rec Δ=-57 (95% CI [-81,-35], significant); hard-200 novel Δ=-196 (95% CI [-228,-164], significant); noise_1000 FP unchanged (1 -> 1)
disposition: DECLINED — linear-power substep averaging regresses real-corpus recall decisively. Ft8Config::linear_power_averaging stays default false.
---

## Task

Decoder-TP-sensitivity plan Task W3.5. Design spec finding (`docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`,
section 3, "Findings: demodulation and fine sync"): "the two TIME_OSR substeps are averaged **in
dB** (`(db_a+db_b)/2`, `decoder.rs:8255`) — a geometric mean of powers. Fractional-time
interpolation is also dB-domain by default (`sync_time_interp_linear_power: false`, 1116)."

Brief: implement the substep combine in linear power behind one flag, A/B on hard-200 +
noise_1000, flip on a clean pass, decline honestly otherwise. Also: "revisit the ×0.3 interp
damping ... once interpolation is linear-power" — investigated below; found not applicable to
this specific change (see "Reconciling the brief's damping note").

## Locating the target: two DISTINCT dB-domain mechanisms, not one

The brief's stale line numbers (from a plan snapshot 20+ commits old) pointed at
`sync_time_interp_linear_power`, but that field **already exists** in production
(`pancetta-ft8/src/decoder.rs`, hb-069, 2026-05-31) and controls a *different* call site than
this task's actual target:

1. **`sync_time_interp_linear_power`** (existing, default `false`) — gates `lookup_time_interp`'s
   conversion when doing the **fractional dt-shift lookup** (`t_base + dt`), active only when
   `candidate.time_refinement != 0` (i.e. only when `sync_time_interpolation`'s parabolic
   refinement fired AND the score gate passed). This flag was already A/B'd in hb-069
   (`research/experiments/2026-05-31-hb-069-linear-power-interp.md`) and **SHELVED**: composite
   -0.003049, hard-200 -54 rec / -34 novel, hard-1000 -94 rec / -33 novel. Production stays dB
   at this call site.
2. **The substep combine** (`extract_symbols_from_spectrogram` / `par_extract_symbols_from_spectrogram`,
   `mags[tone] = (db_a + db_b) / 2.0` where `db_a`/`db_b` are the spectrogram readings at the
   symbol's two TIME_OSR sub-steps `t_base` and `t_base+1`) — this is **unconditional**, runs on
   EVERY symbol of EVERY candidate regardless of `sync_time_interpolation`/`time_refinement`
   (dt=0 is the common case; `lookup_time_interp` with dt=0 returns the raw dB value directly,
   never even consulting `sync_time_interp_linear_power`). Since `sync_time_interpolation`
   defaults `true` (graduated hb-068, 2026-05-30) and `linear_power_averaging` was previously
   nonexistent, **this substep combine has never been tested in linear power before** — it is
   NOT the same axis hb-069 already closed out. This is the design spec's actual W3.5 target
   (confirmed: design line "8255" in the stale snapshot maps to this exact call site in the
   current file, now at line ~10046+).

**Confirmed: this is on the ALWAYS-ACTIVE coarse-sync path**, not gated behind
`fine_sync_enabled` (which defaults false and is unrelated — W3.1-W3.4's new `fine_sync.rs`/
`baseband.rs` modules are untouched by this task). `sync_time_interpolation: true` is already the
production default, so every decode window exercises the substep combine on every candidate.

## Reconciling the brief's damping note

The brief says to "revisit the ×0.3 interp damping (`sync_time_interp_delta_scale`) once
interpolation is linear-power." Investigated via W3.2's own report
(`.superpowers/sdd/task-W3.2-report.md`): the ×0.3 damping's documented justification is that the
**sync-score parabolic fit** (`scored()` closure in `costas_sync_search`, built from
`compute_costas_score`/`compute_costas_score_partial_bc` — a THIRD, separate dB-domain surface
used to pick the fractional `time_refinement` value in the first place) over-corrects because it
fits a parabola through dB-domain Costas correlation scores. That surface is **not** touched by
either `linear_power_averaging` (this task, the *extraction*-time substep combine) or
`sync_time_interp_linear_power` (the *extraction*-time fractional-lookup conversion) — the
sync-score surface stays dB regardless of what either extraction-time flag does. The damping's
justification is therefore **not causally connected to this task's change** and revisiting it
here would not be principled. Given the decisive regression measured below (this task's own
mechanism is a clear net negative on its own), there is no live combination left worth exploring
the damping under — declining without a delta-scale sweep.

## Implementation

`Ft8Config::linear_power_averaging: bool` (new, default `false`) threaded through:
- `combine_substeps(db_a, db_b, linear_power) -> f64` (new helper, next to `lookup_time_interp`):
  `false` reproduces `(db_a+db_b)/2.0` exactly (byte-identical); `true` converts each endpoint
  dB→linear (`10^(db/10)`), takes the arithmetic mean, converts back to dB (`10*log10`), with the
  same `-120.0` floor-sentinel handling as `lookup_time_interp`'s own linear-power branch.
- Both symbol-extraction implementations: the serial `Ft8Decoder::extract_symbols_from_spectrogram`
  method (reads `self.config.linear_power_averaging` directly) and the free function
  `par_extract_symbols_from_spectrogram` (gained a new `avg_linear_power: bool` parameter,
  threaded through all 9 call sites: 2 direct `self.config` reads inline, 4 more `self.config`
  reads at other call sites in coherent-subtraction/residual-repass/a7 passes, 2 via the shared
  `DecodeContext` struct — new field `linear_power_averaging`, threaded at the 1 production
  construction site + 2 test `build_ctx` helpers — and 1 direct-`false` test call site).
- `pancetta-research::DecoderBuilder::with_linear_power_averaging` + `eval.rs
  --linear-power-averaging` CLI flag (does NOT imply `--sync-time-interpolation`, unlike
  `--sync-time-interp-linear-power` — the substep combine is unconditional).

## TDD evidence

New unit test module `pancetta-ft8/src/decoder.rs::combine_substeps_tests` (4 tests, pure
function, no decode pipeline):
1. `db_domain_matches_legacy_average_exactly` — `linear_power=false` byte-identical to the
   pre-existing `(a+b)/2.0` expression across several representative dB values (flag-off
   regression guard).
2. `equal_inputs_are_unchanged_in_either_domain` — averaging two identical readings is a no-op
   in either domain.
3. `linear_power_average_dominates_db_average_for_unequal_inputs` — AM ≥ GM: for unequal inputs,
   the linear-power average is strictly larger than the dB average, and independently matches the
   textbook `10*log10(0.5*(10^(a/10)+10^(b/10)))` formula (not just "larger than baseline").
4. `floor_sentinel_round_trips_without_nan_or_inf` — the `-120` dB floor doesn't produce `-inf`/
   `NaN` through the linear-power path.

```
cargo test --features transmit -p pancetta-ft8 --lib combine_substeps -- --nocapture
test result: ok. 4 passed; 0 failed
```

Full workspace suite (flag-off byte-identical regression check):
```
cargo test --workspace --features transmit
... all crates: test result: ok, 0 failed (15 test binaries + doctests, zero failures)
```

## The A/B: hard-200 + noise_1000, TRUE production default (control) vs. `linear_power_averaging=true`

This is NOT gated behind `fine_sync_enabled` — measured against the actual shipped default
config, no flags forced on in either leg (unlike W3.4's isolation methodology, which forces
`fine_sync_enabled=true` in both legs because that flag is a prerequisite; here there is no
prerequisite, the substep combine already runs in production today).

```
cargo build --release -p pancetta-research --bin eval --bin compare

./target/release/eval --tier curated-hard-200,noise_1000 --mode ft8 \
    --output research/scorecards/w35-linpow-control.json
# wrote scorecard: composite raw 0.3126, saturation-aware 0.3147, 766.1s
# noise_1000: 1 FALSE POSITIVE decode(s) across 1/1000 noise-only WAVs

./target/release/eval --tier curated-hard-200,noise_1000 --mode ft8 --linear-power-averaging \
    --output research/scorecards/w35-linpow-variant.json
# wrote scorecard: composite raw 0.2984, saturation-aware 0.3004, 914.2s
# noise_1000: 1 FALSE POSITIVE decode(s) across 1/1000 noise-only WAVs

./target/release/compare research/scorecards/w35-linpow-control.json \
    research/scorecards/w35-linpow-variant.json
```

```
A: research/scorecards/w35-linpow-control.json (sha 7a61d3c1, score 0.3126)
B: research/scorecards/w35-linpow-variant.json (sha 7a61d3c1, score 0.2984 -0.0142)

REGRESSIONS:
  curated-hard-200      decode_rate   0.6252 → 0.5967  (-0.0285)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200          rec Δ=-57  (95% CI [-81.0, -35.0], n_bootstrap=1000) — significant
  curated-hard-200          novel Δ=-196  (95% CI [-228.0, -164.0], n_bootstrap=1000) — significant

CONFIG DIFF:
  decoder.decoder.linear_power_averaging           false → true
```

## Gate check (standing rule: ΔTP ≥ threshold with bootstrap CI excluding zero, ΔFP-on-noise = 0,
Δunverified-novels ≤ 2×ΔTP)

- **ΔFP-on-noise = 0**: noise_1000 false positives unchanged (1 → 1 across both runs). This axis
  passes.
- **ΔTP**: hard-200 recall **Δ=-57**, 95% CI **[-81, -35]** — entirely negative, decisively
  significant in the WRONG direction. This is not "no measurable effect," it's a clear,
  statistically robust regression.
- **Novels**: also collapsed, Δ=-196 (CI [-228,-164]) — consistent with fewer decodes overall,
  not a hallucination-growth problem, but not a mitigating factor either.
- **Elapsed**: variant ran slower too (914.2s vs 766.1s, +19%) — the extra `powf`/`log10` per
  symbol-tone-substep pair is not free, though this is a secondary concern given the recall result
  already fails the gate outright.

**Decision: DECLINE.** `Ft8Config::linear_power_averaging` stays default `false`. No delta-scale
sweep, no stacking with `sync_time_interp_linear_power` — the mechanism's own effect is decisive
enough (composite -0.0142, more than 4× the magnitude of any single graduation this session) that
further parameter search on top of it is not warranted.

## Why the hypothesis didn't hold (consistent with hb-069's own finding on the adjacent axis)

Same structural reason hb-069 documented for the fractional-lookup axis, transferring cleanly to
the substep-combine axis: `par_compute_soft_llrs_db` consumes dB-domain tone magnitudes directly
(no further log conversion) and the acceptance/noise-floor calibration downstream is tuned against
dB-domain values. Converting to linear power and back adds a second round of conversion error and
shifts the "noise floor" of the two-substep average away from the calibrated dB reference,
compounding per-symbol across all 79 symbols of every candidate — costing far more recall here
than on the fractional-lookup axis (which only applies when `time_refinement != 0`, a smaller
fraction of candidates) because the substep combine runs on literally every symbol.

## Implications for the design spec's Workstream-3 finding

The design spec's finding (section 3) correctly identified BOTH mechanisms as dB-domain "levers,"
but this result shows they are not free wins — pancetta's downstream LLR/acceptance pipeline is
specifically tuned to dB-domain symbol magnitudes, and BOTH of the two tested "make it linear
power" axes (hb-069's fractional lookup, this task's substep combine) regress real-corpus recall.
The remaining untested axis from the design spec's finding 3 (frequency-axis sub-bin refinement —
"nothing in the default pipeline estimates frequency finer than 3.125 Hz") is a genuinely
different mechanism (adds NEW information rather than recombining existing dB samples in a
different domain) and is not closed out by this result.

## Files changed

- `pancetta-ft8/src/decoder.rs`: new `Ft8Config::linear_power_averaging` field + doc, `Default`
  wiring, `combine_substeps` helper + 4 unit tests, `DecodeContext::linear_power_averaging` field,
  9 call-site threads (2 extraction functions + all their callers), 2 test `build_ctx` helpers.
- `pancetta-research/src/decoder.rs`: `with_linear_power_averaging` builder method.
- `pancetta-research/src/bin/eval.rs`: `--linear-power-averaging` CLI flag (struct field, parse
  arm, populate, apply site, `--help` line).
- `research/scorecards/w35-linpow-{control,variant}.json` (new, committed as evidence).
