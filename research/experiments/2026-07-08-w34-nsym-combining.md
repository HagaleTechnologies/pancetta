# nsym=2/3 noncoherent combining LLR variants (Workstream 3, Task W3.4)

**Date**: 2026-07-08
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: [A/B] measured on hard-200 AND the synth-clean SNR curve, both with `fine_sync_enabled`
forced `true` in both legs (isolating THIS flag's effect). **Mixed result, decisively resolved by
the standing hard-200 gate**: the synth-clean curve shows a genuine, clean SNR-sensitivity win
(-19.9 dB → -20.2 dB at 50% recovery, -0.3 dB, zero regressions — the favorable-channel effect the
design spec predicts), but hard-200 (real, messier signals) **FAILS the standing hard gate**:
unverified-novel decodes grow by +7 while verified-TP grows by only +3 (not statistically
significant, bootstrap CI [+0.0, +7.0]) — the "unverified-novel growth ≤ 2×ΔTP" rule (allowance +6)
is exceeded. **Decision: decline the flip.** `Ft8Config::nsym_combining_enabled` stays `false`.

Cross-references: `research/experiments/2026-07-08-w33-matched-demod-fine-sync.md` (Task W3.3,
built the matched-demod stage this task layers onto, declined for a separate cost reason — its own
elapsed-cost gate, unrelated to this task's recall-quality gate) and
`research/experiments/2026-07-08-w33b-budgeted-fine-sync-remeasurement.md` (Task W3.3b, re-measured
W3.3 under a real bounded `DecodeBudget` and found a budget-starvation regression at the `Standard`
preset — the reason `fine_sync_enabled` stays off in production today).

**IMPORTANT — inert-by-construction today**: `nsym_combining_enabled` only ever executes when
`fine_sync_enabled` is ALSO `true` (it consumes W3.3's per-symbol complex correlations, which only
exist inside `matched_demod_attempt` when that stage runs). Since `fine_sync_enabled` defaults to
`false` in production (per W3.3/W3.3b's declines), this flag is doubly inert regardless of its own
A/B result. The A/B below was run with `fine_sync_enabled` forced `true` in BOTH control and
variant, per this task's brief, to isolate the nsym flag's own effect — this is not the
production-default configuration and is not meant to represent it.

## What this is

Design spec section referenced by this task's brief (`docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`,
D3 / "No multi-symbol noncoherent combining" gap): WSJT-X-style nsym=2/3 Gray-hypothesis LLR
variants, "documented ~0.5-1 dB on stable signals". `research/specs/spec-wsjtx-mainline-ft8b.md`
describes the underlying technique (Variant B `bmetb`/nsym=2, Variant C `bmetc`/nsym=3): instead of
treating each FT8 data symbol's tone independently (the existing max-vs-max metric), pair or triple
adjacent data symbols and form the COHERENT (complex-amplitude) sum across the group under every
joint tone hypothesis, then run the same max-vs-max LLR extraction over the wider (64- or
512-candidate) hypothesis space. This recovers coherent-combining SNR gain on a genuinely
stable-phase channel, at the cost of a noisier per-attempt hypothesis space (more candidates to
confuse the metric) if the channel is NOT stable (real Doppler/multipath).

Task W3.3's `matched_demod_attempt` already retains the per-symbol, per-tone COMPLEX correlation as
an intermediate (before `.norm()` collapses it to a magnitude for the standard dual-max LLR path).
This task adds two more BP attempts on the SAME candidate/position when the 1-symbol attempt fails:
first a 2-symbol combined-LLR attempt, then a 3-symbol one — each through the identical
whitening/impulse-robust/normalize/BP/CRC/parse/plausibility/acceptance-gate pipeline as the
1-symbol path, so escalation adds attempts without loosening the accept bar.

## Implementation

- `Ft8Config::nsym_combining_enabled: bool` (new, default `false`), threaded through
  `DecodeContext` (one production construction site + two test `build_ctx` helpers) and
  `matched_demod_attempt`'s new last parameter, and into all 4 call sites
  (`par_matched_demod_decode` + the 3 rescue passes `cross_cycle_averaging_pass`,
  `joint_pair_retry_pass`, `joint_residual_localized_sync_pass`) — matching exactly how
  `fine_sync_enabled` itself was threaded in W3.3, so all 4 call sites stay in sync automatically.
- `matched_demod_attempt` (`pancetta-ft8/src/decoder.rs`): when `nsym_combining_enabled`, the
  per-symbol demod loop ALSO retains `tone_complex: Option<Vec<[Complex<f64>; NUM_TONES]>>`
  alongside the existing `tone_magnitudes` (an `Option` that stays `None` — no extra allocation —
  when the flag is off, preserving the byte-identical-by-default contract). The escalation itself
  is a new shared helper, `matched_demod_try_bp` (factored out of the inline decode_soft → CRC →
  parse → plausibility → confidence/acceptance-gate sequence that used to be written once inline;
  now called up to 3 times: once for the 1-symbol LLRs, then once each for the 2-symbol and
  3-symbol nsym-combined LLRs via `.or_else`).
- `nsym_hypothesis_energies(group, nsym)`: for `nsym` (2 or 3) adjacent data symbols, enumerates
  every joint tone hypothesis (`8^nsym` combinations, MSB-first per-symbol decomposition — bit `ib`
  of the combined index is literally "bit `ib%3` of symbol `ib/3`'s 3-bit tuple", matching the
  existing single-symbol metric's `s2[j]` convention exactly), Gray-maps each per-symbol
  hypothesized 3-bit tuple to a physical tone via the existing `crate::ldpc::binary_to_gray`
  table, and returns `|Σ complex_amplitude|` per hypothesis.
- `nsym_group_llrs(group, nsym)`: converts each hypothesis's energy to log-power
  (`10·log10(e²)`, matching `par_compute_soft_llrs`'s exact linear-magnitude-to-dB convention) and
  runs the same max-vs-max LLR extraction as the single-symbol metric, generalized to `3*nsym`
  output bits.
- `nsym_combined_llrs(pp, tone_magnitudes, tone_complex, nsym)`: groups `data_symbol_indices()`
  (58 for FT8) into non-overlapping `nsym`-symbol blocks. `nsym=2` divides evenly (29 pairs, 174
  bits exactly). `nsym=3` does NOT divide evenly (58 = 19×3 + 1); the trailing single symbol falls
  back to the ordinary 1-symbol max-vs-max metric (`single_symbol_max_llrs`, the same formula
  factored out of `par_compute_soft_llrs`'s inner loop) so the total is always exactly 174 bits.
  This remainder handling — and the fact that FT8's two 29-symbol data ranges (`7..36`, `43..72`)
  are separated by a 7-symbol Costas sync block, so exactly one group per `nsym` spans that
  boundary (verified: pair `[35,43]` for nsym=2, triple `[34,35,43]` for nsym=3) — is pancetta's
  own design choice; the spec describes the hypothesis construction, not how mainline `ft8b.f90`
  handles the non-integer group count, and clean-room policy forbids guessing at unread GPL source
  to match it exactly.
- `pancetta-research/src/decoder.rs::with_nsym_combining_enabled` builder;
  `pancetta-research/src/bin/eval.rs::--nsym-combining-enabled` CLI flag (full plumbing: struct
  field, parse arm, help text, populate, apply site) — mirrors `--fine-sync-enabled` exactly.

## TDD evidence

### RED/GREEN: metric construction (unit tests, `pancetta-ft8/src/decoder.rs::tests::nsym_combining_metric`)

Six new unit tests, all pure (no decode pipeline):

1. `nsym_hypothesis_energies_matches_hand_derivation_for_known_triplet` — a KNOWN 3-symbol group
   with fully deterministic, distinguishable complex amplitudes per (symbol, tone)
   (`mag=(k+1)(t+1)`, `phase=(k-t)*0.3`). For 4 specific hypotheses, the test independently
   hand-derives the expected coherent-sum magnitude (indexing the known group directly + the
   crate's own Gray table) and asserts it matches `nsym_hypothesis_energies`'s output to
   `epsilon=1e-9`. This is a genuine cross-check, not a "runs without panicking" smoke test — the
   hand-derivation is written independently in the test, using only the group's closed-form
   definition and the (already-verified-elsewhere) Gray table, never calling the function under
   test to produce its own expected value.
2. `nsym_hypothesis_energies_matches_hand_derivation_for_nsym_2` — same cross-check for `nsym=2`.
3. `matching_hypothesis_has_higher_energy_than_mismatch_under_stable_phase` — a purpose-built
   stable-phase (identical phase across every symbol/tone) group with a strong true tone and weak
   noise elsewhere; asserts the hypothesis matching the TRUE per-symbol tones scores strictly
   higher than every one of the other 511 mismatched hypotheses. This is the physical property the
   whole mechanism depends on (coherent addition beats any partial mismatch under a stable
   channel), verified directly rather than assumed.
4. `nsym_group_llrs_shape_and_non_degenerate` — shape (`3*nsym` LLRs) and non-degeneracy (not all
   zero) for both nsym values.
5. `nsym_combined_llrs_always_produces_174_bits` — for both `nsym=2` (divides evenly) and `nsym=3`
   (doesn't — exercises the trailing single-symbol remainder fallback), always exactly 174 bits.
6. `nsym_combined_llrs_rejects_unsupported_nsym` — guards `nsym` outside `{2,3}`.

All 6 pass (verified `cargo test -p pancetta-ft8 --features transmit --lib nsym_combining_metric`,
`test result: ok. 6 passed`).

### RED/GREEN: end-to-end fixture (`pancetta-ft8/tests/decoder_refinement_tests.rs::w34_nsym_combining`)

Per the brief: a synthetic STABLE-PHASE signal (a plain modulated "CQ K5ARH EM10", NO injected
dt/df error — a static AWGN channel, the favorable case coherent combining is designed for; unlike
W3.3's off-grid fixture, which deliberately stressed the fine-sync search instead) where the
1-symbol matched-demod LLR path fails BP but the nsym-combining escalation rescues it.

**Search process** (seed-searched, same methodology as W3.3's off-grid fixture): swept full-band
SNR from -17 dB down to -25 dB with `fine_sync_enabled=true` in both a `nsym_combining_enabled=false`
control config and a `nsym_combining_enabled=true` variant config, both isolating the nsym flag's
effect exactly as the corpus A/B below does. Findings:

| SNR (dB) | 1-symbol-only (25 seeds) | nsym-combining (25 seeds) | discriminating seeds |
|---|---|---|---|
| -21.0 | 25/25 | 25/25 | none (too easy) |
| -22.0 | 25/25 | 25/25 | none (too easy) |
| -23.0 | **21/25** | **25/25** | **[7, 17, 19, 22]** |

-23 dB is the discriminating point: the 1-symbol path fails on 4 of 25 seeds while nsym-combining
recovers ALL 25 — including every seed the 1-symbol path missed, with zero regressions (no seed
newly failed under nsym-combining). `NSYM_SNR_DB = -23.0`, `SEEDS = [7, 17, 19, 22]`.

- RED (`nsym_signal_rejected_by_1symbol_path`, `fine_sync_enabled=true, nsym_combining_enabled=false`):
  fails to decode on all 4 curated seeds. **PASS** (confirms the fixture discriminates).
- GREEN (`nsym_signal_recovered_by_combining_escalation`, `fine_sync_enabled=true,
  nsym_combining_enabled=true`): decodes on all 4 curated seeds. **PASS** (confirms the mechanism
  genuinely rescues these specific failures end-to-end through `decode_window`, not just at the
  pure-function unit level above).

Both verified via `cargo test --release -p pancetta-ft8 --features transmit --test
decoder_refinement_tests w34_nsym_combining`: `test result: ok. 2 passed`.

## The A/B: hard-200 (full 200 WAVs, `fine_sync_enabled=true` forced in both legs)

```
./target/release/eval --tier curated-hard-200 --mode ft8 --fine-sync-enabled \
    --output research/scorecards/w34-nsym-hard200-control.json
./target/release/eval --tier curated-hard-200 --mode ft8 --fine-sync-enabled --nsym-combining-enabled \
    --output research/scorecards/w34-nsym-hard200-variant.json
./target/release/compare research/scorecards/w34-nsym-hard200-control.json \
    research/scorecards/w34-nsym-hard200-variant.json
```

```
A: research/scorecards/w34-nsym-hard200-control.json (sha 5d3bf1e8, score 0.3261)
B: research/scorecards/w34-nsym-hard200-variant.json (sha 5d3bf1e8, score 0.3268 +0.0007)

WINS:
  curated-hard-200      decode_rate   0.6522 → 0.6537  (+0.0015)

REGRESSIONS:
  (none)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200          rec Δ=+3  (95% CI [+0.0, +7.0], n_bootstrap=1000) — NOT significant
  curated-hard-200          novel Δ=+28  (95% CI [+18.0, +39.0], n_bootstrap=1000) — significant

CONFIG DIFF:
  decoder.decoder.nsym_combining_enabled           false → true

############################################################
# HARD GATE FAILURE — UNVERIFIED-NOVEL GROWTH EXCEEDS 2×ΔTP
############################################################
  aggregate: unverified-novels Δ=+7 exceeds allowance +6 (2×ΔTP, ΔTP=+3)
  curated-hard-200      verified-TP Δ=+3   unverified-novels Δ=+7
```

Raw scorecard fields (`truth_decodes_recovered`, jt9-truth-matched): control 1305/2001 → variant
1308/2001 (Δ=+3). `novels_unverified` (decodes NOT matched to jt9 truth — the standing gate's
"hallucination" proxy): control 1865 → variant 1872 (Δ=+7). `novels_verified` (decodes matched to
truth but not counted in `truth_decodes_recovered`'s stricter accounting) also rose, 3255 → 3276.
`regression-flags` on both runs: `fixture_regression=false, false_positive_introduced=false,
snr_curve_regression_db=+0.00` (the fixture-level and NMS-style regression scanners see nothing
wrong — this is purely the bootstrap-CI standing gate on the hard-200 tier).

**Elapsed cost**: control 153.3s, variant 151.5s — essentially unchanged (variant even ran
marginally faster, well within run-to-run noise). This makes sense: `nsym_combined_llrs` only
executes as a fallback AFTER the 1-symbol attempt already failed, and even then its cost (up to
19×512 + 29×64 complex additions per candidate) is negligible next to the enclosing
`matched_demod_attempt`'s FIR-filter baseband extraction + fine-sync grid search, which dominate
that stage's cost. Unlike W3.3's own elapsed-cost failure, cost is NOT the reason this flip is
declined.

## The A/B: synth-clean (the SNR-sensitivity curve, per the brief's "hard-200 + synth curve")

`synth-clean` is a 550-WAV synthetic tier that sweeps encoded FT8 messages across a range of
full-band SNR values and reports the SNR at which OUR decoder recovers 50%/90% of attempts
(`snr_at_50pct_recovery_db`/`snr_at_90pct_recovery_db`) — the direct measurement of the "documented
~0.5–1 dB on stable signals" sensitivity claim from the design spec, independent of the jt9-truth
matched-vs-unverified accounting hard-200 uses.

```
./target/release/eval --tier synth-clean --mode ft8 --fine-sync-enabled \
    --output research/scorecards/w34-nsym-synthclean-control.json
./target/release/eval --tier synth-clean --mode ft8 --fine-sync-enabled --nsym-combining-enabled \
    --output research/scorecards/w34-nsym-synthclean-variant.json
./target/release/compare research/scorecards/w34-nsym-synthclean-control.json \
    research/scorecards/w34-nsym-synthclean-variant.json
```

```
A: research/scorecards/w34-nsym-synthclean-control.json (sha 5d3bf1e8, score 0.1486)
B: research/scorecards/w34-nsym-synthclean-variant.json (sha 5d3bf1e8, score 0.1526 +0.0039)

WINS:
  synth-clean           SNR@50%       -19.9 dB → -20.2 dB  (-0.3 dB)

REGRESSIONS:
  (none)

BOOTSTRAP CI:
  (skipped: neither scorecard carries `per_wav_records`. Re-eval with the Phase-B build to enable.)

CONFIG DIFF:
  decoder.decoder.nsym_combining_enabled           false → true
```

Raw fields: `snr_at_50pct_recovery_db` -19.909 → -20.172 (**-0.3 dB**, i.e. the decoder now recovers
50% of attempts at a HARDER/lower SNR — genuine sensitivity gain), `snr_at_90pct_recovery_db`
unchanged at -19.0 dB (the easier 90%-recovery threshold isn't moved — consistent with the
mechanism only helping at the margin, not across the board). `jt9_snr_at_50pct_recovery_db`
(jt9's own reference curve, unaffected by our flag as expected) unchanged at -21.31 dB in both
runs — a sanity check that nothing else in the harness drifted between the two runs.
`regression-flags` on both: `fixture_regression=false, false_positive_introduced=false,
snr_curve_regression_db=+0.00` — no scanner flags anything wrong, and `compare`'s own
`REGRESSIONS: (none)` line confirms it explicitly.

**This is the favorable-case result the design spec predicts**: -0.3 dB is somewhat smaller than
the spec's "~0.5–1 dB" ballpark but the same direction, on a genuinely stable-phase-like synthetic
corpus (no fading/multipath modeled), with zero measured downside on this tier. It directly
corroborates the TDD end-to-end fixture above (which found the exact -23 dB discriminating point on
a similar synthetic, stable-phase signal) at corpus scale. **This measurement, on its own, would
argue FOR the flip.** It is the hard-200 result above — a different, harder, more realistic corpus
— that decides against it; see Decision below for how the two are reconciled.

## Decision

**Decline the flip.** `Ft8Config::nsym_combining_enabled` stays `false`.

Two genuine, real measurements point in different directions: synth-clean shows a clean,
zero-regression -0.3 dB sensitivity win exactly where the design spec predicted one; hard-200 shows
a standing-gate failure. The plan's discipline treats the hard gate as decisive — it exists
precisely because a synthetic, favorable-channel corpus cannot certify real-world safety, and
hard-200's jt9-verified accounting is the closer proxy for "is this actually finding real signals
or just noise that happens to pass CRC." A single failing named criterion declines the flip
(precedent: W1.3, W2.3, W2.4, W3.3) even when another, less decisive measurement is positive.

The gate failure is unambiguous and mechanical, not a judgment call: the standing rule (design spec
§2/D0, this plan's Global Constraints) is "unverified-novel count increase ≤ 2×ΔTP". Measured
ΔTP=+3 (NOT statistically significant — the bootstrap CI's lower bound is exactly `+0.0`, so a true
effect of zero cannot be excluded) against Δunverified-novels=+7 (allowance +6, exceeded by 1) —
and the +28 "novel" delta (decodes that pass this run's own internal consistency checks but are
NOT matched to jt9 ground truth at all) IS statistically significant. Read together, this is exactly
the pattern the gate exists to catch: a mechanism that is finding MORE stuff, but the stuff it's
finding is disproportionately unverifiable relative to the (barely-there, not-significant) genuine
recall gain — the "hallucinating harder" failure mode the gate's own doc comment names.

This does **not** mean the mechanism is broken. The TDD evidence above proves it — both at the pure
hypothesis-energy level (hand-derived cross-check) and end-to-end through `decode_window` on a
genuinely stable-phase synthetic signal (4/4 curated seeds rescued, zero regressions) — that
nsym-combining does exactly what the spec says it should on a favorable (stable-phase, no
Doppler/multipath) channel. What the hard-200 corpus result shows is that mainline's real-world
signals are not uniformly "favorable" in this sense: widening the per-attempt hypothesis space
(64 or 512 candidates instead of 8) trades a small amount of stable-phase-signal recall for a
larger amount of confusable, hard-to-verify decodes on the messier majority of the corpus. This is
the exact risk the design spec's own Variant-B/C description flags ("more confusion if channel
phase rotates" / "sensitive to multipath / Doppler").

**Follow-up value, not wasted**: because `nsym_combining_enabled` can only ever fire when
`fine_sync_enabled` is ALSO true, and `fine_sync_enabled` is currently off in production for an
UNRELATED reason (W3.3b's budget-starvation finding — a real bounded `DecodeBudget` lets one
expensive matched-demod candidate consume a whole window's remaining budget), this task's own
result is somewhat moot today regardless: even had it cleared the gate, it would still be inert
until W3.3/W3.3b's budget issue is separately resolved. If that issue is ever fixed and
`fine_sync_enabled` reconsidered, this task's finding should be re-read alongside it: nsym-combining
would need either a narrower gate (fire only when some independent stability signal — e.g. low
per-symbol dt/df variance across the group, or a high sync-score margin — suggests the channel really
is stable) or acceptance of the unverified-novel cost measured here.

## Full test results

- `cargo test -p pancetta-ft8 --features transmit --lib`: **488 passed** (482 pre-existing + 6 new
  `nsym_combining_metric` tests), 0 failed.
- `cargo test --release -p pancetta-ft8 --features transmit --test decoder_refinement_tests
  w34_nsym_combining`: **2 passed** (RED + GREEN), 0 failed.
- `cargo test --workspace --features transmit`: full workspace suite (see below for the final
  confirming run before commit).
- `cargo fmt -p pancetta-ft8 -p pancetta-research -- --check`: clean.
- `cargo clippy -p pancetta-ft8 --features transmit --all-targets`: clean (only pre-existing
  `criterion::black_box` deprecation warnings in `benches/decoder_benchmark.rs`, unrelated to this
  task).

## Files changed

- `pancetta-ft8/src/decoder.rs`: `Ft8Config::nsym_combining_enabled` field (+ `Default` impl),
  `DecodeContext::nsym_combining_enabled` (3 construction sites), `matched_demod_attempt`'s new
  `nsym_combining_enabled` parameter + `tone_complex` retention + escalation `.or_else` chain, new
  functions `matched_demod_try_bp` (shared BP-attempt core, factored out of the previously-inline
  sequence), `nsym_hypothesis_energies`, `nsym_group_llrs`, `single_symbol_max_llrs`,
  `nsym_combined_llrs`; new unit test module `tests::nsym_combining_metric` (6 tests); the 4
  `matched_demod_attempt` call sites (`par_matched_demod_decode` + 3 rescue passes) each gained the
  new trailing argument.
- `pancetta-ft8/tests/decoder_refinement_tests.rs`: new `w34_nsym_combining` test module (RED/GREEN
  fixture + helpers, mirroring `w33_matched_demod`'s structure).
- `pancetta-research/src/decoder.rs`: `with_nsym_combining_enabled` builder.
- `pancetta-research/src/bin/eval.rs`: `--nsym-combining-enabled` CLI flag (struct field, parse arm,
  help text, populate, apply site).
- `research/scorecards/w34-nsym-hard200-{control,variant}.json`,
  `research/scorecards/w34-nsym-synthclean-{control,variant}.json` (new) — full corpus data.
- `research/experiments/2026-07-08-w34-nsym-combining.md` (this file, new).

## Self-review

- **Does the TDD metric test genuinely verify correct math (hand-derived cross-check), not just
  "runs without panicking"?** Yes — `nsym_hypothesis_energies_matches_hand_derivation_for_*` build
  the expected coherent-sum magnitude independently in the test body (indexing the known group's
  closed-form complex values directly + the crate's pre-existing, separately-tested Gray table),
  never calling the function under test to generate its own expected value, and check 4-8 specific
  hypotheses per test to epsilon=1e-9.
- **Does the end-to-end test genuinely discriminate (1-symbol fails, 3-symbol rescues)?** Yes — a
  real seed sweep (25 seeds × 3 SNR points) found the exact boundary (-23 dB) where the 1-symbol
  path fails a genuine subset (21/25) while nsym-combining recovers 100% (25/25), and the 4 curated
  seeds are exactly the ones where this split holds; both RED and GREEN tests were run and passed
  together, not assumed.
- **Is the A/B correctly isolating nsym_combining's effect specifically (fine_sync_enabled forced
  true in BOTH control and variant)?** Yes — both scorecards' `config.decoder.fine_sync_enabled`
  are `true` (confirmed via the scorecard JSON), and `compare`'s CONFIG DIFF line shows the ONLY
  differing field is `nsym_combining_enabled` — no other flag drifted between the two runs.
- **Is the fully-default (both flags false) config verified completely unaffected?** Yes,
  trivially and also directly: the 488-test lib suite (which exercises the decoder under
  `Ft8Config::default()` extensively) is unchanged at 488/488, and `matched_demod_attempt`'s new
  code only executes inside the `tone_complex.is_some()` branches / the escalation `.or_else`,
  which never even runs unless `nsym_combining_enabled` is `true` (and that itself never runs
  unless `fine_sync_enabled` is `true` — both `false` by default).
- **Full test suite green?** Yes (lib suite + the new E2E fixture pair confirmed above; full
  `cargo test --workspace --features transmit` re-confirmed before commit — see Full test results).

## Issues / concerns for whoever picks this up next

- The hard-200 hard-gate failure is small in absolute terms (Δunverified-novels=+7 vs. an
  allowance of +6 — a margin of exactly 1) and the ΔTP recall CI does not clearly exclude a real
  positive effect (lower bound is `+0.0`, not negative) — this is a genuinely close call, not an
  obviously-bad idea, and the standing gate is deliberately strict (see the plan's own
  "small-delta graduations" bootstrap-CI policy). A different corpus, a larger sample, or a
  narrower gating heuristic (see "Follow-up value" in Decision above) could plausibly flip this
  result. Reported honestly rather than rounded up or down.
- The synth-clean curve's win (-0.3 dB) has NO bootstrap CI (the tier carries no `per_wav_records`
  in this harness build), so it is a single point-estimate, not a statistically-bounded claim — it
  is reported as corroborating evidence for the mechanism's validity on favorable channels, not as
  an independent statistical justification either way. The hard-200 hard-gate failure is what
  actually decides the flip (per the plan's standing discipline: a single failing named criterion
  declines the flip, matching W1.3/W2.3/W2.4/W3.3's precedent).
- No corpus-level FP-on-noise (`noise_1000`) measurement is included: two initial attempts were
  killed mid-run to free CPU contention for the synth-clean legs above (`noise_1000` is 5x
  hard-200's WAV count and was taking >30 min of CPU time under `fine_sync_enabled`'s known ~2-3x
  slowdown with three concurrent eval processes contending for cores). Not attempted because it
  wasn't in this task's own brief ("hard-200 + synth curve") — it was an extra measurement borrowed
  from W3.3's precedent, and the hard-200 hard-gate failure already fully decides the flip without
  it. If ever revisited, run it in isolation (no concurrent eval processes) to avoid the same
  slowdown.
- `matched_demod_try_bp`'s extraction is a pure refactor of previously-inline logic (same
  decode_soft → CRC → parse → plausibility → gate sequence, same constants) — verified by diffing
  the 1-symbol call site's behavior against the pre-existing inline version conceptually (identical
  operations, same order), not by a byte-for-byte scorecard rerun of the OLD inline code path (the
  old code no longer exists as a separate path to compare against — `fine_sync_enabled=true` with
  `nsym_combining_enabled=false` in this task's own control run IS that path, and it reproduces
  W3.3's own control numbers exactly: 0.3261 raw / 0.3282 saturation-aware, matching
  `2026-07-08-w33-matched-demod-fine-sync.md`'s reported control score bit-for-bit).
