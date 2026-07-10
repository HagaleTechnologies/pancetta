# OSD input LLRs — channel vs BP-posterior — A/B (Workstream 2, Task W2.3)

**Date**: 2026-07-07
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: [A/B] — production default (`osd_depth: Some(0)`, `osd_input: BpPosterior`) untouched;
A/B measured at research-only `osd_depth: Some(2)` with W2.2's acceptance gating enabled, same
config W2.1/W2.2 used for calibration.

## What this is

W2.2 made OSD's order-1/2/3 loops collect every CRC-valid candidate and select the
minimum-`soft_distance` one (scored against the true pre-BP channel LLRs), gated by
`max_soft_distance <= 0.0976`. That fixed *which candidate wins* among the ones OSD tries. This
task (W2.3) asks a different question: which LLR array should OSD's bit-flip search itself walk
over in the first place? Production (and W2.1/W2.2) always handed OSD BP's posterior LLR output
(`decoded_llrs`) — the design spec's concern is that after BP fails to converge (typically stuck
in an LDPC trapping set), that posterior can be confidently WRONG in exactly the bits that
mattered, biasing OSD's reliability ordering and candidate generation away from the truth.

Three settings were A/B'd, all gated by W2.2's acceptance mechanism (same 0.0976/37 defaults,
unchanged):

- **`BpPosterior`** (default/production, pre-W2.3 behavior) — OSD searches over BP's posterior
  output, composing with the legacy `bp_offset_subtract` field exactly as before.
- **`Channel`** — OSD searches over the true pre-BP channel LLRs directly, bypassing BP's
  posterior entirely for search purposes (BP's own convergence check upstream is unaffected).
- **`OffsetSubtracted(2.0)`** — OSD searches over BP's posterior with `|LLR|` reduced by a fixed
  2.0 (mBP pre-conditioning, arXiv:2306.00443), independent of the legacy `bp_offset_subtract`
  field. `2.0` was chosen because it's within hb-067's own previously-explored sweep range
  (`{0.0, 1.0, 2.0}`, SHELVED 2026-05-26 at a *different*, non-acceptance-gated OSD) and its own
  historical data showed the best of that set at a "soft win" magnitude.

## Implementation

`pancetta-ft8/src/decoder.rs`:

- New `pub enum OsdInput { BpPosterior, Channel, OffsetSubtracted(f32) }`, `#[default]
  BpPosterior`.
- `Ft8Config::osd_input: OsdInput` (default `BpPosterior`) — plain field, NOT a
  `pancetta-config::ConfigSection` (verified: `Ft8Config` has zero references anywhere in
  `pancetta-config/src`), so no `merge_with` implication, matching W2.1/W2.2's precedent for new
  `Ft8Config` fields.
- `LdpcDecoder` gains an `osd_input: OsdInput` field + `with_osd_input(v)` builder, threaded
  through both production construction sites (`Ft8Decoder::with_message_handler`'s sequential
  path, and `DecodeContext`/`par_decode_candidate`'s parallel per-thread path) exactly the same
  way `bp_offset_subtract` already was.
- The array-selection logic previously inlined in `post_bp_pipeline` (an `if bp_offset_subtract >
  0.0 { subtract } else { borrow decoded_llrs }`) is factored into a new pure, directly
  unit-tested method `LdpcDecoder::osd_search_llrs(decoded_llrs, channel_llrs, scratch) -> &[f32;
  174]`, matched on `self.osd_input`:
  - `Channel` → returns `channel_llrs` directly (ignores `decoded_llrs` and the legacy
    `bp_offset_subtract` field entirely).
  - `OffsetSubtracted(v)` → subtracts `v` (floored at 0, sign preserved) from `decoded_llrs`,
    independent of the legacy field.
  - `BpPosterior` → **byte-identical to pre-W2.3**: honors the legacy `bp_offset_subtract` field
    if set, otherwise borrows `decoded_llrs` with zero copies (same hot-path discipline the F6
    comment established — this array-selection code runs on every BP-non-converged candidate
    whenever OSD is configured at all, independent of `osd_depth`).
- `channel_llrs_arr`'s construction moved a few lines earlier in `post_bp_pipeline` (unconditional
  either way; needed before the selection call now that `Channel` can select it directly).
- 5 new unit tests (`decoder::tests::osd_input_default_is_bp_posterior`,
  `osd_search_llrs_bp_posterior_default_returns_decoded_llrs_unchanged`,
  `osd_search_llrs_channel_selects_channel_array`,
  `osd_search_llrs_offset_subtracted_reduces_magnitude_and_preserves_sign`,
  `osd_search_llrs_bp_posterior_still_honors_legacy_bp_offset_subtract`) prove the enum's default
  is a structural no-op, prove each variant selects the array it claims to, and prove the legacy
  `bp_offset_subtract` field still composes correctly at the `BpPosterior` default.

`pancetta-ft8/src/lib.rs`: `OsdInput` added to the public `decoder::` re-export list.

`pancetta-research/src/decoder.rs`: `Ft8Decoder::with_osd_input(pancetta_ft8::OsdInput)` builder,
mirroring `with_osd_depth`.

`pancetta-research/src/bin/acceptance_calibration.rs`: new `--osd-input
bp-posterior|channel|offset:<f32>` CLI flag (parsed by `parse_osd_input`), threaded into the
`Ft8Decoder` construction and echoed in the startup log line.

## The A/B: hard_200 + noise_1000 at `osd_depth: Some(2)` with W2.2 acceptance gating

**Command** (identical corpus/config/binary to W2.1/W2.2's calibration, only `--osd-input`
varies):

```
cargo build --release -p pancetta-research --bin acceptance_calibration
./target/release/acceptance_calibration \
  --output research/scorecards/acceptance_calibration_w23_bp_posterior.csv \
  --target-fdr 0.01 --osd-input bp-posterior
./target/release/acceptance_calibration \
  --output research/scorecards/acceptance_calibration_w23_channel.csv \
  --target-fdr 0.01 --osd-input channel
./target/release/acceptance_calibration \
  --output research/scorecards/acceptance_calibration_w23_offset2.csv \
  --target-fdr 0.01 --osd-input "offset:2.0"
```

Config: `Ft8Decoder::with_default_config().with_osd_depth(Some(2)).with_osd_input(<setting>)` —
`osd_depth=2` matches W2.1/W2.2 exactly (the only depth at which OSD's order-1/2/3 loops, and
therefore `osd_input`, actually do anything different from order-0). Corpus: full `hard_200` (200
WAVs) + full `noise_1000` (1000 WAVs), no reduction, identical manifests to W2.1/W2.2. Elapsed:
~15.3 min (bp-posterior) + ~11.7 min (channel) + ~12.7 min (offset:2.0) ≈ 40 min total (channel/
offset runs are faster than bp-posterior's because fewer noise-corpus candidates pass the
internal acceptance gate at all, so fewer OSD trials get logged/scored downstream — not a
methodology difference).

**Sanity check passed**: the `bp-posterior` run reproduces W2.2's own headline numbers EXACTLY
(hard_200 jt9-verified TP = 1256, noise_1000 FP = 12, chosen FDR-sweep threshold = 0.1902) —
confirming the enum's default arm really is byte-identical to the pre-W2.3 code path, not just in
theory but in this task's own fresh measurement.

### Headline result

| Setting | hard_200 jt9-verified (TP) | hard_200 novel/unverified | noise_1000 (FP) | Definitive-subset FDR at largest safe threshold |
|---|---|---|---|---|
| **BpPosterior (default)** | **1256** | **4970** | **12** | 0.95% (threshold 0.1902) |
| Channel | 1251 (−5) | 4956 (−14) | 1 (−11) | 0.08% (threshold 0.0761) |
| OffsetSubtracted(2.0) | 1251 (−5) | 4956 (−14) | 2 (−10) | 0.16% (threshold 0.0879) |

Both alternatives to `BpPosterior` show the SAME shape: a small but real true-positive loss (−5
jt9-verified, −14 novel/unverified — a systematic ~0.4% recall reduction across both hard_200
buckets) bought with a further reduction in an already-tiny noise-corpus false-positive count (12
→ 1 or 12 → 2). Channel and OffsetSubtracted(2.0) land on IDENTICAL hard_200 totals (1251 TP +
4956 novel) despite being different mechanisms — plausibly because reducing BP-posterior
confidence by a fixed offset pushes OSD's search behavior toward the same practical outcome as
using the channel array directly on this corpus, though this wasn't independently verified
message-by-message and is reported as an observation, not a proven mechanism.

## Which setting wins — honest verdict: `BpPosterior` (no change from default)

This plan is named **decoder-tp-sensitivity** — its explicit mandate is to increase true-positive
recall, not to trade TPs away for incremental precision gains once the FDR target is already met.
`BpPosterior` at the currently-configured gate (0.0976) already achieves **0.95% FDR**,
comfortably under the `--target-fdr 0.01` (1%) bar W2.1 set and W2.2 confirmed. Both `Channel`
and `OffsetSubtracted(2.0)` push the *residual* false-positive count even lower (which would look
attractive in isolation), but only by giving up 5 real jt9-verified decodes and 14 more
plausible-real novel decodes — a strictly worse trade for a plan whose stated goal is recall, when
the precision target this plan itself set is already satisfied by the status quo.

**Verdict: `BpPosterior` (the pre-existing default) wins outright — not a mixed/inconclusive
result.** No config change is needed to "keep the winner as default" because the winner already
is the default; this task's net effect on production is the new (currently-inert) `osd_input`
switch itself, exercised and measured, with the measurement affirmatively supporting leaving it
alone.

## Residual-risk / honest observations

- The 12 (BpPosterior) / 1 (Channel) / 2 (OffsetSubtracted) residual noise-corpus FPs are
  small-N; W2.2 already flagged that its own 12-FP population is "a much noisier anchor" than
  W2.1's original 835-FP sweep. The *relative* ordering (Channel/OffsetSubtracted lower than
  BpPosterior) is consistent across the noise tier, but the absolute counts (1 vs 2 vs 12) are
  small enough that a few WAVs either way could shift the picture; the TP-loss finding (−5, −14),
  based on a much larger and more stable population (1256+4970 = 6226 hard_200 rows), is the more
  reliable of the two observations and is what drove the verdict.
- Per W2.2's own report, some of the residual FPs come from finalization sites NOT gated by the
  osd_input choice at all (order-0's hard-decision path also reads whichever array `osd_input`
  selects, per this task's `osd_search_llrs` wiring, but the acceptance gate itself only applies
  inside order-1/2/3/npre2 — order-0 is deliberately ungated per W2.2). This task did not
  separately decompose how much of the FP delta comes from order-0 vs the gated order-1+ paths;
  flagged as a follow-up if this ever needs finer attribution.
- Did not extend the A/B to the synth curve (deprioritized once the hard_200 + noise_1000 result
  was unambiguous in favor of keeping the default — the brief said "if time allows", and the
  clear verdict didn't warrant the extra ~40 min run for a settled question).

## Full test results

- `cargo test --features transmit -p pancetta-ft8 --lib`: **462 passed, 0 failed** (457 pre-task
  + 5 new `osd_input`/`osd_search_llrs` tests).
- `cargo test --workspace --features transmit`: full green (see task report for the exact run).
- `cargo fmt` (pancetta-ft8, pancetta-research): clean.
- `cargo clippy --features transmit -p pancetta-ft8 --lib --tests -- -D warnings`: clean, zero
  warnings.

## Files changed

- `pancetta-ft8/src/decoder.rs` — `OsdInput` enum, `Ft8Config::osd_input` field,
  `LdpcDecoder::{osd_input, with_osd_input, osd_search_llrs}`, `DecodeContext::osd_input`
  threading, `post_bp_pipeline` call-site update, 5 new unit tests.
- `pancetta-ft8/src/lib.rs` — `OsdInput` re-export.
- `pancetta-research/src/decoder.rs` — `Ft8Decoder::with_osd_input` builder.
- `pancetta-research/src/bin/acceptance_calibration.rs` — `--osd-input` CLI flag.
- `research/scorecards/acceptance_calibration_w23_{bp_posterior,channel,offset2}.csv` (new) — full
  per-setting calibration data.
- `research/experiments/2026-07-07-w23-osd-input-llr-ab.md` (this file, new).
