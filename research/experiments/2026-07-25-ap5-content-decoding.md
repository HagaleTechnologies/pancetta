# Ap5 (content-AP) recall/false-decode-vs-SNR eval harness — Task 7

**Date**: 2026-07-25
**Branch**: `worktree-ap-decoding`
**Status**: Pilot run, conclusive negative. Full §4A sweep NOT run (deliberately — see
"Decision: why the full sweep didn't run" below). Ship gate: **NOT MET**.
**Plan**: `docs/superpowers/plans/2026-07-25-ap-content-decoding.md`, Task 7 of 8.
**Design**: `docs/ap-decoding-design.md` §4 (eval protocol, the ship gate).
**Harness**: `pancetta-research/examples/ap5_content_recall_fp_sweep.rs`.

## What this is

Task 6 wired `ApLevel::Ap5` (content-hypothesis LLR injection: the AP3/4 callsign bits plus one
enumerated content hypothesis's payload bits 58-76) into the decode loop, gated off by
`Ft8Config::content_ap_enabled` (default `false`). Task 6's implementer searched real audio via 3
methodologies and found **no natural window where the existing AP0-AP4 ladder fails but Ap5
succeeds** — the wiring-correctness test had to structurally disable AP2-4 (empty `recent_calls`,
`active_qso: None`) to isolate a clean Ap5 win. That proves the wiring works; it does not prove Ap5
buys real recall over the full ladder.

This task's brief carried forward an explicit instruction: **before running the full §4A/§4B
sweep, first run a small pilot to check whether this harness's own synthetic corruption
methodology can construct even one genuine "AP0-4 fails, Ap5 rescues" case** — and if it can't
across enough trials to be conclusive, report that as the primary result rather than grinding
through the rest of the spec.

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
   are both populated, so **the full AP0-4 + Cq ladder is reachable on both sides** — this pilot
   does NOT use Task 6's structurally-isolated-Ap5 setup.
3. Decode the SAME noisy audio once with `content_ap_enabled: false` (full legacy ladder, no Ap5)
   and once with `content_ap_enabled: true` (same ladder + Ap5), both via the public
   `Ft8Decoder::decode_window_with_ap` API.
4. A **rescue case** = AP-off decode does not recover the true text, AP-on decode does.

The pilot deliberately swept **deeper than the official §4A range** (-24..-6 dB) — down to -46 dB
in the first exploratory pass, focused to -19..-25 dB in the two follow-up runs — specifically to
answer "can this corruption method construct a rescue window at all", not just "within the nominal
operating range".

## Results

Three separate pilot runs, all foreground/synchronous, all showing the same pattern:

**Run 1** (coarse scan, sanity + cliff-location): 8 trials/point, SNR ∈
{-18,-20,-22,-24,-26,-28,-30,-32,-34} dB, both stages. 144 trials total.
`off_hit% == on_hit%` **exactly** at all 18 (SNR × stage) rows. 0 rescues. Located the AP0-4
recall cliff at roughly -20 to -24 dB (consistent with the decoder's known ~-21 dB AP0 sensitivity
floor — `batch30_snr_recall_curve.rs`'s reference curve — plus the extra margin AP3/4's 56 known
callsign bits buy).

**Run 2** (deep sweep at the cliff, partial — 6 of 14 rows captured before the harness process was
interrupted mid-run by an unrelated tooling issue, not a harness failure): 80 trials/point, SNR ∈
{-19,-20,-21,-22,-23,-24,-25} dB. Every captured row (-19 through -24, `WaitingForReport` stage):
`off_hit% == on_hit%` exactly (87.5/87.5, 61.2/61.2, 15.0/15.0, 2.5/2.5, 0.0/0.0, 0.0/0.0). 0
rescues in 480 captured trials.

**Run 3** (clean, complete, foreground, 122.4s wall-clock): 25 trials/point, SNR ∈
{-19,-20,-21,-22,-23} dB, both stages. 250 trials total, ALL 10 rows tied exactly:

```
stage                     snr_db  trials  off_hit%   on_hit%   rescues
----------------------------------------------------------------------
WaitingForReport           -19.0      25     88.0%     88.0%         0
WaitingForReport           -20.0      25     68.0%     68.0%         0
WaitingForReport           -21.0      25      8.0%      8.0%         0
WaitingForReport           -22.0      25      4.0%      4.0%         0
WaitingForReport           -23.0      25      0.0%      0.0%         0
WaitingForConfirmation      -19.0      25    100.0%    100.0%         0
WaitingForConfirmation      -20.0      25     84.0%     84.0%         0
WaitingForConfirmation      -21.0      25     52.0%     52.0%         0
WaitingForConfirmation      -22.0      25     20.0%     20.0%         0
WaitingForConfirmation      -23.0      25      4.0%      4.0%         0

PILOT VERDICT: 0 genuine rescue cases out of 250 trials across 5 SNR points x 2 stages.
```

**Aggregate across all three runs: 0 rescue cases in 874 total trials**, spanning -18 to -34 dB —
well past both the official §4A range and the AP0-4 recall cliff in both directions. At every
single SNR point tested, AP-off and AP-on produced byte-identical hit rates. This is not "a low
rescue rate" — it is an exact tie at every measured point, which is the strongest form of the null
result this design (`inject_ap_llrs` is soft, LDPC/CRC can override AP either way) predicts if the
56 callsign bits AP3/4 already inject are enough on their own: below the cliff, sync itself already
fails for both configurations (no candidate to even attempt AP on); above the cliff, AP3/4's prior
already saturates recovery and the extra 19 content bits Ap5 adds are marginal.

Positive control (sanity check the harness isn't just failing to decode anything): at -8 and -16
dB, both AP-off and AP-on hit 100% on both stages — the harness decodes correctly when signal
quality is good, confirming the zero-rescue result isn't a harness bug that fails all decodes.

## Decision: why the full §4A/§4B sweep didn't run

Per the carry-forward instruction: the pilot's job was to determine, before investing in the full
sweep, whether the ship-gate question ("does recall rise meaningfully") has a knowable answer
already. It does: **no**, at a level of evidence (874 trials, exact ties at every SNR point across
a wide range, positive control confirming harness validity) that running the sweep at the official
-24..-6 dB range with more trials per point would not overturn — the cliff sits inside that range
and the tie holds on both sides of it. The false-decode protocols and knob-tradeoff curve (§4A
steps 2-3) measure a cost that only matters if there's a benefit to weigh it against; with zero
benefit, running them would characterize a tradeoff for a feature this harness already shows has
no upside. `--full` exists on the harness (`cargo run ... --example
ap5_content_recall_fp_sweep -- --full`) for anyone who wants to run it anyway — e.g. if a future
change to the LDPC/OSD pipeline or the AP3/4 callsign-bit injection changes this picture.

## §4B (corpus A/B) — documented gap, not built

Not implemented. Two independent reasons:

1. **Missing submodule** (pre-existing, unrelated to this task, flagged by the task brief in
   advance): `pancetta-ft8/vendor/ft8_lib` is not checked out in this worktree (`git submodule
   status` shows a `-` prefix; `cargo build` prints "ft8_lib C sources not found ... building
   WITHOUT the C decoder (ft8lib_stub, degraded decode recall)"). This blocks the ft8_lib-truth
   precision proxy §4B's spec calls for.
2. **Disproportionate given the pilot's answer**: §4B's own spec text requires "context
   reconstructed from the decode stream" — a nontrivial QSO-state-machine replay over corpus
   audio, not a small addition. Building it to characterize a tradeoff the synthetic harness
   already shows doesn't exist would be effort spent answering a question that's already answered.

If a future change makes the pilot's answer non-zero, §4B becomes worth building as a real
follow-on; recorded here so it isn't silently dropped.

## A load-bearing implementation note: `ap_llr_magnitude` is a no-op today

While building the harness, confirmed by grep that `Ft8Config::ap_llr_magnitude` and
`::min_ap_decode_confidence` are config-surface-only as of this task — `inject_ap_llrs` and
`try_ldpc_with_ap` still read their own hardcoded consts (`AP_LLR_MAGNITUDE = 15.0`,
`MIN_AP_DECODE_CONFIDENCE = 0.55`), not these `Ft8Config` fields (no call site outside a
default-value unit test reads `.ap_llr_magnitude` or `.min_ap_decode_confidence`). Only
`content_ap_enabled`, `min_content_ap_confidence`, `max_ap_hypotheses`, and `max_ap_qsos` are
actually wired to the Ap5 decode loop. The harness's `--full` knob-tradeoff table includes
`ap_llr_magnitude` anyway (to demonstrate the no-op directly rather than silently omit it), flagged
in the harness's own doc comment. Not a bug in this task's scope — Task 4's own doc comments
already say this promotion is "config surface only... until a later task wires it through" — just
worth recording so a future knob-tradeoff run doesn't mistake "no effect from `ap_llr_magnitude`"
for a real finding about the mechanism's sensitivity.

## Ship-gate statement (for the operator — not a decision, per the plan's global constraint)

Per `docs/ap-decoding-design.md` §4's ship gate: *"graduate `content_ap_enabled` ... only if, at a
fixed operating point, synthetic recall rises meaningfully AND the false-decode rate stays within
budget across the SNR sweep AND the corpus A/B shows non-negative decode-rate at non-negative
precision."*

- **Synthetic recall rises meaningfully**: **NOT MET**. 0 rescue cases in 874 trials across a wide
  SNR range, exact ties at every point.
- **False-decode rate within budget**: not measured (moot — no recall benefit to weigh against a
  cost).
- **Corpus A/B non-negative**: not measured (§4B gap above; also moot for the same reason).

**`content_ap_enabled` stays `false`, unchanged, per the plan's binding global constraint** — this
was never in question regardless of the eval outcome. This entry documents the evidence for the
operator's own later call on whether content-AP is worth further investment (e.g. a different
corruption methodology, a harder/more adversarial synthetic scenario, or revisiting after an
unrelated LDPC/OSD change), not an automatic action.

## Files changed

- `pancetta-research/examples/ap5_content_recall_fp_sweep.rs` (new) — the eval harness. Implements
  the full §4A recall sweep, both false-decode decoy protocols (wrong-context, noise-only), and the
  knob-tradeoff curve (`--full`), plus the pilot phase described above (default, no flag needed).
  `--help` documented.
- `research/experiments/2026-07-25-ap5-content-decoding.md` (this file).

## Follow-ups

- If anyone revisits content-AP after a future decoder change, re-run
  `cargo run --release -p pancetta-research --example ap5_content_recall_fp_sweep -- --full`
  first — it's already built and will pick up any new daylight between AP-off and AP-on.
- The §4B corpus A/B (real-audio decode-rate delta + ft8_lib-truth precision proxy) is still
  genuinely unbuilt, not just unrun — worth doing if a future measurement ever shows real synthetic
  recall lift, or if the ft8_lib submodule gap gets fixed and someone wants the regression-check
  value independent of the recall question.
- `ap_llr_magnitude` / `min_ap_decode_confidence` being config-surface-only (not wired) is a
  pre-existing, documented (Task 4) gap — not raised here as a new bug, just flagged as relevant
  context for anyone reading this harness's knob-tradeoff output.
