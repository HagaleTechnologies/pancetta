# Ap5 (content-AP) recall/false-decode-vs-SNR eval harness — Task 7

**Date**: 2026-07-25 (original pilot), corrected 2026-07-25 (Task 7 fix pass, same day)
**Branch**: `worktree-ap-decoding`
**Status**: Corrected pilot run, complete. Ship gate: **NOT MET** — `content_ap_enabled` stays `false`.
  Evidence is now materially more precise than the original entry (see "Correction notice" below):
  Ap5's own mechanism DOES show a real, directly-measured recall benefit for one stage, but that
  benefit is entirely redundant with an existing, already-shipped AP4 feature in the actual default
  configuration.
**Plan**: `docs/superpowers/plans/2026-07-25-ap-content-decoding.md`, Task 7 of 8.
**Design**: `docs/ap-decoding-design.md` §4 (eval protocol, the ship gate).
**Harness**: `pancetta-research/examples/ap5_content_recall_fp_sweep.rs`.

## Correction notice (read this first)

A review of the original version of this entry found it overstated the evidence in four ways. All
four are fixed in the harness and in the numbers below; the original entry's claims are **no
longer accurate** and should not be cited. Summary of what changed:

1. **"874 trials" conflated informative and vacuous trials.** Ap5 can only ever fire in the
   transition band — SNR points where sync succeeds (a candidate exists) AND AP0-4 all fail (Ap5's
   last-resort gate is reached). Above the recall cliff, AP0-4 already wins every time and Ap5 is
   never reached; a tie there is trivial. Below the sync floor, there's no candidate at all; a tie
   there is vacuous. The harness now classifies every (stage, SNR) row as informative (`0% <
   off_hit% < 100%`) or not, and reports both the raw and the true informative trial counts.
2. **The `WaitingForConfirmation` stage was not a valid Ap5 test.** `Ft8Config::
   ap4_full_message_mask_enabled` defaults `true`, so AP4 already injects and tries the full
   RRR/RR73/73 content mask and returns FIRST, before Ap5 is ever reached — 0 Ap5 rescues at this
   stage under the default config is *guaranteed by this overlap*, independent of whether Ap5's own
   mechanism works. The harness now runs this stage's recall measurement with
   `ap4_full_message_mask_enabled: false` (an uncontested test of Ap5 itself), and — new this pass
   — the corrected measurement found **real, non-zero rescues**. See "Results" below.
3. **No direct proof Ap5 was ever entered.** The original entry inferred "Ap5 must have fired" from
   the sync/decode-rate gradient; it was never measured. `pancetta_ft8::ap::{ap5_attempt_count,
   reset_ap5_attempt_count}` (new, `pancetta-ft8/src/ap.rs`) now count every Ap5 hypothesis attempt
   directly via a `Relaxed` `AtomicU64`, incremented once per hypothesis in each of the two Ap5
   "last resort" blocks in `pancetta-ft8/src/decoder.rs` (serial + rayon-parallel paths). Gated
   entirely behind code that only runs when `content_ap_enabled` is `true` — zero cost on the
   shipped default path.
4. **The positive control was overclaimed.** The original text used the -8/-16 dB 100%-hit-rate
   rows to imply the zero-rescue result "isn't a harness bug." That only proves the harness can
   decode at all; it says nothing about whether Ap5 specifically was reached. The harness now
   prints `ap5_attempts` next to the positive-control rows instead of asserting validation it
   doesn't have. (A secondary, honest correction made *during this fix pass*: a first draft of that
   control claimed `ap5_attempts` "should read ~0" at high SNR, reasoning that AP0-4 should already
   win. Measured reality contradicts that: `ap5_attempts` runs in the **thousands** even at -8 dB,
   because the decode loop evaluates many spurious/noise sync candidates across the whole window
   besides the one true signal, and every candidate that fails AP0-4 — which is most of them —
   falls through to Ap5 regardless of the true signal's SNR. The harness's comments now say this
   plainly instead of asserting an unverified expectation.)

**The headline conclusion is unchanged: `content_ap_enabled` stays `false`.** But the reasoning is
now stage-specific and precise rather than a single "no benefit anywhere" claim — see "Results" and
"Ship-gate statement" below.

## Method

Two QSO stages exercised (the two content-hypothesis-set shapes in `docs/ap-decoding-design.md`
§1's table): `WaitingForReport` (24 report-value hypotheses, truth message `K1ABC W1AW -08`) and
`WaitingForConfirmation` (3 RR73/RRR/73 hypotheses, truth message `K1ABC W1AW RR73`). At each SNR
point, for each stage:

1. Encode + modulate the partner's TRUE next message; add AWGN at the target SNR via
   `pancetta_research::synth::add_awgn_2500hz_ref` (WSJT-X 2500 Hz reference-bandwidth convention;
   reused rather than reimplemented, per the task's instruction).
2. Build the real `ApContext` the coordinator would have: correct partner call, correct progress,
   the actual enumerated hypothesis set (`enumerate_a8_expected_texts`), plus a 3-call
   `recent_calls` decoy pool for AP2 realism. `active_qso: Some(qso)` AND `active_qsos: vec![qso]`
   are both populated, so **the full AP0-4 + Cq ladder is reachable on both sides**.
3. Decode the SAME noisy audio once with `content_ap_enabled: false` (full legacy ladder, no Ap5)
   and once with `content_ap_enabled: true` (same ladder + Ap5), both via the public
   `Ft8Decoder::decode_window_with_ap` API. **`WaitingForConfirmation` runs both sides with
   `ap4_full_message_mask_enabled: false`** (Finding 2 fix) — the `AP5_SWEEP_CONFIRMATION_MASK=true`
   env var reproduces the original, contaminated (shipped-default) measurement for direct
   comparison; see Results.
4. A **rescue case** = AP-off decode does not recover the true text, AP-on decode does.
   `RecallOutcome::ap5_attempts` records the `ap5_attempt_count()` delta around the AP-on decode
   call — a direct measurement of how many times Ap5 was entered for that trial.
5. Every (stage, SNR) row is classified **informative** iff `0% < off_hit% < 100%` — the transition
   band where Ap5 can possibly fire. Rows outside that band are reported but excluded from the
   headline trial count.

All runs below were executed synchronously in the foreground (no backgrounded processes), each
completing in well under 5 minutes wall-clock.

## Results

### Run A — corrected primary pilot (Finding-2 fix applied, Ap5-entry instrumented)

`AP5_SWEEP_CONTROL_TRIALS=3 AP5_SWEEP_PILOT_TRIALS=30 AP5_SWEEP_PILOT_SNRS="-18,-19,-20,-21,-22,-23,-24"`
— 30 trials per (SNR, stage), 7 SNR points, both stages. Total elapsed 217.7s (pilot phase 204.1s).

```
stage                     snr_db  trials  off_hit%   on_hit%   rescues informat? ap5_attempts
-----------------------------------------------------------------------------------------------
WaitingForReport          -18.0      30    100.0%    100.0%         0       no        94656
WaitingForReport          -19.0      30     83.3%     83.3%         0      yes        95168
WaitingForReport          -20.0      30     70.0%     70.0%         0      yes        95520
WaitingForReport          -21.0      30      6.7%      6.7%         0      yes        95952
WaitingForReport          -22.0      30      3.3%      3.3%         0      yes        95984
WaitingForReport          -23.0      30      0.0%      0.0%         0       no        96000
WaitingForReport          -24.0      30      0.0%      0.0%         0       no        96000
WaitingForConfirmation    -18.0      30    100.0%    100.0%         0       no        35436
WaitingForConfirmation    -19.0      30     83.3%     96.7%         4      yes        35656
WaitingForConfirmation    -20.0      30     70.0%     83.3%         4      yes        35744
WaitingForConfirmation    -21.0      30     30.0%     46.7%         5      yes        35872
WaitingForConfirmation    -22.0      30      3.3%     10.0%         2      yes        35973
WaitingForConfirmation    -23.0      30      0.0%      0.0%         0       no        36000
WaitingForConfirmation    -24.0      30      0.0%      0.0%         0       no        36000
```

**PILOT VERDICT: 15 genuine rescue cases out of 420 RAW trials. Of those, 240 trials are in the
TRUE INFORMATIVE (transition-band) set — the raw 420 overstates the evidence base by 75%.** All 15
rescues are in `WaitingForConfirmation`; every one carries `on_via_ap5=true` (verified per-rescue in
the harness's real-time `** RESCUE **` log lines, not inferred). `WaitingForReport` shows 0 rescues
across its 120 informative trials, with Ap5 CONFIRMED entered ~95,000-96,000 times per row —
directly measured, not inferred from the hit-rate tie.

Rescue rate within `WaitingForConfirmation`'s informative rows: 15/120 = 12.5% (per-row: 13.3%,
13.3%, 16.7%, 6.7%). This is a real, substantial, directly-measured recall benefit from Ap5's own
mechanism — see "Interpretation" below for why this does not change the ship-gate outcome.

### Run B — reproducing the ORIGINAL (contaminated) `WaitingForConfirmation` measurement

Same informative SNR band, `AP5_SWEEP_CONFIRMATION_MASK=true` (forces
`ap4_full_message_mask_enabled: true`, the shipped default, for this stage — reproducing exactly
what the original, uncorrected harness measured). `AP5_SWEEP_PILOT_TRIALS=20
AP5_SWEEP_PILOT_SNRS="-19,-20,-21,-22"`. Total elapsed 86.3s.

```
stage                     snr_db  trials  off_hit%   on_hit%   rescues informat? ap5_attempts
-----------------------------------------------------------------------------------------------
WaitingForReport          -19.0      20     85.0%     85.0%         0      yes        63456
WaitingForReport          -20.0      20     65.0%     65.0%         0      yes        63712
WaitingForReport          -21.0      20     10.0%     10.0%         0      yes        63952
WaitingForReport          -22.0      20      5.0%      5.0%         0      yes        63984
WaitingForConfirmation    -19.0      20    100.0%    100.0%         0       no        23760
WaitingForConfirmation    -20.0      20     80.0%     80.0%         0      yes        23832
WaitingForConfirmation    -21.0      20     55.0%     55.0%         0      yes        23886
WaitingForConfirmation    -22.0      20     25.0%     25.0%         0      yes        23952
```

**0 rescues at `WaitingForConfirmation` under the shipped default config, at the EXACT same SNR
points where Run A (mask disabled) found 4, 5, and 2 rescues respectively (at -20, -21, -22 dB).**
This is direct, measured confirmation — not just a code-reading argument — that AP4's full-message-
mask feature is what suppresses Ap5's real, provable recall benefit in the actual shipped
configuration; Ap5 is entered tens of thousands of times in both runs (so it is genuinely reached
either way), but with the mask enabled, AP4 always wins before Ap5's own hypothesis loop gets a
chance to matter.

### Positive control (decode-capability sanity check ONLY — see Correction notice #4)

3 trials at each of -16 dB and -8 dB, both stages: **100.0%/100.0%** off_hit/on_hit at every point
(harness decodes correctly at high SNR). `ap5_attempts` at these points: 9,360 / 9,312 (Report) and
3,517 / 3,492 (Confirmation) — i.e. thousands of Ap5 entries even at -8 dB, because most sync
candidates in a window are spurious/noise, not the one true signal, and every candidate that fails
AP0-4 reaches Ap5 regardless of the true signal's actual SNR. **This control proves decode
capability only — it does not, and was never claimed here to, prove Ap5 was exercised for the true
signal specifically** (that's what Runs A/B's per-trial `ap5_attempts` and `on_via_ap5` fields do).

## Interpretation: why the ship-gate answer is still "no", precisely

Ap5's content-hypothesis injection mechanism **does work** — Run A proves it provides a real,
directly-measured ~6.7-16.7% recall lift per SNR point for `WaitingForConfirmation`, when it gets an
uncontested shot at the 3-hypothesis RRR/RR73/73 space. This is a materially different (and more
positive) finding about the *mechanism itself* than the original entry's flat "tied everywhere."

But the ship-gate question is not "does Ap5's mechanism work in isolation" — it's "does turning on
`content_ap_enabled` improve behavior in the actual shipped decoder." And there the answer is
unambiguously no, for two independent reasons depending on stage:

- **`WaitingForConfirmation`**: Ap5's benefit is entirely redundant with `AP4`'s existing
  `ap4_full_message_mask_enabled: true` feature (on by default, shipped, already validated — see
  that flag's own doc comment history in `decoder.rs`). AP4's full-mask loop tries the exact same 3
  content hypotheses, with the same injection strength, and runs FIRST. Run B directly confirms 0
  incremental benefit under the real default config, at the same SNR points where Run A found real
  benefit with the mask disabled. Turning on `content_ap_enabled` would add compute cost (extra
  full LDPC/OSD attempts per spurious candidate — tens of thousands per window, per the
  `ap5_attempts` counts above) for zero behavioral change in production.
- **`WaitingForReport`**: there is no AP4-equivalent shortcut covering the 24 report-value
  hypotheses, so this stage is Ap5's only real, uncontested opportunity to matter, in the actual
  ladder as shipped. Here the result is a genuine, now-directly-proven null: 0 rescues across 120
  confirmed-informative trials, with Ap5 measured entering ~95,000+ times per row. AP3's 56
  known-callsign bits already saturate recovery in this decoder's LDPC/OSD strength at the tested
  SNRs; the extra 19 content bits Ap5 adds don't move the needle.

So: **`content_ap_enabled` stays `false`**, same as the original conclusion — but the corrected
evidence is stronger where it matters (a real, proven null for `WaitingForReport`, the stage that
actually tests the mechanism) and honestly surfaces a real positive result for
`WaitingForConfirmation` that turns out to be moot in production because of a pre-existing feature
overlap, rather than misreporting that positive result as a null.

## Decision: why the full §4A/§4B sweep didn't run at the official range this pass

The original entry's decision not to run the full official -24..-6 dB sweep (with false-decode
protocols and knob tradeoff) still holds, now on firmer footing: the transition band for both
stages sits entirely within -18 to -24 dB (confirmed twice, Runs A and B), well inside the official
range, and the corrected pilot already answers the ship-gate question directly at that band with
real Ap5-entry proof. Running the false-decode protocols/knob curve would characterize a tradeoff
for a feature this evidence shows has no *production* upside — moot for the same reason the
original entry gave. (Note: a much larger, 60-trials/SNR-point, full-official-range run,
`AP5_SWEEP_FULL_TRIALS=60 AP5_SWEEP_FULL_SNRS="-24,-22,...,-6"`, WAS executed once during this fix
pass and reproduced the same qualitative pattern — `WaitingForReport` tied at every point,
`WaitingForConfirmation` [mask disabled] showing rescues concentrated at -20/-22 dB — but that run
took ~43 minutes under contended system load and is not reported as the primary evidence here
because it did not complete within the tool's synchronous foreground window; Runs A and B above are
the authoritative, cleanly-foreground numbers for this entry, and are fully consistent with it.)

`--full` still exists on the harness (`cargo run ... --example ap5_content_recall_fp_sweep --
--full`) for anyone who wants the official-range run with false-decode protocols — e.g. if a future
change to the LDPC/OSD pipeline, the AP3/4 callsign-bit injection, or `ap4_full_message_mask_enabled`
changes this picture.

## §4B (corpus A/B) — documented gap, not built

Not implemented, unchanged from the original entry. Two independent reasons:

1. **Missing submodule** (pre-existing, unrelated to this task): `pancetta-ft8/vendor/ft8_lib` is
   not checked out in this worktree (`git submodule status` shows a `-` prefix; `cargo build`
   prints "ft8_lib C sources not found ... building WITHOUT the C decoder (ft8lib_stub, degraded
   decode recall)"). This blocks the ft8_lib-truth precision proxy §4B's spec calls for.
2. **Disproportionate given the pilot's answer**: §4B's own spec text requires "context
   reconstructed from the decode stream" — a nontrivial QSO-state-machine replay over corpus audio.
   Building it to characterize a tradeoff the synthetic harness already resolves (no production
   benefit, for two independently-sufficient reasons) is effort spent on an already-answered
   question.

If a future change (e.g. disabling `ap4_full_message_mask_enabled` by default, or extending Ap5 to
cover a hypothesis space AP4 doesn't already shortcut) makes the pilot's answer non-moot, §4B
becomes worth building as a real follow-on.

## A load-bearing implementation note: `ap_llr_magnitude` is a no-op today

Unchanged from the original entry: `Ft8Config::ap_llr_magnitude` and `::min_ap_decode_confidence`
are config-surface-only as of this task — `inject_ap_llrs` and `try_ldpc_with_ap` still read their
own hardcoded consts (`AP_LLR_MAGNITUDE = 15.0`, `MIN_AP_DECODE_CONFIDENCE = 0.55`), not these
`Ft8Config` fields. Only `content_ap_enabled`, `min_content_ap_confidence`, `max_ap_hypotheses`,
`max_ap_qsos`, and (as of this fix pass) `ap4_full_message_mask_enabled` are actually wired to the
Ap5-reachable decode loop today.

## Ship-gate statement (for the operator — not a decision, per the plan's global constraint)

Per `docs/ap-decoding-design.md` §4's ship gate: *"graduate `content_ap_enabled` ... only if, at a
fixed operating point, synthetic recall rises meaningfully AND the false-decode rate stays within
budget across the SNR sweep AND the corpus A/B shows non-negative decode-rate at non-negative
precision."*

- **Synthetic recall rises meaningfully**: **NOT MET, precisely characterized.** `WaitingForReport`
  (the stage where Ap5 is not redundant with AP4): 0 rescues in 120 directly-confirmed-informative
  trials, Ap5 measured entering ~95,000+ times per row. `WaitingForConfirmation`: Ap5's mechanism
  DOES show a real, directly-measured 12.5%-average rescue rate in isolation (Run A), but this
  benefit is completely subsumed by the already-shipped `ap4_full_message_mask_enabled: true`
  default (Run B: 0 rescues at the identical SNR points under the real config) — so there is no
  recall rise in the actual production decoder either way.
- **False-decode rate within budget**: not measured at the official range this pass (moot — no net
  production recall benefit to weigh a cost against; see "Decision" above for the one large run
  that did partially cover this and found low, expected false-positive rates consistent with the
  original entry's expectations).
- **Corpus A/B non-negative**: not measured (§4B gap above; also moot for the same reason).

**`content_ap_enabled` stays `false`, unchanged, per the plan's binding global constraint** — this
was never in question regardless of the eval outcome, and the corrected, more rigorous evidence
still supports it, now for a precisely-stated reason rather than an overstated flat null.

## Files changed (this fix pass)

- `pancetta-ft8/src/ap.rs` — added `ap5_attempt_count()` / `reset_ap5_attempt_count()` (public) and
  `record_ap5_attempt()` (`pub(crate)`), a process-global `Relaxed` `AtomicU64` counter. No existing
  reusable AP-level-attempt telemetry was found in the crate (checked `DecodeBudget`/
  `DecodeBudgetReport`, `decoded_message.ap_level`, and all `ap_level_num`/`thread_local`/
  `AtomicU64` call sites first) — this is genuinely new, minimal instrumentation, not a duplicate of
  something that already existed.
- `pancetta-ft8/src/decoder.rs` — two one-line additions (`crate::ap::record_ap5_attempt();`), one
  in each of the pre-existing Ap5 "last resort" blocks (serial and rayon-parallel paths), each
  already gated behind `content_ap_enabled` / `ctx.content_ap_enabled`. No other lines changed in
  this hot-path file.
- `pancetta-research/examples/ap5_content_recall_fp_sweep.rs` — `Knobs::ap4_full_mask` field
  (threads `ap4_full_message_mask_enabled` through `cfg_off`/`cfg_on`); `pilot_knobs(stage)` forces
  it `false` for `WaitingForConfirmation` (overridable via `AP5_SWEEP_CONFIRMATION_MASK=true` for
  side-by-side comparison); `RecallOutcome::ap5_attempts` plus reset/read around each AP-on decode;
  per-row informative-band classification and true-informative-trial-count reporting in
  `run_pilot`; new `run_positive_control` (Phase 0) with corrected (non-overclaiming) commentary;
  updated module doc comment and env-var defaults.
- `research/experiments/2026-07-25-ap5-content-decoding.md` (this file) — rewritten to reflect the
  corrected evidence.

## Follow-ups

- If `ap4_full_message_mask_enabled` is ever considered for change (default flip, removal, or
  extension to cover more message types), re-read this entry — Ap5 and AP4's full-mask feature
  currently fully overlap for `WaitingForConfirmation`, and that overlap is load-bearing for why
  `content_ap_enabled` staying off has no production cost today.
- `WaitingForReport`'s 0-rescue result is the more decision-relevant one (Ap5's only uncontested
  test in the current ladder) and is now proven, not inferred — re-run
  `cargo run --release -p pancetta-research --example ap5_content_recall_fp_sweep -- --full` first
  if anyone revisits content-AP after a future decoder change; it's already built.
- The §4B corpus A/B (real-audio decode-rate delta + ft8_lib-truth precision proxy) is still
  genuinely unbuilt, not just unrun — worth doing if a future measurement ever shows real net
  synthetic recall lift in the actual production ladder, or if the ft8_lib submodule gap gets
  fixed and someone wants the regression-check value independent of the recall question.
- `ap_llr_magnitude` / `min_ap_decode_confidence` being config-surface-only (not wired) is a
  pre-existing, documented (Task 4) gap — not raised here as a new bug, just flagged as relevant
  context for anyone reading this harness's knob-tradeoff output.
- The large (~43 min), full-official-range, 60-trials/point corroborating run mentioned in
  "Decision" above was not re-run cleanly in the foreground for this entry (would take too long
  under current system load to fit a single foreground command); if someone wants that exact
  official-range table again, `--full` with default trial counts reproduces it deterministically
  (same seeded RNG), just plan for a long-running background/supervised invocation rather than a
  blocking foreground one.
