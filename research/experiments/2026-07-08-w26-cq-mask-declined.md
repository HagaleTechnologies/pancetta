# AP coverage: CQ mask — A/B against the TRUE production baseline — DECLINED (Workstream 2, Task W2.6, component 1 of 3)

**Date**: 2026-07-08
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: [A/B] — **DECLINED**. `Ft8Config::default().cq_ap_enabled` stays `false`.

## What this is

Task W2.6 adds a new `ApLevel::Cq` (`pancetta-ft8/src/ap.rs`): when a candidate fails standard
(AP0) decode, assume it's a plain "CQ" call and inject the packed "CQ" special token (`pack28`
value 2) at the `to_callsign`/first-token position (payload bits 0-27 + the bit-28 suffix flag,
forced false) plus the i3=1 "standard message" type bits (74-76) — the same message-type
assumption `ApLevel::Ap4` already makes. Unlike AP1-AP4, this level needs **no context at all**:
"CQ" is a fixed protocol token, not a personal callsign, so it requires neither `ApContext.my_call`
nor `ApContext.active_qso` and is tried unconditionally (sequenced before AP1, gated only by the new
`Ft8Config::cq_ap_enabled` flag) on every candidate that reaches AP injection in
`par_try_ap_decode`/`par_decode_candidate`.

Because it needs no context, this is the one W2.6 sub-component that is measurable at FULL,
unmodified corpus scale via `eval`'s stock CLI (`--cq-ap`/`--no-cq-ap`) — it does **not** hit the
`active_qso`/`my_call` harness-measurability gap flagged in W1.1/W1.7 for AP3/AP4 (see the plan's
own framing). This is a genuine "measured, doesn't help" result, not a "couldn't be measured"
result.

## TDD (bit-level, encoder ground truth)

`pancetta-ft8/tests/ap_i3_tests.rs`:
- `encoder_cq_message_packs_cq_token_at_to_callsign_position`: encodes "CQ K1DEF FN42" with the
  real `Ft8Encoder`, confirms `to_callsign` (bits 0-27) equals `pack28("CQ")`'s packed value 2,
  bit 28 (suffix flag) is clear, and i3=1 (bits 74-76 = 0,0,1).
- `cq_mask_injection_matches_encoder_ground_truth`: `inject_ap_llrs(.., ApLevel::Cq, ..)`'s injected
  LLR signs at bits 0-28 and 74-76 match that same encoder ground truth exactly.
- `cq_mask_requires_no_context`: identical injection output regardless of `ApContext` contents
  (empty vs. fully-populated my_call/recent_calls/active_qso).

`pancetta-ft8/src/ap.rs` unit tests: `test_inject_cq_uses_pack28_cq_token_at_called_position`,
`test_inject_cq_requires_no_context`.

## Audio-domain rescue search (honest negative finding)

Before running the corpus A/B, an extensive audio-domain search was performed to look for a
"AP0 fails, CQ mask rescues" scenario, using the exact same global-noise + targeted-burst
methodology `ap_injection_ordering_tests.rs` (Task W1.7) used to discriminate the AP3 fix: a real
encoded+modulated "CQ K5ARH EM10" signal, global Gaussian noise across the whole 12.64s window plus
an extra noise burst localized to the data tones carrying payload bits 0-56 (tones 7-25).

**~106 (global_snr_db, field_snr_db) combinations tested** (0.1-0.2 dB resolution near the AP0-fails
transition, global noise -25.0 to -34.0 dB, burst depth -14 to -40 dB) found **zero** points where
`ApLevel::Cq` rescued a decode AP0 (no context) could not already produce: at every level, either
AP0 already decoded it too, or nothing decoded at all. There is a sharp, narrow transition (AP0
flips from always-succeeds to always-fails within ~0.1 dB of global noise), but no window in
between where the CQ-token bias alone tips LDPC belief propagation into convergence.

**Plausible mechanism for why**: CQ's injected token (`pack28("CQ")` = 2) is a mostly-zero 28-bit
value — it constrains far less real information into the LDPC prior than a genuine (higher-entropy)
callsign does (the kind AP1/AP3 inject). The "CQ" prior is structurally "thin" compared to a real
call.

This is exactly the kind of empirical caveat the plan asks to distinguish honestly: this is NOT the
"harness can't exercise this" gap (CQ needs no context, so the harness gap doesn't apply here at
all) — it is a genuine "searched hard and found no rescue in this scenario" result. The corpus-level
A/B below is the actual arbiter, not this one hand-tuned synthetic signal; see
`pancetta-ft8/tests/w26_ap_coverage_tests.rs`'s documented test note for the permanent record of
this finding, plus `pancetta-ft8/src/decoder.rs`'s internal `w26_ap_coverage_tests` module for a
direct-function plumbing-correctness test (does `par_try_ldpc_with_cq` actually decode when asked —
yes, `ap_level=5`, text-correct — independent of whether it helps net recall).

## A/B methodology

Same `eval`/`compare` pipeline as every other task in this plan, measured against the TRUE current
production default (`cq_ap_enabled: false`):

```
cargo build --release -p pancetta-research --bin eval --bin compare
./target/release/eval --tier curated-hard-200 --mode ft8 --no-cq-ap \
  --output research/scorecards/w26_cq_hard200_baseline.json
./target/release/eval --tier curated-hard-200 --mode ft8 --cq-ap \
  --output research/scorecards/w26_cq_hard200_variant.json
./target/release/eval --tier noise_1000 --mode ft8 --no-cq-ap \
  --output research/scorecards/w26_cq_noise_baseline.json
./target/release/eval --tier noise_1000 --mode ft8 --cq-ap \
  --output research/scorecards/w26_cq_noise_variant.json
./target/release/compare research/scorecards/w26_cq_hard200_baseline.json research/scorecards/w26_cq_hard200_variant.json
./target/release/compare research/scorecards/w26_cq_noise_baseline.json research/scorecards/w26_cq_noise_variant.json
```

Full `curated-hard-200` (200 WAVs) + full `noise_1000` (1000 WAVs). `compare`'s CONFIG DIFF confirms
`cq_ap_enabled` is the ONLY field differing between the two runs in both comparisons.

Elapsed: hard-200 baseline 63.3s, variant 63.5s. noise_1000 baseline 1268.6s (~21.1 min), variant
1269.4s (~21.2 min) — no measurable cost either way (CQ injection is a cheap 28+3-bit LLR write plus
one extra LDPC/CRC attempt per candidate that reaches AP injection, and it never survives to a
decode in this corpus, so no downstream work is added).

## Results

### hard_200 (recall) — ZERO measured effect

```
A: w26_cq_hard200_baseline.json (score 0.3126)
B: w26_cq_hard200_variant.json  (score 0.3126 +0.0000)

REGRESSIONS:
  (none)

BOOTSTRAP CI (n_bootstrap=1000, seed=0xb007):
  curated-hard-200   rec Δ=+0   (95% CI [+0.0, +0.0])   — NOT significant
  curated-hard-200   novel Δ=+0 (95% CI [+0.0, +0.0])   — NOT significant

CONFIG DIFF:
  decoder.decoder.cq_ap_enabled   false -> true
```

Bit-for-bit identical composite score — not a single decode differs between baseline and variant on
the full curated-hard-200 corpus, corroborating the audio-domain search above exactly.

### noise_1000 (false positives — the plan's hard gate) — NO CHANGE

```
A: w26_cq_noise_baseline.json  false_positives_total = 1  (1/1000 WAVs)
B: w26_cq_noise_variant.json   false_positives_total = 1  (1/1000 WAVs)
```

No new false positives introduced. The pre-existing 1-WAV noise hallucination (present in the
`main.json` baseline too, unrelated to this task) is unchanged.

## Standing gate evaluation

| Criterion | Result | Verdict |
|---|---|---|
| Bootstrap-CI recall delta (hard-200) excludes zero, in favor | rec Δ=+0, CI [+0.0, +0.0] — literally zero | **FAIL** |
| FP-on-noise = 0 new decodes | 1 -> 1 (no change) | PASS (moot given the above) |
| Elapsed hard gate | No measurable cost on either tier | PASS (moot given the above) |

## Decision

**DECLINED.** `Ft8Config::default().cq_ap_enabled` stays `false`. No production behavior changes.

This is a genuine "measured and it doesn't help" result — distinct from the harness-measurability
gap flagged for AP3/AP4 (this level needs no context, so that gap simply doesn't apply here). The
mechanism (injection, gating, survival check) is implemented, tested, and provably correct (TDD
against encoder ground truth + a direct-function plumbing test decodes a real clean CQ signal); it
just doesn't add recall on this corpus, on either tier, at any noise level tested. Per this plan's
established discipline (W2.4/W2.5): declined honestly, no forced flip.

## Files

- `research/scorecards/w26_cq_hard200_baseline.json`, `w26_cq_hard200_variant.json`
- `research/scorecards/w26_cq_noise_baseline.json`, `w26_cq_noise_variant.json`
