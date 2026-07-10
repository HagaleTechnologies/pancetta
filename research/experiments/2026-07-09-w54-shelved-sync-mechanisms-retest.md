---
slug: w54-shelved-sync-mechanisms-retest
mode: ft8
state: shelved
created: 2026-07-09T00:00:00Z
last_updated: 2026-07-09T00:00:00Z
branch: worktree-decoder-tp-sensitivity
parent_hypothesis: decoder-tp-sensitivity plan Task W5.4 (spec Section 6 —
  "re-run the shelved sync experiments that the flat cap previously killed
  (costas_partial_metric, costas_half_loop_disabled, costas_two_baseline,
  dt_history, relaxed_sync_near_partner) under the new selection scheme").
  Given W5.1 (per-bin candidate selection) and W5.2 (costas_two_baseline
  percentile normalization) were BOTH declined as defaults, the "new
  selection scheme" never materialized as a shippable default — the
  candidate-selection pipeline this task retests against is, in practice,
  the SAME flat-cap pipeline as before W5.1-5.3, plus W5.3's new
  default-OFF `extended_capture_window_enabled` opt-in. Each of the four
  remaining mechanisms (`costas_two_baseline` itself was W5.2's subject)
  was independently A/B'd against that TRUE current production default.
wild_card: false
delta_vs_main: |
  Four independent mechanisms, four independent decisions. Full numbers
  in-body below; summary:

  1. `costas_half_loop_disabled` (DECLINE, reconfirmed): hard-200 rec
     Δ=-43 (95% CI [-62,-26], significant), noise_1000 FP unchanged
     (1→1). Already independently retested once before in a SEPARATE
     plan (decoder-speed-overhaul Task 6, 2026-07-06,
     `research/experiments/2026-07-06-costas-half-loop.md`, rec Δ=-58
     there) — this task's retest on the current pipeline (30+ commits
     later) reconfirms the same verdict, same direction, same
     significance, smaller magnitude. A CONSISTENT, expected result.

  2. `costas_partial_metric_enabled` (DECLINE): hard-200 rec Δ=+3 (95%
     CI [-3,+9], NOT significant), novel Δ=-59 (significant decrease),
     decode_rate +0.0015 (win). noise_1000: false_positives_total 1→11
     (10x increase) — HARD GATE FAILURE, auto-flagged by `compare`. A
     purpose-built synthetic slot-edge corpus paired with Task W5.3's
     `extended_capture_window_enabled` (per this task's "pair with W5.3"
     instruction) confirmed the pattern at smaller scale: IDENTICAL
     recall at every one of 28 dt x lead cells (28/140 default-lead,
     51/140 extended-lead, both partial_off==partial_on exactly), while
     synthetic noise-only trials went 0/200 -> 1/200 (default lead) and
     0/200 -> 2/200 (extended lead), plus a real elapsed-cost increase
     (+17.6%/+6.8%). This flag's own doc comment previously (incorrectly)
     claimed "non-destructive... default true" — stale text predating
     the flat-cap-displacement shelving; corrected in this task.

  3. `dt_history_enabled` (DECLINE-AS-INERT): hard-200 rec Δ=+0 EXACT
     (every bootstrap resample identical) under the TRUE production
     default (`max_decode_passes=1`) — structurally a no-op, since the
     mechanism only touches the residual sync pass, unreachable at
     max_decode_passes=1 (same "dead path" class as
     `time_varying_subtraction_enabled`). A diagnostic retest with
     `max_decode_passes` forced to 3 (where the mechanism IS reachable)
     STILL showed rec Δ=+0 exactly (novel Δ=-9, 95% CI [-17,-3],
     significant but tiny). noise_1000 FP unchanged (1→1) in both
     configurations. No recall benefit found under either configuration.

  4. `relaxed_sync_near_partner_{hz_radius,score_delta}` (DECLINE, no
     real delta found): synthetic isolated-signal marginal-SNR test
     (partner reply exactly at the relaxed window's center, dt=0,
     well-aligned) across a well-resolved cliff (43.3%/30.0%/23.3%/
     6.7%/0.0% recall at -16.2/-16.4/-16.6/-16.8/-17.0 dB, mechanism
     off) showed IDENTICAL recall at delta in {0, -1.0, -2.0, -3.0} (the
     last is the maximum possible relaxation, min_sync_score=3.0 clamped
     at 0) at every SNR point -- the relaxed threshold never rescued a
     single trial even at maximum relaxation. Corpus-level sanity check
     (hard-200 with a FIXED partner freq forced across all 200 WAVs,
     radius=3.0, delta=-2.0): composite byte-identical to control
     (0.3126, zero decode-count change) -- expected dilution given no
     real per-WAV partner-frequency concept. noise_1000 under the same
     forced config: FP unchanged (1->1). No delta value found empirical
     support.
disposition: |
  All four mechanisms DECLINE as production defaults; none flipped.
  `Ft8Config::{costas_half_loop_disabled, costas_partial_metric_enabled,
  dt_history_enabled}` stay `false`;
  `relaxed_sync_near_partner_hz_radius` stays `None`,
  `relaxed_sync_near_partner_score_delta` stays `0.0`. Doc comments on
  all four fields updated in `pancetta-ft8/src/decoder.rs` to record this
  retest's numbers and reasoning (see per-field diffs in this commit).
  No source behavior changed anywhere -- byte-identical production
  decode path, confirmed by full `cargo test --workspace --features
  transmit` green throughout.

  This closes Workstream 5 of the decoder-tp-sensitivity plan
  (candidate pipeline restructure) with ALL FOUR of its exploratory
  tasks (W5.1-W5.4) declining every mechanism they tried to flip
  default-on, except W5.3's opt-in capture-window widening (a genuinely
  NEW capability, not a default-behavior change). The consistent theme:
  every one of these five mechanisms (per-bin selection, percentile
  normalization, half-loop disable, partial-BC metric, relaxed
  near-partner threshold) either regresses hard-200 recall outright, or
  increases noise-tier false positives, or shows no measurable benefit
  under any tested configuration -- the flat top-N candidate cap that
  spec Section 6 identified as the shared root cause of their historical
  shelving was never actually replaced (both of the two candidate-cap
  fixes explored in this workstream, W5.1 and W5.2, were themselves
  declined), so "retest under the new pipeline" in practice meant
  "retest under the same pipeline" for all four -- and the same negative
  verdicts held, which is the expected, self-consistent outcome given
  that premise.
follow_ups:
  - If a future effort ever wants to revisit any of these four
    mechanisms, the real prerequisite is still an actual working
    candidate-cap-displacement fix (spec Section 6's D5) -- W5.1's
    per-bin selection and W5.2's percentile normalization were the two
    candidates tried in this plan and both declined on their own merits.
    A different cap-displacement design (not yet attempted) would be the
    natural next lever, not another retest of these four consumers under
    the unchanged cap.
  - `costas_partial_metric_enabled`'s FP-on-noise blowup (1->11 on
    noise_1000) is a genuinely new, decisive, previously-undocumented
    data point -- worth remembering if anyone considers reviving this
    specific mechanism: it is not merely "no benefit," it actively
    increases false-alarm rate on pure noise by an order of magnitude.
  - The synthetic isolated-signal test design used for
    `relaxed_sync_near_partner` (and the W5.3-paired
    `costas_partial_metric_enabled` retest) cannot exercise the
    candidate-CAP-displacement failure mode these mechanisms are
    actually designed to fix (a single signal in an otherwise-quiet
    window never competes for the flat cap). A future retest that
    wanted to test the ACTUAL design hypothesis would need a
    multi-signal "crowded band" synthetic corpus (many strong signals
    plus one marginal one, all competing for the same top-N slots) --
    out of scope for this task's time budget, flagged for whoever
    revisits Section 6's D5 properly.
  - `w54_partial_metric_capture_window.rs` and
    `w54_relaxed_sync_near_partner.rs` (new
    `pancetta-research/examples/`) are kept as harness infrastructure
    (mirrors W5.3's `w53_slot_edge_bucket_recall.rs` precedent) in case a
    future cap-displacement fix wants to re-run these exact synthetic
    scenarios.
---

# Task W5.4: Retest the shelved sync mechanisms under the new pipeline

See `.superpowers/sdd/task-W5.4-report.md` for the full per-mechanism
investigation, exact commands, and self-review. This log exists per the
plan's standing "every A/B result gets an experiment log" rule, covering
all four mechanisms in one log (the task brief permits either one log or
four; this session chose one, covering all four decisively-independent
measurements).
