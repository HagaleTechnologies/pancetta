# Algorithm spec: per-pass parameter variation in multipass decoding

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- Primary doc (traceability only, NOT quoted): `docs/internal_pass_enhancement.md`
- Secondary docs: `docs/wsjtr.md` §"Decode Diversity (Feb 2026)"
- Code paths cited for traceability only, NOT to be read by implementer:
  `crates/jt9r/src/decoder.rs` (`decode_multipass_ft8`, `DecoderParams`
  fields `escalate_depth`, `relax_sync`, `vary_subtraction_order`, and the
  helper schedule fns `depth_schedule()`, `sync_schedule()`,
  `subtraction_order_schedule()`),
  `crates/wsjtr/src/main.rs` (CLI flag plumbing).
- Upstream lineage:
  - The amplitude-vs-power per-pass metric split is from WSJT-X
    `ft8_decode.f90` (pass 1: `imetric=1` amplitude; passes 2-3:
    `imetric=2` power).
  - The pass-3-on-zero-decodes skip-condition is from WSJT-X same file.
  - The depth-escalation, sync-relaxation, and subtraction-order-cycling
    schedules are wsjtr's elaborations not present in mainline WSJT-X.
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

A multipass FT8 decoder runs the decode pipeline several times against
progressively cleaner residual audio (after each pass subtracts its
decoded signals). Empirically, adding *more passes with identical
parameters* yields almost nothing past pass 3. Once the pipeline is
saturated, repeating it on slightly cleaner audio finds no new signals.

The wsjtr finding is that the diminishing returns are an artifact of
"carbon-copy passes" — each pass does exactly the same work. If instead
each pass does *something different* (different soft-bit metric, different
sync threshold, different candidate sort order, different subtraction
order), the same number of total decode evaluations finds materially more
unique signals. The cost is no higher; the diversity is what matters.

WSJT-X mainline already does a limited version of this: pass 1 uses
amplitude metrics, passes 2-3 use power metrics, and pass 3 is skipped
entirely if no decodes have been found yet (a cheap CPU save when nothing
on the band came through). wsjtr generalizes this into three independent,
composable per-pass schedules:

1. **Per-pass metric variation** — match WSJT-X's pass-1-amplitude /
   pass-2+-power split, instead of pancetta's current
   "every-pass-tries-everything".
2. **Per-pass depth escalation** — cheap BP-only on pass 1; expensive
   OSD+AP on later passes against clean residual.
3. **Per-pass sync threshold relaxation** — strict sync_min on pass 1;
   progressively looser on later passes as the residual gets cleaner.
4. **Skip-pass-on-zero-decodes** — if a pass finds nothing, skip the next
   one (the next pass's residual will be identical to this one, so the
   next pass is structurally guaranteed to also find nothing).

Wsjtr explicitly does NOT claim a measured corpus-scale win from
combining all four. It claims a *theoretical motivation* and a
*mechanism* — the experiment phase is enumerated as "Phase 1-5" in the
source doc and is left as the user's experiment. Pancetta should adopt
the same posture: implement the mechanism, then bootstrap-CI the actual
gain on hard-200 before shipping any of the per-pass schedules as
default-on.

## Algorithm description (PROSE ONLY — no code)

### Inputs

- A multipass decoder configuration with a baseline single-pass
  parameter set:
  - `passes`: total internal passes to run (default 3).
  - `depth`: decoder depth (BP-only, BP+OSD, BP+OSD+AP).
  - `sync_min`: minimum sync metric threshold for a candidate to enter
    the decode pipeline.
  - `metric_set`: which of the LLR variants to try per candidate.
  - `subtraction_order`: in what order to subtract decoded signals from
    the audio after the pass completes.
- Three opt-in flags:
  - `escalate_depth: bool` — enable per-pass depth schedule.
  - `relax_sync: bool` — enable per-pass sync_min schedule.
  - `vary_subtraction_order: bool` — enable per-pass subtraction-order
    schedule.
- A `skip_pass_on_zero_decodes: bool` flag (default true; this is the
  cheap WSJT-X-style guard).

### Outputs

- The final set of decoded signals across all passes (deduplicated),
  exactly as the multipass loop produces today. The *contents* may
  differ from baseline (more unique decodes, ideally with no
  regressions); the *interface* does not change.

### Steps

The multipass loop is unchanged in structure: for each pass, build a
candidate set on the current residual, decode the candidates, subtract
the new decodes, advance to the next pass. The change is **what
parameters each pass uses**.

1. **Pass-count default.** When any of the three variation flags are
   enabled and `passes` is not explicitly set, default `passes` to 4
   (instead of the usual 2-3). This gives the schedules room to vary
   meaningfully. If `passes` is explicitly set, honour it; if the
   schedule is shorter than `passes`, repeat the schedule's last value
   for remaining passes; if the schedule is longer, truncate.

2. **Per-pass metric selection (match WSJT-X).** This is conceptually
   the simplest schedule and lives outside the three opt-in flags
   above (it is always-on, matching WSJT-X mainline). Pancetta
   currently runs every pass with **all** LLR variants (5 amplitude +
   5 power = 10 variants). The new behaviour:
   - Pass 1: amplitude metrics only (5 variants).
   - Pass 2: power metrics only (5 variants).
   - Pass 3+: power metrics only.

   Each pass thereby does half the per-candidate work, which leaves
   budget for more passes (or for the deeper-on-later-passes schedule
   below).

3. **Per-pass depth escalation (`escalate_depth=true`).** When
   enabled, scale `depth` up across passes from a floor of 1 to the
   configured ceiling. Concrete schedule, parametrized by the
   ceiling (`base_depth`):

   | base_depth | Pass 1 | Pass 2 | Pass 3 | Pass 4+ |
   |------------|--------|--------|--------|---------|
   | 1          | 1      | 1      | 1      | 1       |
   | 2          | 1      | 2      | 2      | 2       |
   | 3          | 1      | 2      | 3      | 3       |

   Rationale: strong signals are decodable by BP alone (depth=1).
   Subtracting them early means later, expensive OSD/AP passes
   operate on a much cleaner residual where their effective gain is
   highest.

4. **Per-pass sync threshold relaxation (`relax_sync=true`).** When
   enabled, multiply `sync_min` by a decreasing schedule, clamped at
   `sync_min · 0.5` as a floor (don't let it go arbitrarily low).
   The wsjtr schedule:

   | Pass | Multiplier |
   |------|------------|
   | 1    | 1.00       |
   | 2    | 0.85       |
   | 3    | 0.75       |
   | 4    | 0.65       |

   Concretely with the default `sync_min = 1.2`: `1.20, 1.02, 0.90,
   0.78`. The intuition: after each pass subtracts strong signals,
   the local noise floor drops, so signals that were just below
   threshold can now appear as valid candidates. The 0.5 floor
   prevents pure noise from drowning the candidate list on very late
   passes.

5. **Per-pass subtraction order cycling (`vary_subtraction_order=true`).**
   When enabled, cycle through subtraction-order variants across
   passes:

   | Pass | Subtraction order                                    |
   |------|------------------------------------------------------|
   | 1    | `decode_order` (default: subtract in decoded order)  |
   | 2    | `snr_asc` (subtract weakest decode first)            |
   | 3    | `snr_desc` (subtract strongest decode first)         |
   | 4    | `freq_asc` (subtract in ascending-frequency order)   |

   The mechanism: different subtraction orders produce different
   subtraction artifact patterns. wsjtr's diversity experiments found
   `snr_asc` to be the best single variant (+2 decodes, 0 lost on
   one window). Cycling provides diversity *between passes* — even if
   one order is best on average, the others surface signals the best
   order misses.

6. **Skip-pass-on-zero-decodes (`skip_pass_on_zero_decodes=true`).**
   At the end of each pass, check if any new decodes were added
   relative to the pass's input set. If zero new decodes AND the
   audio extent is fully available (i.e., not in a live-mode early
   checkpoint), terminate the multipass loop early. The next pass's
   residual would be identical to this one (no signals were
   subtracted), so any further passes are structurally guaranteed to
   also find nothing. This is the cheap CPU save matching WSJT-X
   mainline's pass-3-skip behaviour, generalized to arbitrary
   pass index.

7. **Composability of the three opt-in schedules.** All three flags
   are independently toggleable and apply to non-overlapping
   parameters. When all three are enabled together, the wsjtr
   "smart passes" composite schedule emerges naturally — pass 1
   is cheap BP-only with strict sync; pass 4 is expensive OSD+AP
   with relaxed sync and a different subtraction order — without
   any single composite flag being required.

8. **Verbose-log per-pass parameters.** Whenever any of the schedules
   are active, the per-pass parameter set (depth, sync_min,
   subtraction_order, metric_restriction) should be logged at the
   start of each pass. This is essential during experimentation: it
   lets the experimenter verify that the schedule is being applied
   correctly and trace why a given decode appeared on a given pass.

### Numerical constants (facts, not expression)

- Default pass count when no variation flag is set: 2 (depth ≤ 1) or
  3 (depth ≥ 2), matching the standard multipass auto behaviour.
- Default pass count when any variation flag IS set, and `passes`
  is not explicit: **4**.
- Sync-relaxation multiplier schedule: `1.00, 0.85, 0.75, 0.65`,
  clamped at `sync_min · 0.5`.
- Sync-relaxation example with `sync_min = 1.2`: `1.20, 1.02, 0.90,
  0.78`.
- WSJT-X `syncmin` default: 1.3 (depth ≥ 3); 2.1 (depth ≤ 2).
- WSJT-X amplitude/power split: pass 1 amplitude, passes 2-3 power.
- LLR variant counts: 5 amplitude + 5 power = 10 total in pancetta's
  current pipeline (the WSJT-X split would reduce per-pass work
  by ~50%).
- Pass-skip rule: terminate multipass when current pass adds 0 new
  decodes AND audio extent is fully available.
- Time-budget reference (per wsjtr, typical wall-clock on modern
  multi-core CPU, 1 window):
  - `-p 3 -d 3,3,3` uniform-depth: ~800 + 700 + 600 = ~2100 ms.
  - 4-pass escalating: ~150 + 200 + 600 + 600 = ~1550 ms (actually
    *faster* despite one more pass).

### Edge cases

- **Schedule shorter than pass count.** Repeat the last value (e.g.,
  if `escalate_depth` schedule is `[1, 2, 3, 3]` and `passes=6`, use
  `[1, 2, 3, 3, 3, 3]`).
- **Schedule longer than pass count.** Truncate.
- **All variation flags disabled.** Behaviour is identical to
  pre-existing multipass (uniform parameters across passes). The
  variation code is purely opt-in.
- **`skip_pass_on_zero_decodes` interaction with live-mode early
  checkpoints.** Live-mode early passes (running on a 9.6s prefix of
  audio) MUST NOT terminate the loop early because more audio is
  still arriving. Only apply the skip rule when the audio extent is
  fully available (i.e., full 13.5s or 15s prefix). Otherwise the
  decoder would miss decodes that simply hadn't yet been recorded.
- **Sync floor clamp.** Without the 0.5· clamp, a long pass schedule
  would drive `sync_min` arbitrarily low (`1.00 · 0.65^N`), producing
  pure-noise candidates on late passes. The clamp is essential.
- **Cumulative subtraction artifacts.** Each subtraction is imperfect
  and leaves residual energy. More passes = more accumulated
  artifacts. The wsjtr DT-refinement-during-subtract mechanism is the
  natural companion to per-pass variation (the two specs were
  designed together).
- **Performance regression risk on slow tier.** A 4-pass escalating
  schedule on the Slow hardware tier can blow the slot budget. Per
  hb-216, on Slow tier pancetta already forces
  `max_decode_passes=1, osd_depth=Some(1)`. The variation flags
  should be **force-disabled on Slow** regardless of operator config
  — there's only one pass, so there's nothing to vary.
- **FP risk from relaxed sync.** Lower sync threshold means more
  noise-only candidates, each capable of producing an OSD false
  positive (cf. the wsjtr "OSD False Decode Analysis" finding). The
  WSJT-X-style OSD distance-tracking fix is the primary defense
  (pancetta should confirm it has the analogous behaviour). hb-103
  content-score, hb-052 callsign-continuity, hb-058 /R-filter
  downstream catch most surviving FPs.

## Conflict with pancetta's existing mechanisms

Pancetta's current internal multipass — `pancetta-ft8/src/decoder.rs` —
runs `max_decode_passes` iterations with uniform parameters. The
per-pass-metric split is not present (every pass tries all 10 LLR
variants). The depth-escalation, sync-relaxation, and
subtraction-order-cycling schedules are not present. The
skip-pass-on-zero-decodes optimization is partially present (the
multipass loop terminates if the *current* pass added zero new
decodes, but it does not match WSJT-X mainline's specific skip
condition for pass 3).

Possible conflicts to think through:

1. **Cost budget.** The per-pass-metric split (amplitude pass 1, power
   pass 2+) is a *cost reducer*, not a cost adder — it halves
   per-candidate decode work. This is the cleanest win and the safest
   ship. The escalation/relaxation/cycling schedules trade some
   pass-1 work (less) for some pass-N work (more); net cost is
   roughly neutral. The skip-pass-on-zero-decodes optimization is a
   pure savings on quiet bands.

2. **Interaction with hb-216 hardware tier.** On Slow tier, hb-216
   forces `max_decode_passes=1`. All four mechanisms in this spec are
   no-ops with a single pass and should be inert. Add explicit
   guards in the multipass loop so that even if the operator sets
   `escalate_depth=true` on Slow, the actual depth schedule
   collapses to `[1]`. On Moderate/Fast, all four are eligible.

3. **Interaction with hb-091 scoped fast path.** Orthogonal. hb-091
   short-circuits a hot inner loop in the candidate stage; this
   spec governs which *parameters* each pass uses, not whether the
   inner loop is short-circuited. Combine freely.

4. **Interaction with hb-103 content score / hb-052 callsign
   continuity / hb-058 /R filter.** These are post-decode FP filters
   that sit downstream of the multipass loop. They will catch any
   FPs introduced by relaxed sync on later passes. Mild positive
   interaction: cleaner residuals from depth escalation may slightly
   reduce ghost-candidate FPs, easing the filters' work.

5. **Interaction with hb-237 cross-sequence A7.** Mildly positive.
   Per-pass variation can surface additional decodes that feed the
   A7 seed table, enabling more cross-sequence decodes in subsequent
   windows. The two work at different scopes — per-pass variation
   is intra-window; A7 is inter-window — so they compose cleanly.

6. **Interaction with hb-217 RR73 fix.** Independent. RR73 fix is in
   the parser; per-pass variation is in the decoder loop. No conflict.

7. **Interaction with neural OSD.** Slightly important. Pancetta's
   neural OSD reduces OSD trials by ~600× via learned bias. If
   per-pass depth escalation pushes OSD to later passes, the neural
   bias is still valid (it conditions on the LLR shape, not on
   pass index). No special handling.

8. **FP regression risk.** This is the main risk. Relaxed sync on
   later passes is the most exposed mechanism. Bootstrap-CI gate
   before shipping any of the three opt-in flags as default-on.

## Estimated Rust port effort

- New fields on the multipass-decoder config struct (likely
  `Ft8Config` or a sub-struct):
  - `escalate_depth: bool`, `relax_sync: bool`,
    `vary_subtraction_order: bool`, `skip_pass_on_zero_decodes: bool`,
    `use_wsjtx_metric_split: bool`. ~25 LOC.
- Schedule helper functions:
  - `depth_schedule(base_depth: u8, passes: u32) -> Vec<u8>`. ~20 LOC.
  - `sync_min_schedule(base: f32, passes: u32) -> Vec<f32>`. ~20 LOC.
  - `subtraction_order_schedule(passes: u32) -> Vec<SubtractionOrder>`.
    ~15 LOC.
  - `metric_restriction_schedule(passes: u32) -> Vec<MetricSet>`. ~15 LOC.
- Multipass loop changes:
  - Pull per-pass parameters from the schedules instead of static
    fields. ~40 LOC.
  - Add skip-pass-on-zero-decodes guard at end of each pass body. ~10 LOC.
  - Force schedules to length-1 (collapse to baseline) when
    `max_decode_passes == 1`. ~10 LOC.
- Subtraction-order plumbing: pancetta currently subtracts in
  decode-order. Add an enum `SubtractionOrder { DecodeOrder, SnrAsc,
  SnrDesc, FreqAsc }` and a per-call switch in the subtractor. ~40 LOC.
- Verbose logging: ~20 LOC.
- Unit tests:
  - Schedule generator correctness (each combination of
    `(base_depth, passes)` produces the expected schedule). ~50 LOC.
  - End-to-end: confirm each flag affects the per-pass parameters
    observed by the decoder. ~80 LOC.
  - Skip-pass-on-zero-decodes terminates the loop correctly. ~30 LOC.
- Total: ~250 LOC production + ~160 LOC tests.
- 2 iter sessions:
  1. Schedules + composability + unit tests; default off.
  2. Hard-200 eval per-flag-individually and per-flag-combined;
     bootstrap-CI; decide which to ship by default.

## Implementation notes for the implementer thread

- **Ship the cheap WSJT-X-style metric split first.** It is
  independent of the three opt-in flags, it is a cost *reducer*, and
  matching WSJT-X mainline behaviour is the safest possible change.
  If hard-200 shows any regression from the split, something else is
  wrong; otherwise it's the floor for everything else.

- **Implement the four boolean flags as orthogonal.** Each governs a
  separate schedule (depth, sync, subtraction order, skip). Do NOT
  introduce a single composite "smart_passes" flag — the wsjtr
  experiment shows the flags are best evaluated and shipped
  individually.

- **Hard-200 eval per flag.** Run `cargo run -p pancetta-research`
  (or whatever the equivalent eval harness command is) with each
  flag toggled individually, then in pairwise combinations, then all
  together. Record decode counts, FP counts, and bootstrap-CI for
  each. Ship only the configurations whose CI lower bound is
  positive.

- **Tier-default policy after eval.** Recommended starting position:
  - Fast: enable WSJT-X metric split; leave the three opt-in flags
    OFF until eval data justifies them.
  - Moderate: same as Fast.
  - Slow: force all four off (single pass anyway).

- **Skip-pass-on-zero-decodes is the safest opt-in.** Pure CPU
  savings on quiet bands, zero risk of decode loss (by definition,
  if the pass found nothing, the next pass with the same parameters
  on the same residual also finds nothing). Ship this default-on
  outside of any of the experimental gating, after a one-batch
  verification it doesn't trigger spuriously on edge cases (e.g.,
  early checkpoints in live mode where the audio is still
  arriving).

- **Subtraction-order cycling requires care.** The pancetta
  subtractor currently subtracts in decode-order. Adding the
  alternatives requires sorting the decode list before iterating.
  Make sure the SNR field is reliable — early-pass decodes may
  report less-reliable SNR estimates. The cycling order should NOT
  affect decode results within a pass (only the residual entering
  the next pass), so unit tests can verify per-pass dedup.

- **Verbose logging is non-optional during eval.** Print per-pass
  `(pass_idx, depth, sync_min, subtraction_order, metric_set,
  decodes_found_this_pass, decodes_total_so_far)` at INFO level when
  any variation flag is enabled. Without this, debugging an
  unexpected hard-200 regression is impossible.

- **Test fixture for the skip rule.** Construct a synthetic WAV with
  exactly zero decodable signals. Confirm the multipass loop
  terminates after pass 1 (rather than running all 3 or 4).
  Construct a synthetic WAV with one strong signal that is decoded
  on pass 1 and subtracted cleanly. Confirm the loop terminates
  after pass 2 (because pass 2's residual has nothing).

- **Citation hygiene.**
  - The amplitude/power metric split: cite as
    `WSJT-X-aligned (ft8_decode.f90 imetric split)`.
  - The skip-pass-on-zero-decodes: cite as
    `WSJT-X-aligned (ft8_decode.f90 pass-3 skip generalization)`.
  - The depth-escalation, sync-relaxation, subtraction-order-cycling
    schedules: cite as
    `wsjtr-inspired (per-pass parameter variation)`. The mechanism
    is wsjtr's, not WSJT-X mainline's; pancetta's contribution
    beyond wsjtr is the bootstrap-CI graduation gate.
