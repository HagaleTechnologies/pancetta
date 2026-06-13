# Algorithm spec: f64 tanh-domain belief propagation for FT8 (174,91) LDPC

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- File paths (traceability only, NOT quoted):
  - `crates/ft8core/src/ldpc_decode.rs`, the `bp_decode` function and the
    companion `bp_decode_with_zsum_saves`. Both run identical BP logic;
    the latter adds snapshot bookkeeping (see the sister spec
    `spec-wsjtr-zsum-osd-init.md`).
  - The wsjtr authors describe this as a port of WSJT-X's
    `lib/ft8/decode174_91.f90`.
- Companion docs: `docs/jt9r.md` ("LDPC Decoding" subsection).
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

Belief propagation (BP) is the inner-loop decoder for the FT8 (174, 91)
LDPC code. It iteratively exchanges soft messages between variable nodes
(174 codeword bits) and check nodes (83 parity checks) until either the
hard-decision codeword satisfies all parity checks (success) or an
iteration budget is exhausted (failure → fall back to OSD).

The numerically delicate part of BP is the check-node update: the new
message from check node `c` to variable node `v` is
`2 * atanh(prod_{v' != v} tanh(msg_{v'→c} / 2))`. The product can range
from near +1 (very confident) to near -1 (very confident wrong) and the
`atanh` near ±1 is ill-conditioned: a `tanh` that saturates to within
1e-7 of 1.0 in single precision turns into infinity after `atanh`. To
manage this, the implementation must clamp the product (or compute the
update in log-domain via `log|tanh| + sign` to avoid representing the
product directly).

Pancetta currently runs this entire chain in **f32**, using
`fast_tanh`/`fast_atanh` approximations (`pancetta-ft8/src/decoder.rs`,
near lines 5681 and 5692). The f32 path is fast and works for
strong-signal decode, but at low SNR the BP messages have small absolute
values, and small-difference accumulation accumulates rounding error.
On the marginal bin (signals at the noise floor), the difference between
"BP converges in iteration 22" and "BP just barely fails to converge
within 30 iterations" is sometimes a sub-percent difference in
intermediate message magnitudes — exactly what f32 rounding can change.

wsjtr's BP runs in **f64** end to end: every message variable, every
LLR, every accumulator is f64. The `tanh` and `atanh` are the standard
library f64 implementations (no fast approximation). The product is
computed in log-magnitude + sign decomposition (`sign_excl * exp(log_excl)`),
which keeps the dynamic range well-controlled.

The hypothesis is that the f64 path recovers marginal decodes that f32
just barely misses, especially at OSD-2 where small differences in the
saved zsum snapshots affect which bit positions get nominated as least
reliable. The MEMORY-cited "numerical precision affects OSD-2 marginal
recovery" is the load-bearing claim.

## Algorithm description (PROSE ONLY — no code)

### Inputs

- `llr`: a 174-element array of channel LLRs, conventionally
  `negative ⇒ bit = 1` (the wsjtr / WSJT-X sign convention). Type is f32
  at the input boundary (because the LLR generator upstream is in f32);
  the BP promotes to f64 immediately.
- `max_iterations`: positive integer, default 30. The iteration cap.

### Outputs

- `Option<[u8; 77]>`: the 77 message bits if BP converges within the
  iteration budget and CRC-14 matches; `None` otherwise (caller falls
  back to OSD).

### Internal state

All in f64:

- `tov[edge]`: check-to-variable message for each edge of the Tanner
  graph. Length = N_edges. Initialized to 0.0.
- `toc[edge]`: variable-to-check message for each edge. Same length.
- `tanh_toc[edge]`: `tanh(toc[edge] / 2)`. Reused as a temporary.
- `sign[edge]`: sign of `tanh_toc[edge]`, ±1.0.
- `log_abs[edge]`: `ln(|tanh_toc[edge]|)`. Clamped at the bottom to
  `ln(1e-30) ≈ -69.07` to avoid `-Inf`.
- `zn[var]`: per-variable posterior LLR. Length 174.
- `llr_f64[var]`: f64-promoted copy of the input LLR. Length 174.
- `nclast`, `ncnt`: integer state for early stopping.

### Tanner graph

Pancetta and wsjtr both use the same FT8 (174,91) LDPC parity check matrix.
The graph has:

- 174 variable nodes (codeword bits).
- 83 check nodes (parity checks, since 174 − 91 = 83).
- Each variable node connects to exactly 3 check nodes (the variable
  degree is 3 — every bit appears in exactly 3 parity checks; the row
  table in the wsjtr source is the standard `Nm` table from the FT8
  protocol spec).
- Edge count: N_edges = 174 × 3 = 522. (Check degrees vary; the row sum
  over the check-bit table is also 522, confirming the bipartite
  structure.)

The graph is computed once at startup from the constant table and stored
as:

- For each check node `j`: list of variable indices it touches.
- For each variable node `v`: 3-tuple of edge indices it touches.

Pancetta's existing `ldpc.rs` already builds this graph; the BP change
does not affect the graph structure.

### Steps (per BP call)

1. **Promote**: copy `llr` to `llr_f64` as f64.
2. **Initialize** `tov[edge] = 0.0` for all edges.
3. **Iterate** `iter` from 0 to `max_iterations` inclusive:

   a. **Variable update**: for each variable node `bit` in 0..174,
      compute `zn[bit] = llr_f64[bit] + sum over edges e in
      bit_edges[bit] of tov[e]`. (For variable-degree-3 codes, the sum
      has exactly 3 terms.) This is the running posterior LLR.

   b. **Hard decision**: for each `bit`, `codeword[bit] = 1 if zn[bit] < 0
      else 0`.

   c. **Parity check count**: for each check node, XOR the codeword bits
      it connects to; count check nodes whose parity is nonzero. Call
      this `n_unsatisfied`.

   d. **Success test**: if `n_unsatisfied == 0`, extract the first
      `K_LDPC = 91` codeword bits as the message-with-CRC. Run CRC-14:
      compute the 14-bit CRC of the first 77 bits and verify it matches
      bits 77..90. If CRC matches, return `Some([bits 0..76])`. If CRC
      fails, **do not return early** — fall through to the next
      iteration. (BP convergence without CRC happens on noise; CRC is
      the gatekeeper.)

   e. **Early-stop heuristic** (only if `iter > 0`):
      - Compute `nd = n_unsatisfied - nclast`.
      - If `nd < 0` (improving), reset `ncnt = 0`.
      - Else, increment `ncnt`.
      - If `ncnt >= 5` AND `iter >= 10` AND `n_unsatisfied > 15`, return
        `None` (give up; the candidate is hopeless).
      - Then set `nclast = n_unsatisfied`.

   f. **Exit-on-budget**: if `iter >= max_iterations`, break out of the
      loop and return `None`.

   g. **Variable-to-check message**: for each edge `e` (which connects
      variable `bit` to some check), set `toc[e] = zn[bit] - tov[e]`.
      This is the "exclude the message coming back from this check"
      extrinsic computation. (Use of the running `zn[bit]` minus `tov[e]`
      is the standard "exclude this edge from the sum" trick.)

   h. **Tanh / log-magnitude decomposition**: for each edge `e`:
      - Compute `t = tanh(toc[e] / 2.0)`.
      - Clamp: if `t > 0.999999`, set `t = 0.999999`. If `t < -0.999999`,
        set `t = -0.999999`. This prevents the downstream `atanh` from
        producing `±Inf`. (The clamp value is `1 - 1e-6`, which is well
        above f64 precision but below the saturation knee for atanh.)
      - Store `tanh_toc[e] = t`.
      - Compute `sign[e] = 1.0 if t >= 0.0 else -1.0`.
      - Compute `log_abs[e] = ln(max(|t|, 1e-30))`. The `max` guards
        against `t = 0` exactly producing `-Inf`.

   i. **Check-to-variable update**: for each check node `j`:
      - Walk all edges incident to `j` once. Compute
        `total_sign = product over edges of sign[e]` (this is ±1.0,
        accumulated as a running product).
      - Compute `total_log = sum over edges of log_abs[e]`.
      - Walk the edges of `j` again. For each edge `e` of `j`:
        - `sign_excl = total_sign * sign[e]` (multiplying by `sign[e]`
          once removes it from the product because `sign[e] * sign[e] = 1`
          for ±1 values).
        - `log_excl = total_log - log_abs[e]`.
        - `prod = sign_excl * exp(log_excl)`.
        - Clamp: if `prod > 0.999999`, set `0.999999`. If `< -0.999999`,
          set `-0.999999`.
        - `tov[e] = 2.0 * atanh(prod)`.

   The log-magnitude decomposition is what makes the check-node update
   numerically well-behaved. The mathematical equivalent would be
   "compute the product of tanh values excluding edge `e`", which
   underflows in f32 (and even occasionally in f64) when many tanh values
   are near zero. By summing logs and re-exponentiating after the
   single-edge subtraction, the working magnitude stays in a sane range.

   The clamp values (`0.999999`) appear in two places: on the tanh before
   logging (to avoid `log(0)`) and on the reconstructed product before
   atanh (to avoid `atanh(±1) = ±Inf`). Both are facts of the source; do
   not loosen.

4. **Failure return**: if the loop exits without converging on a
   CRC-valid codeword, return `None`.

### Numerical constants (facts, not expression)

- LLR precision in BP: f64. Promote from input f32 on entry.
- Tanh / atanh: standard library f64 (no `fast_*` approximation).
- `tanh_toc` clamp: ±0.999999 (i.e. `1 − 1e-6`).
- `log_abs` floor: `ln(1e-30) ≈ −69.07`.
- Reconstructed-product clamp before atanh: ±0.999999.
- Default `max_iterations`: 30.
- Early-stop heuristic constants:
  - Minimum iteration before bailing: 10.
  - Consecutive non-improving iterations before bailing: 5.
  - Unsatisfied-check floor below which we keep trying: 15 (i.e. only
    bail if `n_unsatisfied > 15`).
- CRC: CRC-14 (polynomial standard for FT8 protocol; pancetta's
  `pancetta-ft8/src/protocol.rs` or equivalent already implements it).
- Variable-node degree: 3 (each codeword bit appears in 3 parity checks).
- Total edges: 522.
- Message bits: 77 (the first 77 of the 91-bit message+CRC layer; the
  remaining 14 bits are CRC).

### Edge cases

- **All-zero LLR input**: every variable's `zn` stays at 0 forever; hard
  decisions are all 0; the all-zero codeword is a valid LDPC codeword
  (parity-zero) but its CRC-14 is nonzero (CRC of 77 zero bits is the
  CRC-14 of zero, which is itself zero — actually the all-zero CRC of
  all-zero message IS zero, so the all-zero "decode" passes CRC). The
  source does not special-case this because the upstream LLR generator
  reliably produces nonzero LLRs from real audio. Pancetta should mirror
  the wsjtr behaviour: do not check for zero LLRs in BP itself.
- **One iteration runs but cannot converge**: the iteration budget of 30
  is enough for converged signals; the early-stop heuristic catches
  hopeless candidates after 15 iterations or so.
- **Mid-iteration parity satisfied but CRC fails**: a known LDPC
  failure mode — a noisy channel produces a valid codeword that is not
  the transmitted message. The implementation must continue BP (not
  return) when CRC fails on a parity-satisfied codeword. The OSD
  fallback (via zsum snapshots; see sister spec) is the mitigation.
- **Pathological saturation: a tanh comes back exactly +0.999999 from
  saturation**: handled by the clamp before logging.
- **Pathological underflow: a tanh comes back exactly 0**: `log(1e-30)`
  via the `max` guard returns `-69.07`. Downstream the
  `total_sign * exp(total_log - log_abs[e])` produces a small but finite
  value. Safe.
- **Two BP-runs of the same LLR**: deterministic. f64 ops are bitwise
  reproducible on a given CPU; rustfft / standard library `tanh`/`atanh`
  are deterministic. Required for bootstrap-CI policy.

## Conflict with pancetta's existing mechanisms

Pancetta's BP runs in f32 with `fast_tanh` and `fast_atanh`
approximations (`pancetta-ft8/src/decoder.rs` lines ~5681 and 5692; the
core BP loop is around line 5973 `belief_propagation` and the layered
variant around 6077). The approximations are designed for speed: a
single multiply-divide approximation of tanh, plus a careful
table-driven atanh. They are 100% accurate enough for healthy LLRs but
incur a small (<1e-4) relative error.

Conflicts to think through:

1. **f32 → f64 cost**: the per-iteration cost of the BP loop dominates
   when OSD is not run. Going f64 doubles register pressure and roughly
   halves SIMD width on most architectures. Expected slowdown: ~1.5×
   to 2× on the BP-only path. Mitigation:
   - The hb-091 scoped fast path / hb-216 hardware tier already gates
     decode work by cost. Slow tier keeps f32; Fast tier moves to f64.
     Moderate tier is an A/B target.
   - Pancetta's BP is not the dominant decode cost when neural OSD is
     enabled — OSD itself dominates. The f64 BP cost is small on
     net for Fast tier.
2. **Removing `fast_tanh` / `fast_atanh`**: should not be removed; they
   stay as the f32 path. Add a new f64 path in parallel. The f32 path
   is kept for the Slow tier and for backwards compatibility tests.
3. **Numerical determinism vs. existing bootstrap-CI**: the f64 path
   produces *different* numerical messages from the f32 path, so the
   absolute decode counts will shift. Mitigate by treating this as a
   bootstrap-CI graduation: run hard-200 with both paths, confirm the
   f64 path has nonnegative recall delta with no FP regression.
4. **Interaction with neural OSD**: neural OSD consumes LLRs (the saved
   zsum snapshots if the zsum-init spec is also implemented; otherwise
   the channel LLRs). The OSD itself can stay in its current precision;
   the f64 affects only the BP messages and the zsum accumulators.
5. **Layered (sequential) BP variant** (`layered_bp: true` in
   `pancetta-ft8/src/decoder.rs:545`): pancetta already has both
   flooding and layered (Gauss-Seidel-style) schedules. wsjtr uses
   flooding. The precision change is orthogonal to the schedule; both
   schedules can be promoted to f64 independently.
6. **Existing bp_offset_subtract knob** (around line 273): the offset
   subtraction is a separate mechanism (a min-sum-style attenuation of
   over-confident BP messages). The precision change is orthogonal; both
   stay.
7. **CRC and parity logic**: stays in u8/u32, unchanged.
8. **Interaction with `spec-wsjtr-zsum-osd-init.md`**: that spec
   requires the zsum accumulator to be accurate. If zsum is computed in
   f64 along with BP, the snapshots are stably more precise; saved as
   f32 for the OSD consumer is acceptable (or saved as f64 if pancetta
   wants the maximum precision through to OSD).
9. **Interaction with the cached-bandpass downsampler spec**: orthogonal.
   The downsampler produces LLRs; BP consumes them. The interface (a
   174-element f32 array per attempt) is unchanged.
10. **Interaction with `spec-wsjtr-sync-bc.md` / `spec-wsjtr-sync-norm.md`
    / `spec-wsjtr-grid-refinement.md`**: orthogonal; all sync-stage.

## Estimated Rust port effort

- ~150 LOC for the f64 BP function. It is structurally identical to the
  existing f32 path; the change is mechanical (types + tanh/atanh source).
- ~30 LOC for tier gating (read `scoped_fast_path` atomic or a new
  `bp_precision: enum { F32, F64 }` config).
- ~80 LOC for unit tests: round-trip a synthetic LLR through both paths
  and confirm both decode the same strong-signal codeword; introduce
  controlled LLR noise and verify the f64 path tolerates higher noise
  before failing.
- 1 iter session for the implementation + tests.
- 1 iter session for hard-200 A/B eval with bootstrap-CI graduation.
- 1 iter session for tier integration if eval is positive.
- Total: 2–3 iter sessions, ~260 LOC.

## Implementation notes for the implementer thread

- Splice point: pancetta-ft8/src/decoder.rs `belief_propagation` (around
  line 5973) and `belief_propagation_with_trajectory` (around line 6077).
  Both are flooding BP; pancetta also has a layered variant — promote
  both, gated on a single `bp_precision` config knob.
- The existing `fast_tanh` and `fast_atanh` helpers are f32-only. Do
  **not** remove them; add `tanh_f64` (which is just `f64::tanh`) and
  `atanh_f64` (which is `f64::atanh`) as the f64 alternates. Std-lib
  `f64::tanh` is bit-exact across platforms (modulo the host's libm
  conformance) and is what wsjtr uses.
- The log-magnitude decomposition is the critical reasoning detail.
  Implement it exactly as the source: maintain `sign[edge]` as ±1.0 and
  `log_abs[edge] = ln(max(|tanh|, 1e-30))`. The check-node loop walks
  edges twice — once to accumulate `total_sign` and `total_log`, once
  to compute each excluded-edge message as
  `sign_excl * exp(log_excl)` then `2 * atanh(...)`. Do not "simplify"
  to a direct product-of-tanh computation; that loses the dynamic-range
  control the log decomposition gives you.
- The clamp at `±0.999999` appears twice. Both are required.
- The early-stop heuristic constants (5, 10, 15) come from the
  WSJT-X `decode174_91.f90` source via wsjtr. Treat them as facts.
  Pancetta may already have its own early-stop tuning; either preserve
  it for the f32 path and adopt the wsjtr constants for the f64 path,
  or align both. Test both.
- For unit tests, build a round-trip: pack a CQ message → CRC → LDPC
  encode → introduce a controlled per-bit flip with LLR magnitude ε
  (e.g. ε = 0.05), confirm f64 BP recovers it, confirm f32 BP at the
  same ε does or does not. The MEMORY hypothesis is that there exist
  ε values where f64 succeeds and f32 fails; demonstrate this with a
  specific test fixture.
- For the eval, compare on hard-200 with two configs identical except
  for `bp_precision`. Expected signal: small but nonzero recall lift,
  concentrated in the OSD-2 marginal bucket (the f64 BP feeds better
  zsum snapshots, OSD-2 recovers signals just below the BP-only
  threshold). Estimated lift: +0.5% to +2.0% recall, no FP regression.
- Determinism caveat: f64 `tanh` and `atanh` are deterministic per host
  but not byte-identical across libm implementations (musl vs glibc vs
  macOS). Pancetta's bootstrap-CI runs on a single host so this is
  fine in practice; document it.
- Cite as `wsjtr-inspired` in the journal entry. The original
  precision choice in WSJT-X is f64 throughout; the wsjtr source is
  the inspection target.
- This is a quick win if it pans out (single-knob A/B, all the existing
  BP code structure is preserved). Recommended sequencing: implement
  in 1 session, eval in the next, ship if positive.
