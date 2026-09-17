# PAN-157 re-measurement: Task W4.3's bounded-budget win no longer reproduces

**Date**: 2026-09-17
**Branch**: `tony/pan-157-operators-on-the-standard-performance-tier-should-decode`
**Cross-references**: `research/experiments/2026-07-09-w43-multipass-remeasurement.md` (the
corrected narrative for the original measurement this re-measures), `docs/decoder-comparison.md`
(lines 282-295, corrected by this file — see below).
**Status**: [A/B] **PAN-157 declined.** The `--effort standard` (bounded, 250ms) hard-200 win this
ticket was scoped around — `max_decode_passes=2` + `time_varying_subtraction_enabled=true`,
originally measured at **+32 TP** (CI [+12, +57], excludes zero) in the 2026-07-09 remeasurement —
re-measures today as **+0** (CI [+0.0, +0.0], bit-for-bit identical decode set), at +978% elapsed
cost. PAN-156's tier-conditional plumbing ships correctly as pure no-op infra regardless (already
merged as PR #387); this ticket's specific default-flip does not.

## Why re-measure

PAN-157 (filed 2026-09-16 from a decoder-decline audit) proposed wiring exactly this mechanism into
the `Standard` `DecodeEffort` preset via PAN-156's new tier-conditional `Ft8Config` override seam.
The ticket's own notes required reconfirming the original measurement via this harness before
flipping any default, since it "predates several months of subsequent decoder changes." That
turned out to be the right caution: the gap between July and September closed the exact headroom
this mechanism used to recover.

## What was measured

`cargo run --release -p pancetta-research --bin eval -- --tier curated-hard-200,synth-clean --mode
ft8 --effort standard --max-passes {1,2} [--time-varying-subtraction-enabled] --output ...`, then
`compare`. Full 200-WAV `curated-hard-200` corpus (copied from `sophon`, which also supplied a
pre-existing jt9 baseline cache for 161/200 WAVs; the remaining 39 were freshly jt9-baselined here
after installing WSJT-X 3.0.1 arm64 on `aldebaran`). `synth-clean`'s own jt9 baseline cache is
still absent fleet-wide, so its `jt9_snr_curve` metric is empty in both scorecards — informational
only, does not affect the hard-200 result below.

- **Baseline** (`max_decode_passes=1`, today's `Ft8Config::default()`): `truth_decodes_recovered`
  5436 / 9094 (59.78%), 741 novel decodes, 44.2s elapsed (both tiers).
  `research/scorecards/pan157-baseline-standard.json`.
- **Treatment** (`max_decode_passes=2`, `time_varying_subtraction_enabled=true`):
  `truth_decodes_recovered` **5436 / 9094 — identical**, 741 novel — **identical**, 476.9s elapsed
  (**+978%**). `research/scorecards/pan157-treatment-standard.json`.
- **`compare`**: `curated-hard-200 rec Δ=+0 (95% CI [+0.0, +0.0], n_bootstrap=1000) — NOT
  significant`; `novel Δ=+0` likewise. Hard elapsed-time gate also fails independently (+978% vs.
  a +20% budget) — this configuration would be disqualified on cost alone even with a real recall
  win.

## Why the win disappeared

The July 2026 measurement's baseline recovered 1206/? truths on hard-200 under the same bounded
budget; today's baseline recovers 5436/9094 — the decoder's pass-0 recall has grown substantially
in the interim (`coherent_multipass_iterations` graduating to a default-on `3`, LDPC iteration
increases, cross-cycle averaging, AP4 full-message-mask, block-score reranking, and other Batches
shipped since). This is the same "ceiling effect" the July doc already documented for the
*unlimited*-budget case (pass 0 exhaustively runs every rescue mechanism, leaving nothing for pass
2 to find) — except by September, pass 0 alone has become strong enough to hit that same ceiling
even under the *bounded* Standard budget that used to leave real headroom. The mechanism itself
isn't broken; the specific gap it used to fill has been closed by unrelated, already-shipped work.

## Decision

**PAN-157 declined — no code change.** `effort::effort_ft8_overrides`'s `Standard` arm (added by
PAN-156, PR #387) stays a no-op, which is now the *confirmed-correct* state rather than an interim
placeholder pending this ticket. Nothing to revert; nothing to ship.

No new hypothesis or reopen condition is proposed here — this isn't a "shelve for later," it's a
re-measurement that came back null. If a future decoder regression or a different corpus regime
resurfaces headroom this mechanism could recover, that would need its own fresh measurement, not a
revival of this one.

## Full test suite

Not applicable — no source or config default changed by this finding, only documentation
(`docs/decoder-comparison.md`) and this experiment log.
