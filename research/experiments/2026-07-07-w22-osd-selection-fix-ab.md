# OSD best-by-distance selection + acceptance gate — A/B (Workstream 2, Task W2.2)

**Date**: 2026-07-07
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: [A/B] — production default (`osd_depth: Some(0)`) untouched; A/B measured at
research-only `osd_depth: Some(2)`, same config W2.1 used for calibration.

## What this is

W2.1 built a signal-domain acceptance metric (`AcceptanceScore { soft_distance, hard_errors,
coherence }`, `acceptance::score()`) and calibrated `soft_distance <= 0.0976` on a definitive
TP/FP subset (hard_200 jt9-verified vs. noise_1000), purely informationally — nothing consulted
it to accept/reject a decode. This task (W2.2) is the fix design spec §4 called "the pivotal
measurement of the whole workstream": OSD's order-1/2/3 loops previously returned the **first**
CRC-14-passing candidate out of up to 121,485 flip patterns with no distance metric at all — at
CRC-14's 2⁻¹⁴ collision rate, false passes are a statistical certainty at that trial volume
(Batch 73 measured ~7,000 spurious FPs for zero net TP). This task makes the loops collect ALL
CRC-valid candidates per order, select the minimum-`soft_distance` one, and gate acceptance on
`max_soft_distance`/`max_hard_errors` (both derived from W2.1's calibration) before returning —
falling through to the next (deeper) order if the best candidate at the current order isn't
trustworthy.

## Implementation

`pancetta-ft8/src/osd.rs`:

- `OsdConfig` gains three new fields: `max_soft_distance: f32` (default `0.0976`, W2.1's
  calibrated threshold), `max_hard_errors: u16` (default `37`, the max `hard_errors` observed
  among W2.1's hard_200 jt9-verified population — a secondary, weaker backstop; `soft_distance`
  is the load-bearing metric per W2.1's own finding that a raw hard-error count can rank cases
  in the *wrong* order), and `accept_immediately_below: f32` (default `0.02`, below W2.1's
  observed noise-corpus minimum `soft_distance` 0.0242 — a pure cost-bounding early-out that
  never changes which candidate wins among those actually compared).
- `OsdDecoder::decode_with_features` (2-arg, existing signature) is now a thin wrapper around
  a new `decode_with_features_scored(llrs, channel_llrs, neural_ordering)`, which takes a
  **separate** `channel_llrs` array used ONLY for scoring/ranking candidates via
  `acceptance::score()` — never for reliability ordering or candidate generation, which continue
  to use `llrs` exactly as before. `decode_with_features(llrs, ordering)` calls
  `decode_with_features_scored(llrs, llrs, ordering)` (channel = llrs when the caller has no
  separate channel array — true of every existing test and every other call site except the one
  production site).
- The order-1, order-2, npre2-warm-start, and order-3 loops each now: collect every CRC-valid
  candidate at that order (no early return on first CRC pass), track the minimum-`soft_distance`
  one via a shared `is_better_candidate` helper, early-out via `accept_immediately_below` once a
  clearly-good match is found (a pure cost bound — never changes which candidate wins), then
  gate the chosen best candidate via `passes_acceptance_gate` (`soft_distance <=
  max_soft_distance && hard_errors <= max_hard_errors`) before returning it. If the gate fails,
  falls through to the next order instead of trusting a low-confidence "best of a bad lot".
- **Order-0 (single hard-decision candidate, zero flips) is deliberately left UNGATED and
  UNCHANGED.** It runs unconditionally regardless of `max_depth` — including at `max_depth ==
  0`, the production default — so gating it would change production behavior. There is also no
  first-vs-best selection ambiguity there (only one candidate exists). This is the mechanism by
  which production stays byte-identical: all of the new collect-and-rank/gate logic lives in
  code paths (`max_depth >= 1`) that are structurally unreachable when `osd_depth: Some(0)`.

`pancetta-ft8/src/decoder.rs`: the one production OSD call site (`post_bp_pipeline`) now builds
a `channel_llrs_arr: [f32; 174]` copy of its own `llrs` parameter (the true pre-BP channel
array, per `acceptance::score`'s documented contract) and calls
`osd.decode_with_features_scored(llr_arr, &channel_llrs_arr, neural_ordering.as_ref())` instead
of `decode_with_features(llr_arr, ...)` — `llr_arr` (BP-posterior, optionally
`bp_offset_subtract`-adjusted) still drives the search exactly as before; only the acceptance
scoring now uses genuine channel LLRs instead of accidentally being unavailable.

## TDD evidence

### Abandoned approach (documented, not hidden)

First attempt: mine a genuine CRC-14 collision by randomizing payloads + a single low-confidence
corrupted bit, hoping order-1's ~90 trials would occasionally produce a spurious CRC pass before
the true fix. **500,000 random attempts, 0 qualifying collisions** (see `diag_mining_stats`
probe, no longer in the tree). Root cause, confirmed via a follow-up brute-force search
(`diag_crc_kernel_search` probe): the CRC-14 kernel's minimum-weight nonzero element (a 77-bit
payload delta with `crc14(delta) == 0`) has weight **4** — one more than fits in OSD's max
order-3 trial budget as a single flip pattern from a shared corrupted baseline, and most
single-info-bit flips don't even touch the sparse (~1.3% density) parity columns that carry the
CRC-relevant bits, so most trials can't possibly change CRC pass/fail at all. This ruled out a
naive single-order probabilistic mining strategy.

### Working approach: deterministic construction spanning order-1 → order-3

Split a weight-4 kernel element's support `{p1,p2,p3,p4}` 3-and-1 between two valid messages:
`M` (true) and `M' = M XOR {p1,p2,p3,p4}` (also CRC-valid, since the delta is a kernel element).
Set `baseline = M XOR {p1,p2,p3}`. Then `baseline XOR {p1,p2,p3}` recovers `M` (an order-3 fix),
while `baseline XOR {p4}` recovers `M'` (an order-1 "fix" — really a spurious collision relative
to the true intended signal). Order-1 is exhaustively tried before order-3 ever runs, so under
OLD first-CRC-accept semantics, order-1 finds `M'` and returns it immediately, never reaching
order-3 where `M` lives.

Channel LLRs are set to confidently match the TRUE codeword `C = encode(M)` at every position
except `{p1,p2,p3}` (deliberately corrupted, lower magnitude, wrong sign — forcing `baseline`).
This guarantees (verified in-test, not assumed): `C` has small `soft_distance` (mismatch only at
3 low-magnitude positions), and `C' = encode(M')` has large `soft_distance` (it disagrees with
confident channel bits at `p4` plus however many parity positions the weight-4 delta's LDPC
image touches).

Test: `pancetta-ft8/src/osd.rs::osd_decode_tests::test_osd_rejects_untrustworthy_order1_collision_and_finds_truth_at_order3`.

**RED** (verified by temporarily reverting the order-1 loop to old first-CRC-accept semantics —
return on first `try_solution` success — then restoring):

```
assertion `left != right` failed: decoder must NOT accept the untrustworthy order-1 CRC-14
collision (soft_distance=0.12849845) merely because it was found first — this is exactly what
OLD first-CRC-accept code did
  left: Some(<M' bits>)
 right: Some(<M' bits>)
test ... FAILED
```

**GREEN** (fixed collect-and-rank + gate restored):

```
test osd::tests::osd_decode_tests::test_osd_rejects_untrustworthy_order1_collision_and_finds_truth_at_order3 ... ok
```

The test also asserts the underlying invariant the fix relies on:
`score_collision.soft_distance (0.128) > score_true.soft_distance` — verified, not assumed.

## The A/B: hard_200 + noise_1000 at `osd_depth: Some(2)` (research-only, OSD forced on)

Production default (`osd_depth: Some(0)`) never runs order-1/2/3 at all — this A/B exists purely
to measure the effect at a config where the fixed code path is actually exercised, exactly like
W2.1's calibration run.

**Command** (same binary/corpus/config as W2.1, re-run unmodified against the W2.2 fix):
```
cargo build --release -p pancetta-research --bin acceptance_calibration
./target/release/acceptance_calibration \
  --output research/scorecards/acceptance_calibration_w22.csv --target-fdr 0.01
```
**Config**: `Ft8Decoder::with_default_config().with_osd_depth(Some(2))` — production
`Ft8Config::default()` (`osd_depth: Some(0)`) untouched.
**Corpus**: full `hard_200` (200 WAVs) + full `noise_1000` (1000 WAVs, 30% birdie) — no
reduction.
**Elapsed**: ~16.7 minutes (hard_200: 79.5s/200; noise_1000: 920.2s/1000) — comparable to (in
fact somewhat faster than) W2.1's ~21.8-minute run on the same corpus.

### Headline result — the pivotal measurement

| Population | W2.1 (pre-fix, informational-only) | W2.2 (post-fix, gated) | Δ |
|---|---|---|---|
| hard_200 jt9-verified (TP) | 1250 | **1256** | +6 (retained + slightly grew) |
| hard_200 novel/unverified | 5222 | **4970** | −252 (−4.8%) |
| noise_1000 (FP) | 835 | **12** | **−823 (−98.6%)** |

**Noise-tier FPs collapsed from 835 to 12 — a 98.6% reduction — while jt9-verified TPs were
fully retained (in fact +6).** The hard_200 novel/unverified bucket (ambiguous — jt9 itself
misses ~80% of this operationally-hard corpus, so "unverified" isn't necessarily a
hallucination) also dropped a modest 4.8%, consistent with a small fraction of those novel
decodes previously being spurious OSD order-1+ false-accepts that are now correctly rejected,
while the large majority of "novel" decodes come from BP/order-0/AP paths this task doesn't
touch and stayed put.

### Re-verified FDR at the production gate value (0.0976), on THIS task's own data

Per the carry-forward caveat from W2.1's review (corpus-mix-dependent FDR — the 0.0976
*threshold value* is the transferable artifact, not the reported FDR number), re-measured
independently on this task's own post-fix A/B corpus rather than assumed from W2.1:

- Definitive subset (noise_1000 FP + hard_200 jt9-verified TP only): **1268 rows** (1256 TP + 12
  FP) — down from W2.1's 2085 rows (1250 TP + 835 FP), because the internal gate now suppresses
  most spurious candidates before they ever become a `DecodedMessage` at all.
- At the nearest observed `soft_distance` at/below the production gate value (0.0967, since
  0.0976 itself isn't an exact observed value in this run): **1266 accepted, 10 FP → FDR =
  0.79%** (vs. W2.1's originally reported ~0.95% on its own calibration corpus). TP retention:
  **100%** (all 1256 TPs fall at or below this threshold).
- Full re-sweep on this smaller, cleaner definitive subset: the largest threshold keeping FDR
  <=1% is now `0.1902` (up from 0.0976) — but this reflects only **12 total FP samples** (vs.
  W2.1's 835), so it is a much noisier, less statistically robust anchor; I am NOT proposing to
  raise the production gate to 0.1902 off this small-N re-sweep. The 0.0976 value stays the
  configured default; the FDR re-verification above is the honest, task-specific number.

### Honest residual-risk reporting (per the carry-forward caveat)

The 12 residual noise_1000 FP rows are **not** failures of this fix — they come from
finalization sites W2.2 does not touch: order-0 (deliberately ungated, matching production) and
the other 8 CRC-verified-decode sites W2.1 wired acceptance scoring into (AP-injection paths,
multi-pass rescue mechanisms) that were never in scope for this task's gate. The soft boundary
W2.1 documented (noise minimum `soft_distance` 0.0242 overlapping inside the verified-TP range,
max 0.0761) is still real — a small number of noise-derived decodes can and do slip through
paths this task didn't gate. This is the expected, scoped shape of the fix: it eliminates the
*dominant* order-1/2/3-driven flood (823 of 835 FPs, ~98.6%), not every conceivable false-accept
path in the decoder.

## Full test results

- `cargo test --features transmit -p pancetta-ft8 --lib`: 457 passed, 0 failed (includes the
  new mining/collision test).
- `cargo test --features transmit -p pancetta-ft8 --test osd_tests`: 7 passed, 0 failed —
  notably `test_osd_no_false_positives_on_noise` (osd_depth=2, pure-noise WAV) now reports **0**
  OSD false positives (was previously bounded at "<=5, generous allowance").
- `cargo test --workspace --features transmit`: full run, **2454 tests passed, 0 failed** across
  every crate (captured to avoid truncation-induced miscounts from a prior partial-output
  reading).
- `cargo fmt -p pancetta-ft8` + `cargo clippy --features transmit -p pancetta-ft8 --lib --tests
  -- -D warnings`: clean, no changes/warnings.

## Files changed

- `pancetta-ft8/src/osd.rs` — `OsdConfig` new fields + defaults; collect-and-rank + acceptance
  gate in the order-1/2/3/npre2 loops; `decode_with_features_scored`; new RED/GREEN test.
- `pancetta-ft8/src/decoder.rs` — the one production OSD call site now threads real channel
  LLRs separately from the BP-posterior search array; two `OsdConfig { .. }` literal
  construction sites updated with `..Default::default()` for the new fields.
- `research/scorecards/acceptance_calibration_w22.csv` (new) — full post-fix calibration data.
- This experiment log.

## Concerns for the task report

- The FDR re-verification (0.79% at the production gate, on a much smaller 12-FP population) is
  the honest task-specific number requested by the carry-forward caveat — reported as-is, not
  smoothed over. Small-N caveat noted explicitly above.
- `max_hard_errors: 37` is a secondary, largely redundant backstop (soft_distance is the
  load-bearing gate per W2.1's own finding) — flagged as a judgment call in the task report.
