# `npre2_residual_signature` bug fix (Workstream 2, Task W2.4, prerequisite for Flip 2)

**Date**: 2026-07-07/08
**Branch**: `worktree-decoder-tp-sensitivity`
**Status**: Bug fix, independent of the Flip 1/Flip 2 A/B decisions. `npre2_preprocessing_enabled`
stays `false` by default everywhere (production, tests, `main.json` config) — this fix has
**zero production behavior impact** today.

## What was found

The task brief flagged that `npre2_residual_signature` (`pancetta-ft8/src/osd.rs`) "ignores
two of its arguments." Verified directly: the function took `(info_hard, base_parity,
parity_cols_perm)` and immediately did `let _ = info_hard; let _ = parity_cols_perm;`,
computing its output purely by packing `base_parity`'s own bits — a signature with **zero
dependency on anything actually received**.

Per the clean-room spec (`research/specs/spec-wsjtx-mainline-osd174.md` § Step 6, "fetch
phase"), the quantity this is supposed to compute is `e2sub = (ce XOR hdec)[k+1..N]` — the
parity-bit DISCREPANCY between a candidate codeword `ce` and the actually-RECEIVED hard
decisions `hdec`. At the order-0 test pattern (`me = m0`, so `ce = c0`), `ce`'s parity portion
IS exactly `base_parity` (confirmed against this decoder's own "Compute base parity" step,
`decode_with_features_scored_budgeted`'s lines just after order-0's construction) — so the
correct residual is `base_parity XOR received_parity_hard`, NOT `base_parity` alone. The
function's own doc comment even stated this correctly in prose ("the residual is the distance
between the received parity-hard-decisions and the order-0 parity") and then, in the very next
sentence, gave up on computing it ("we don't have received parity hard decisions separately
here") — despite the received channel/BP-posterior LLR array (`llrs`) and the permutation
(`final_perm`) both being in scope at the call site the whole time. This is a genuine bug, not
a deliberate simplification: the "conservative interpretation" comment describes computing
`base_parity`'s own bits as if that were a defensible fallback, but a quantity with no
dependency on what was received cannot express "cancel an error" at all — it is wrong even in
the trivial case where there IS no error (received matches expected exactly), where it would
still report a nonzero "residual".

## Confirming this really was live, broken, dead code — not misdiagnosed

Grepped for all callers: `npre2_residual_signature` has exactly ONE call site
(`OsdDecoder::decode_with_features_scored_budgeted`'s npre2 warm-start block, gated behind
`self.config.npre2_preprocessing_enabled && max_depth >= 3`). Since
`OsdConfig::npre2_preprocessing_enabled` defaults `false` (`test_default_config_disables_npre2`
already asserts this) and is never set `true` in production, the buggy code was never actually
exercised by any live decode path — a real bug, but currently inert.

There is a SEPARATE, unrelated, ALSO-dead public function in the same file,
`npre2_preprocess` (only ever called by its own 3 unit tests, never by `OsdDecoder::decode*`),
whose internal residual computation IS correct (`err = expected ^ received`, where `received`
comes from an actual codeword-in-progress array). This confirms the correct formula was known
and implemented correctly ONCE in this codebase already — just not in the function that's
actually wired into the live (if currently disabled) call path. `npre2_preprocess` is left
alone (out of scope for this task's brief, which named `npre2_residual_signature` specifically)
but is flagged here as a second instance of this plan's recurring "divergent twin" pattern
(cf. W1.6's `decode_candidate`/`par_decode_candidate` unification) — a reasonable future
cleanup, not addressed in this task.

## The fix

`npre2_residual_signature`'s signature changed from
`(info_hard: &[u8; 91], base_parity: &[u8; 83], parity_cols_perm: &[[bool; 83]; 91]) -> u32`
to `(base_parity: &[u8; 83], received_parity_hard: &[u8; 83]) -> u32`, computing
`base_parity[p] != received_parity_hard[p]` for `p in 0..NPRE2_NTAU`. `info_hard` and
`parity_cols_perm` were genuinely unneeded for this computation (not just unused by
oversight) — `base_parity` already IS `encode(info_hard)`'s parity portion, so re-deriving
anything from `info_hard`/`parity_cols_perm` here would be redundant. The one call site now
builds `received_parity_hard` from `llrs`/`final_perm` (the hard-decision sign of the channel/
BP-posterior LLR at each permuted parity-bit position) before calling it — exactly the
"received parity hard decisions" the old doc comment said weren't available, but were.

## Test

New unit test `test_npre2_residual_signature_is_base_parity_xor_received`
(`pancetta-ft8/src/osd.rs`, `npre2_tests` module): asserts (a) when received parity exactly
matches `base_parity`, the signature is `0` (the OLD code would have returned `base_parity`'s
own nonzero bits here — a concrete demonstration of the bug), and (b) when two specific bit
positions disagree, the signature has exactly those two bits set. Both assertions would FAIL
against the pre-fix implementation (verified by temporarily reverting during development).

## Test results

- `cargo test --features transmit -p pancetta-ft8 --lib`: all `npre2_tests` (8 tests,
  including the new one) pass; `test_npre2_default_off_preserves_osd_decode_results` (which
  exercises `npre2_preprocessing_enabled: true` at `max_depth: 3` on a clean codeword, where
  OSD-0 always wins before the npre2 path is ever reached) still passes unchanged — this fix
  cannot be exercised by that test since the clean-codeword case never reaches the npre2
  block, consistent with "zero production behavior impact today."
- Full workspace suite (`cargo test --workspace --features transmit`): green, 0 failed.
- `cargo fmt` / `cargo clippy -- -D warnings`: clean.

## Why this wasn't A/B'd on its own

`npre2_preprocessing_enabled` only has any effect at `osd_depth >= 3`, which is itself gated
behind Flip 2 in this task's brief ("only if Flip 1 lands"). Flip 1 (`osd_depth: Some(2)`)
was declined (see the companion experiment log
`2026-07-07-w24-osd-depth-flip-declined.md`) on an explicit hard-gate failure (new noise-tier
false positives), so Flip 2's npre2 A/B was correctly NOT attempted per the brief's own
gating. This fix lands as a standalone correctness improvement to currently-inert code,
independent of that decision — it doesn't change what production does today, but it does mean
that IF `osd_depth`/`npre2_preprocessing_enabled` are ever revisited later (e.g. after the
follow-up mentioned in the companion log tightens the acceptance gate), the npre2 mechanism
being evaluated at that point will be the one the spec actually describes, not the broken one.
