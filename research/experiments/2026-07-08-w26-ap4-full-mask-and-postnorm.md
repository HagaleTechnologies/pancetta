# AP coverage: AP4 full message-content mask + post-normalization AP injection ordering — A/B (Workstream 2, Task W2.6, components 2 & 3 of 3)

**Date**: 2026-07-08
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: [A/B] — see per-component verdicts below.

## What these are

**AP4 full message-content mask** (`Ft8Config::ap4_full_message_mask_enabled`): AP4 previously only
constrained the message TYPE (i3=1 bits, 74-76) on top of AP3's callsign injection. This task adds
`crate::ap::ConfirmationToken` (`Rrr`/`RR73`/`Final73`) + `inject_confirmation_token_bits` (injects
the `ir` bit, payload bit 58, always 0 for these tokens, plus the full 15-bit `igrid4` field, bits
59-73, to the token's `MAXGRID4`-relative value: RRR=+2, RR73=+3, 73=+4) — pinning the actual
completion CONTENT, not just the message class. When `ap4_full_message_mask_enabled` is `true`, the
decoder tries each of the three tokens (`ConfirmationToken::ALL`, RR73 first as the most common
real-world single completion message) via a new `par_try_ldpc_with_ap4_full`, strictly additive:
falls through to the plain (i3-only) AP4 attempt if none of the three survive. A new
`ap4_full_mask_survived` verifies the decoded `standard_type` matches the specific token that was
injected (on top of the existing `ap_injection_survived(Ap4, ...)` callsign check).

**Post-normalization AP injection ordering** (`Ft8Config::ap_injection_post_normalization`): every
live AP injection site (`par_try_ldpc_with_ap`, `par_try_ldpc_with_recent_only`,
`par_try_ldpc_with_cq`, `par_try_ldpc_with_ap4_full`) previously injected the fixed-magnitude AP bits
into the raw channel LLRs, THEN called `normalize_llrs` — so the injected magnitude contributed to
the variance `normalize_llrs` computes over the whole 174-element array, distorting the scale factor
applied to the (majority) non-injected channel-evidence bits. When the flag is `true`,
`normalize_llrs` runs FIRST (pure channel evidence), and AP bits are injected AFTER, at the fixed
post-normalization magnitude — leaving the non-injected bits' scale entirely untouched by the
injection.

## Check performed first: does `a8_qso_state_ap_enabled`'s template machinery subsume the RR73 case?

**No — investigated directly, confirmed by re-reading `ap.rs`/`decoder.rs` before writing any code.**
`a8_qso_state_ap_enabled` (`enumerate_a8_expected_texts` + `a8_text_matches`) is a **post-decode
confidence-gate relaxation**: it enumerates canonical texts ("K1ABC W1AW RR73"/"73"/"RRR" for
`WaitingForConfirmation`) and, if the ALREADY-DECODED message's text matches one, relaxes the
`MIN_AP_CONFIDENCE` floor down to `MIN_DECODE_CONFIDENCE` and skips suspicion scrutiny — it never
touches LLRs and runs strictly AFTER LDPC has already converged. The full message-content mask is a
**pre-decode LLR-injection bias** — it changes what BP is solving for, potentially rescuing decodes
that never converge at all without it. These are complementary, not overlapping mechanisms: a8
can't rescue a non-convergent BP run (nothing to gate-relax if there's no decode), and the full mask
doesn't touch the confidence gate at all. Both can be enabled together (independent flags); a new
mechanism was needed, not a duplicate — confirmed rather than assumed.

## TDD (bit-level, encoder ground truth)

`pancetta-ft8/tests/ap_i3_tests.rs`:
- `encoder_confirmation_tokens_have_distinct_igrid4_fields`: real `Ft8Encoder`-encoded "K1ABC W1AW
  RR73"/"RRR"/"73" have `ir=0` for all three and pairwise-distinct 15-bit `igrid4` fields.
- `confirmation_token_mask_matches_encoder_ground_truth_for_all_three`: `inject_confirmation_token_bits`
  for each of the 3 tokens matches that exact encoder ground truth at bits 58-73.
- `ap4_plus_full_mask_matches_full_encoder_payload_for_rr73`: the COMBINED injection
  (`inject_ap_llrs(Ap4)` + `inject_confirmation_token_bits(RR73)`) matches the real encoder's full
  "K1ABC W1AW RR73" payload at every injected bit (0-27, 29-56, 58-76).

`pancetta-ft8/src/ap.rs` unit tests: `test_inject_confirmation_token_bits_rr73`,
`test_confirmation_token_igrid4_values_match_maxgrid4_offsets`.

`pancetta-ft8/src/decoder.rs`'s internal `w26_ap_coverage_tests` module:
`ap4_full_mask_survived_matches_only_the_injected_token` (direct unit test: matches only its own
token, cross-products with the other two + an unrelated `Reply` type all correctly rejected);
`injection_post_normalization_flag_controls_order` (direct unit test proving the two orderings
produce numerically different LLR vectors for a non-trivial input, and that the post-normalization
ordering's non-injected bits are an EXACT match to normalizing the base LLRs alone — the legacy
ordering's non-injected bits are NOT, confirming the distortion the fix targets is real and
measurable at the unit level).

## Audio-domain rescue test (real differential, not synthetic-sync-override)

`pancetta-ft8/tests/w26_ap_coverage_tests.rs::ap4_full_mask_rescues_signal_that_plain_ap4_cannot_decode`:
a real encoded+modulated "K1ABC W1AW RR73" QSO-context signal (global noise -32.5 dB + a -28.5 dB
burst over the callsign/content data tones, calibrated via a 0.2 dB-resolution grid search that found
a robust 6-point window, [-32.0..-33.0] dB global / [-28.0..-29.0] dB burst, where plain AP4
consistently fails but the full mask consistently rescues) where: AP0 fails, plain (i3-only) AP4
ALSO fails (proving the scenario is genuinely hard for AP4, not gamed), and the full mask rescues it
(`ap_level=4`, correct text).

## Harness-measurability gap (per W1.1/W1.7): confirmed, and how it was worked around

Investigated directly (not assumed): `pancetta-research`'s `eval` CLI (`--ap-my-call`/
`--ap-recent-calls`) can set `ApContext.my_call`/`recent_calls` for a whole corpus run, but its
`ApContext` construction (`eval.rs` ~line 1875) hardcodes `active_qso: None` unconditionally — there
is NO CLI path to construct `active_qso` at all. Since the full message-content mask (like AP4
itself) only ever fires when `active_qso == Some(WaitingForConfirmation)`, it is **structurally
unmeasurable via `eval`'s stock CLI at ANY corpus scale** — this is the exact gap flagged in
W1.1/W1.7's memory (backlog item #34), now hit again for a new AP level built on the same
foundation. This is a "couldn't be measured via the stock harness" situation, not a "measured and it
doesn't help" one.

**Distinguishing the two, and the workaround used**: rather than accept an unmeasurable result, a
new cheat-informed corpus example was built —
`pancetta-research/examples/w26_ap4_full_mask_cheat.rs` — mirroring the SAME established
methodology `ap_recovery_ceiling.rs` (hb-051) used for exactly this class of problem (corpus-scale
AP2 measurement, which also needs per-WAV context the stock CLI can't build). For each hard-200 WAV
whose jt9-verified truth IS itself an RR73/RRR/73 confirmation message, the harness constructs the
"perfect information" `ApContext` a real operator mid-QSO would have (my_call = the truth's
`to_callsign`, active_qso = the truth's `from_callsign` awaiting confirmation) and compares plain AP4
vs. the full mask against that EXACT truth text. Separately, on noise_1000 (which has no truth
messages — every WAV is band noise), a FIXED synthetic mid-QSO context (my_call=K5ARH,
active_qso=W1AW awaiting confirmation) is applied to every noise WAV to measure whether the full
mask (or the post-norm ordering) increases the false-decode rate GIVEN that AP4 is already active —
the fair comparison, since AP4 only ever fires in production when an operator genuinely has an
active QSO.

This IS a genuine measurement (not a stand-in), using real recorded/jt9-verified corpus audio and
the actual production code paths — it is a different corpus-scale methodology (perfect-information
cheat, established precedent in this codebase) rather than the stock `eval` CLI, because the stock
CLI cannot express the state needed at all.

## A/B methodology

```
cargo build --release -p pancetta-research --example w26_ap4_full_mask_cheat
./target/release/examples/w26_ap4_full_mask_cheat hard200
./target/release/examples/w26_ap4_full_mask_cheat noise
```

Plus the standard `eval`/`compare` pipeline for the post-normalization ordering's hard-200 leg, using
the CQ mask as the "vehicle" (since CQ needs no context and fires unconditionally on every candidate
that reaches AP injection — the only context-free way to exercise the ordering flag at FULL,
unmodified corpus scale without per-WAV cheat information):

```
./target/release/eval --tier curated-hard-200 --mode ft8 --cq-ap --no-ap-post-normalize \
  --output research/scorecards/w26_postnorm_hard200_baseline.json
./target/release/eval --tier curated-hard-200 --mode ft8 --cq-ap --ap-post-normalize \
  --output research/scorecards/w26_postnorm_hard200_variant.json
./target/release/compare research/scorecards/w26_postnorm_hard200_baseline.json research/scorecards/w26_postnorm_hard200_variant.json
```

(Both configs enable `--cq-ap` so the ordering change has SOMETHING to act on; `compare`'s CONFIG
DIFF confirms `ap_injection_post_normalization` is the only differing field.)

## Results

### hard-200, cheat-informed (perfect-information QSO context, RR73/RRR/73 truths only)

213 RR73/RRR/73 confirmation-message truths found in hard-200's jt9 baselines.

```
Matched by plain AP4 (i3-only, baseline):            158 / 213
[full_mask] matched: 166   recovered: 8   regressed: 0
[post_norm] matched: 158   recovered: 1   regressed: 1
```

**Full mask: a real, clean +8 recall gain, zero regressions** — 8 confirmation messages that plain
AP4 (i3-only) could not decode are recovered by the full message-content mask, and NOT ONE previously
correct decode is lost. This is exactly the shape of result W2.4/W2.5's discipline asks to weigh
honestly rather than force: unlike W2.4 (real gain PLUS a real FP regression, declined) or W2.5 (zero
gain PLUS a real FP regression, declined), this is a real gain with (pending the noise-tier check
below) apparently no downside on this measure.

**Post-norm ordering (tested independently, plain AP4 + ordering only, no full mask)**: net zero
(158 == 158) but NOT byte-identical underneath — 1 recovered, 1 regressed, canceling out. This
confirms the ordering change has a REAL, non-trivial effect on decode outcomes when AP context
actually fires (as the unit test `injection_post_normalization_flag_controls_order` already proved
at the LLR level) — but on this specific 213-message cheat-informed sample, the net effect is a wash.

### hard-200, CQ-mask-as-vehicle (full unmodified corpus, `--cq-ap` both sides)

Zero effect — see the CQ mask experiment log; since CQ mask itself never fires meaningfully on this
corpus (see that log's audio-domain search), this vehicle cannot exercise the ordering change in a
way that shows up in aggregate corpus recall. Superseded by the cheat-informed measurement above,
which DOES exercise a context that fires (AP4).

### noise_1000, cheat-informed (fixed mid-QSO context on every noise WAV)

```
Noise WAVs processed:                   1000
False positives, plain AP4 (baseline):  1  (1 WAV)
[full_mask] false positives:            1  (1 WAV)
[post_norm] false positives:            1  (1 WAV)
```

All three configs hit the EXACT SAME false positive (noise_0668.wav, "EL2NQF R30XZA/P R HQ32",
`ap_level=0` — a standard/AP0-level decode, config-independent by construction, confirmed identical
across baseline and both variants at every progress checkpoint through the full 1000-WAV run). This
is a pre-existing decoder artifact (the same class of "1 known noise FP" already present in
`main.json`/other tasks' baselines — see the CQ mask log), not attributable to either new mechanism.
**Neither the full mask nor the post-norm ordering introduces a single new false positive** relative
to plain AP4 under a realistic "operator has an active QSO" context.

## Statistical read on the hard-200 cheat-informed result (full mask)

8 recovered / 0 regressed out of 213 confirmation-message truths is a McNemar-style paired
comparison: 8 discordant pairs, all 8 in the same direction. Under the null (no true effect, each
discordant pair equally likely either direction), P(all 8 in one direction) = 2 × 0.5⁸ ≈ 0.0078 — a
simple binomial sign test rejects the null at the conventional 0.05 level, even without the plan's
usual bootstrap-CI tooling (which isn't wired to this cheat-informed harness's raw counts; the
`eval`/`compare` pipeline can't run at all here per the harness-measurability gap). For post-norm,
1 recovered / 1 regressed (1 discordant pair each direction) gives sign-test p = 1.0 — indistinguishable
from no effect at this sample size.

## Standing gate evaluation

### AP4 full message-content mask

| Criterion | Result | Verdict |
|---|---|---|
| Recall gain, statistically supported | +8/-0 out of 213 (sign-test p≈0.0078) | **PASS** |
| FP-on-noise = 0 new decodes | 1 -> 1 (no change), same WAV/message/ap_level across all 3 configs | **PASS** |
| Mechanism correctness (TDD + unit + audio-domain differential) | All green (see above) | **PASS** |

### Post-normalization AP injection ordering

| Criterion | Result | Verdict |
|---|---|---|
| Recall gain, statistically supported | net 0 (1/-1, sign-test p=1.0); also 0 on the full-corpus CQ-vehicle test | **FAIL (no gain to weigh)** |
| FP-on-noise = 0 new decodes | 1 -> 1 (no change) | PASS (moot given no gain) |
| Mechanism is real (unit-level) | `injection_post_normalization_flag_controls_order` proves a genuine numeric difference | Confirmed, but doesn't translate to a measured net corpus benefit |

## Decision

**AP4 full message-content mask: FLIPPED ON.** `Ft8Config::default().ap4_full_message_mask_enabled`
now `true` (was `false`). A clean pass per this plan's standing discipline: a real, statistically
supported recall gain with zero measured false-positive cost. Full workspace test suite re-verified
green after the flip (`w26_flags_default_matches_ab_decisions` updated; `ap4_full_mask_rescues_signal_that_plain_ap4_cannot_decode`
updated to explicitly force `ap4_full_message_mask_enabled: false` for its "plain AP4" baseline leg,
since that's no longer the default). No hard-200/noise_1000 fixture decode counts changed (neither
touches `active_qso`, so they're unaffected by the flip either way).

**Post-normalization AP injection ordering: DECLINED.** `Ft8Config::default().ap_injection_post_normalization`
stays `false`. The mechanism is real at the unit-LLR level (proven directly) and the underlying
rationale (injected magnitude shouldn't contaminate the variance normalize_llrs computes from the
non-injected majority) is sound, but the measured net effect on both corpora tested is zero — one
recovery, one regression, no discernible directional benefit at this sample size, and zero effect on
the full unmodified hard-200 corpus via the CQ-vehicle test. Per this plan's established discipline
(W2.4/W2.5): measured honestly, declined on a genuine lack of demonstrated benefit, not forced.

## Follow-up (not built here, flagged honestly)

- The full message-content mask's A/B relied on a cheat-informed 213-message sample (not the full
  hard-200/noise_1000 gate at native scale) because the stock `eval` harness cannot construct
  `active_qso` at all — this is the SAME harness gap flagged in W1.1/W1.7 (backlog item #34),
  still unresolved as general infrastructure. If a future task wants a larger-scale confirmation,
  building CLI support for `--ap-active-qso <call> --ap-progress <waiting-report|waiting-confirmation>`
  in `eval` (mirroring `--ap-my-call`) would let this be re-measured on a bigger, non-cheat sample.
- The post-norm ordering's zero measured effect might not generalize past this specific sample —
  a larger cheat-informed corpus (if one existed) could resolve the 1-vs-1 tie either way. Not
  pursued further given the already-clear signal (net zero at n=213, net zero at n=200 full-corpus
  vehicle test) and this plan's guidance to avoid re-attempting an already-declined change unchanged.

## Post-flip harness self-consistency re-check (2026-07-08, review finding, re-confirmed)

**Finding**: after the flip in commit `e777fdf4` set `Ft8Config::default().ap4_full_message_mask_enabled`
to `true`, the harness's `baseline_cfg = Ft8Config::default()` (in both `run_hard200` and `run_noise`)
silently stopped meaning "plain AP4, no full mask" — it became byte-identical to the `full_mask`
variant's explicit `ap4_full_message_mask_enabled: true`. Re-running the harness at HEAD as committed
would have reported `recovered: 0, regressed: 0` for `full_mask`, directly contradicting the `+8/0`
this log used to justify the flip. The `post_norm` variant had a second, subtler instance of the same
bug: it only set `ap_injection_post_normalization: true` and relied on `..Ft8Config::default()` for
everything else, so after the flip it silently ALSO carried `ap4_full_message_mask_enabled: true` —
no longer isolating the post-norm-ordering effect alone. This is the same class of problem the
parallel unit test `ap4_full_mask_rescues_signal_that_plain_ap4_cannot_decode` (in
`pancetta-ft8/tests/w26_ap_coverage_tests.rs`) already caught and fixed for its own "plain AP4"
comparison leg, just missed here in the corpus harness.

**Fix applied**: `pancetta-research/examples/w26_ap4_full_mask_cheat.rs` now explicitly pins
`ap4_full_message_mask_enabled: false` on `baseline_cfg` AND on the `post_norm` variant (both
`run_hard200` and `run_noise`), independent of whatever the crate default happens to be, with an
inline comment explaining why. The `full_mask` variant's explicit `true` was already correct and is
unchanged.

**Re-run result (2026-07-08, post-fix, same build/commands as the original methodology section
above)**:

```
RR73/RRR/73 confirmation truths found in hard-200: 213
Matched by plain AP4 (i3-only, baseline):           158
[full_mask] matched: 166  recovered: 8  regressed: 0
[post_norm] matched: 158  recovered: 1  regressed: 1

Noise WAVs processed:                         1000
False positives, plain AP4 (baseline):        1  (1 WAV)
[full_mask] false positives:                  1  (1 WAV)
[post_norm] false positives:                  1  (1 WAV)
```

**These are IDENTICAL to the original numbers reported above** (+8/0 on hard-200 for the full mask;
1/1 net-zero for post-norm; 1→1 unchanged false-positive rate on noise, same single WAV/message).
With the baseline now correctly pinned to `false` independent of the crate default, the original
`+8/0` result IS reproducible and the flip decision stands on honest, re-confirmed evidence — this
was a harness self-consistency bug (the evidence-generator no longer matching its own claimed output
if re-run unmodified), not an error in the original measurement or the underlying decision. **No
change to the flip decision.**

## Residual risk not covered by this measurement (flagged for on-air soak, not built here)

Both the original and re-confirmed measurements above are a **perfect-information ceiling**: the
cheat harness sets `my_call`/`active_qso` to the truth message's own `to_callsign`/`from_callsign`
(hard-200 leg) or a fixed, always-active synthetic context (noise leg). Two things this does NOT
exercise, both flagged here rather than built (constructing a faithful test for the first is
genuinely hard — noted below — and was already flagged as appropriately out of scope by the original
reviewer):

1. **Real signal + stale/wrong `active_qso`.** In production, `active_qso` is maintained by the QSO
   state machine and can be stale or momentarily wrong (e.g. a QSO just completed, or another
   station's frame is in flight) at the exact moment a DIFFERENT real signal is being decoded. Both
   `ap_injection_survived(Ap4, ...)` and `ap4_full_mask_survived` only check that the decode AGREES
   with the injected hypothesis — neither checks that the hypothesis is CURRENT. It is theoretically
   possible for a strong genuine LLR set from an unrelated real signal to be bent, by the injected
   to/from/token bias, into a survival-passing decode whose content matches the (wrong) injected
   context rather than the real over-the-air message — a wrong-content false decode, not caught by
   either survival check. This scenario needs a real competing signal decoded under a deliberately
   stale/mismatched `active_qso`, which is a materially harder harness to build honestly (it needs a
   second real signal AND a plausible "just went stale" QSO-state snapshot) than the noise-only FP
   check done here, and was not attempted.
2. **Perfect-information ceiling.** Both the `+8` recovered count and the noise-leg 1→1 FP-parity
   result assume `active_qso` is exactly correct at decode time. The real-world benefit (and the real-
   world FP exposure) is bounded above/below by how accurately production's live QSO-state tracking
   actually stays in sync with the true on-air state — not measured here.

Both risks are documented in a code comment next to `Ft8Config::ap4_full_message_mask_enabled` in
`pancetta-ft8/src/decoder.rs`. Per this plan's spec (`docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md`
§7.5) and the pattern used for other production-behavior changes this session, this flip should be
treated as **not fully validated until confirmed by an on-air soak** comparing real QSO sessions with
the full mask enabled vs. disabled, specifically watching for wrong-content decodes during genuine
mid-QSO operation (not just the noise-corpus FP rate checked here).

## Files

- `research/scorecards/w26_postnorm_hard200_baseline.json`, `w26_postnorm_hard200_variant.json`
  (CQ-mask-as-vehicle full-corpus run)
- Cheat-informed results are raw stdout (this log), not `eval` scorecards — no `compare`-compatible
  JSON exists for this measurement method.
