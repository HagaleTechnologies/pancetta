# Algorithm spec: zsum-snapshot OSD initialization (BP-softened LLRs into OSD)

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- File paths (traceability only, NOT quoted):
  - `crates/ft8core/src/ldpc_decode.rs`, `bp_decode_with_zsum_saves`.
    Sister BP function `bp_decode` is the same loop without snapshot
    bookkeeping; cross-reference the sister spec
    `spec-wsjtr-f64-tanh-bp.md`.
  - `crates/ft8core/src/osd.rs`, `decode_bp_then_osd` (the orchestrator
    that runs BP, collects snapshots, and dispatches them to OSD), plus
    `osd_decode` (the OSD pass itself) and the `OsdParams::from_depth` /
    `OsdMode` presets.
  - The wsjtr authors describe this as a port of WSJT-X's
    `lib/ft8/decode174_91.f90`.
- Companion docs: `docs/jt9r.md` ("LDPC Decoding" subsection,
  "zsum accumulation" paragraph). The OSD false-positive analysis in
  `docs/wsjtr.md` (the "OSD False Decode Analysis (Feb 2026)" section)
  documents the distance-tracking fix that pairs with this work.
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

OSD (Ordered Statistics Decoding) is the fallback decoder for FT8 LDPC
when BP fails. Its quality depends critically on its **input** — the
soft LLRs it uses to rank bits by reliability and to weight Hamming
distances. The naive choice is "the channel LLRs from the soft-bit
demodulator": this is what pancetta's `osd.rs` currently does
(`pancetta-ft8/src/osd.rs` `OsdDecoder::decode`, which sorts by
`|llrs[i]|` directly). It works, but it leaves recall on the table.

The reason it leaves recall: BP, even when it fails to converge to a
parity-satisfying codeword, has done useful work. Each iteration
exchanges messages that pull each bit's running posterior LLR closer to
the maximum-likelihood estimate. The first one or two iterations move
the messages out of the "completely uncertain" zone for bits that have
strong evidence. By iteration 10 or 15, the running posterior has
"smoothed" the channel LLRs — bits with consistent evidence get
amplified, bits with conflicting evidence get attenuated. This
smoothed view is a strictly better reliability ranking for OSD than
the raw channel LLRs.

WSJT-X formalizes this as: during BP, accumulate the running
posteriors into a `zsum` array (`zsum[bit] += zn[bit]` each iteration).
At specific early iterations (iterations 1 and 2), take a snapshot of
`zsum`. After BP either succeeds or gives up, if BP failed, run OSD
**twice** — once on the iteration-1 snapshot and once on the
iteration-2 snapshot. Whichever recovers a CRC-valid codeword first
wins. As a final fallback, OSD is run a third time on the raw channel
LLRs.

The "why early iterations" insight: by iteration 1, BP has incorporated
exactly one round of check-node feedback. By iteration 2, two rounds.
Beyond that, when BP is failing, later iterations often start
oscillating or saturating, and the snapshots become noisier. Iterations
1 and 2 capture the sweet spot of "BP has helped, but not yet
diverged."

For OSD itself, the wsjtr source pairs the zsum-snapshot init with a
critical fix to the distance-tracking logic (documented in detail in
`docs/wsjtr.md`'s "OSD False Decode Analysis"). The old wsjtr behavior
— "accept any CRC-valid codeword found at any order" — was the source
of a 40+/10-window false-decode rate. The fix: track the **minimum
weighted Hamming distance** codeword regardless of CRC, then check CRC
only on the final winner. This pairs with the zsum snapshots because
better OSD input increases the *recall* of true codewords; the
distance fix protects the *precision* against noise.

Pancetta's existing OSD pipeline does not snapshot zsum and does not
have the distance-tracking fix. Both are headroom.

## Algorithm description (PROSE ONLY — no code)

### Two-part algorithm

This spec covers two coupled mechanisms:

**Part A — zsum snapshot capture during BP**: modify the BP loop to
accumulate posteriors and save snapshots at iterations 1 and 2.

**Part B — OSD orchestration**: try OSD on snapshot 1, then snapshot 2,
then the channel LLRs, accepting the first CRC-valid result.

(A third coupled mechanism — the min-distance tracking fix inside OSD
itself — is documented here at the orchestration boundary because it
is what makes zsum-init safe at scale. The implementer should treat
it as part of the same change.)

### Inputs (Part A)

- `llr`: 174-element array of channel LLRs (same as the sister
  `spec-wsjtr-f64-tanh-bp.md`).
- `max_iterations`: positive integer, default 30.
- `max_saves`: positive integer, default 2 (the source caps at 2;
  raising it has not been evaluated by the wsjtr author).

### Outputs (Part A)

- A pair: `(Option<[u8; 77]>, Vec<[f32; 174]>)`. The first element is
  the BP-decoded message if BP itself converged with a CRC-valid
  codeword. The second element is the captured snapshots (zero, one, or
  two of them, depending on when BP terminated).

### Snapshot capture (Part A)

The BP loop runs identically to the f64 tanh BP described in the
sister spec, with the following additions:

1. Allocate a 174-element f64 `zsum` buffer, initialized to 0.0.
2. Allocate a `Vec` of saved snapshots, capacity `max_saves`.
3. In the per-iteration variable update step, **after** computing
   `zn[bit]` for all 174 bits, accumulate:
   `zsum[bit] += zn[bit]` for each bit.
4. **At the end of iteration 1 and iteration 2**, if the snapshot vector
   has fewer than `max_saves` entries, take a snapshot:
   - Convert each `zsum[bit]` to f32.
   - Push the resulting 174-element f32 array to the snapshot vector.
   - The snapshot is taken even if BP is going to fail later; the
     consumer decides whether to use it.
5. Continue BP normally (success → return Some(bits) plus the
   accumulated snapshots; early-stop or budget-exhaust → return None
   plus the accumulated snapshots).

The snapshots are running cumulative sums, not single-iteration
posteriors. The iteration-2 snapshot is `zn_iter1 + zn_iter2 + zn_iter0`
(where `iter0` is the pre-message-passing initialization with
`zn = llr`). The cumulative form acts as a temporal smoother of the
BP messages, which is what the source picks; do not switch to
single-iteration posteriors.

The snapshot is taken **after** the variable update of that iteration
but **before** the check-node update of that iteration. The
distinction matters for iteration ordering: iteration 1's snapshot
captures the LLR plus exactly one round of accumulated check-node
feedback (the `tov` values that were computed in iteration 0's
check-node update). Iteration 2 captures the LLR plus two rounds.

### Inputs (Part B)

- `llr`: original channel LLRs (174-element f32 array).
- `bp_max_iterations`: passed through to BP.
- `osd_params`: OSD strategy presets (order, pool size, npre1
  preprocessing). Same as the existing OSD configuration in pancetta.

### Outputs (Part B)

- `Option<[u8; 77]>`: the 77 message bits, or `None`.

### Orchestration (Part B)

1. **Order-zero short circuit**: if `osd_params.nord == 0`, skip OSD
   entirely. Call BP-only and return its result.

2. **Run BP with snapshots**: call the snapshot-capturing BP. Receive
   back `(bp_bits, saves)` where `saves` is a vector of zero or more
   174-element f32 LLR-like arrays.

3. **BP-success path**: if `bp_bits` is `Some(bits)`, return it. (BP
   converged with CRC-valid codeword.)

4. **Snapshot OSD passes**: iterate over `saves` in order (snapshot 1
   first, snapshot 2 second). For each snapshot, call OSD with the
   snapshot as the LLR input. If OSD returns `Some(bits)`, return it
   immediately.

5. **Channel-LLR OSD fallback**: if no snapshot recovered a codeword,
   call OSD one final time on the original channel `llr`. Return
   whatever it returns (Some or None).

### OSD-side requirements (paired distance fix)

Inside the OSD `osd_try_order` routine (the inner loop that flips
combinations of bits in the most-reliable-bits/MRB pool):

- **Initialize** `best_dist` to the weighted Hamming distance of the
  order-0 codeword (the hard-decision MRB → re-encode → measure
  distance from the received hard decisions). **Do not** initialize
  `best_dist` to infinity.
- **Initialize** `best_cw` to the order-0 codeword.
- For each flip-pattern in the order-1, order-2, … sweep:
  - Compute the candidate codeword by flipping the corresponding MRB
    bits and re-encoding via the systematic-form generator.
  - Compute the candidate's weighted distance from the received hard
    decisions (the weights are the `|LLR|` values in the permuted
    coordinate frame).
  - If the candidate's distance is strictly less than `best_dist`,
    update `best_dist` and `best_cw`. **Do not check CRC inside the
    loop.**
- After the entire flip sweep completes, **check CRC** on `best_cw`.
  - If CRC passes, return `Some(message_77)`.
  - If CRC fails, return `None`.

This is the load-bearing precision fix. The naive "accept any
CRC-valid codeword found in the sweep" lets random patterns slip
through at probability ~4% per OSD call on pure noise (per the
`docs/wsjtr.md` analysis). The min-distance-then-CRC structure
constrains acceptance to "the codeword closest to the received signal,
which happens to also pass CRC" — a much rarer event in noise.

### Snapshot OSD pass: per-snapshot full OSD

When OSD is invoked on a snapshot, the snapshot is treated *exactly*
like a fresh channel LLR input. The OSD routine:

1. Hard-decides each bit from snapshot sign (negative ⇒ 1).
2. Sorts bits by |snapshot value| descending (most reliable first).
3. Permutes the systematic generator according to the new ordering.
4. Runs Gaussian elimination to put the first 91 columns in identity
   form.
5. Computes the order-0 codeword and its weighted distance.
6. Selects a pool of the K least-reliable MRB bits.
7. Sweeps flip combinations of orders 1..nord with min-distance
   tracking (per the paired fix above).
8. Checks CRC on the winner.

The snapshot is *not* used in any way other than as a reliability
ranking and weight source. The reliability ranking is *better*
because BP has smoothed it; the weights are *better* because the
amplifications and attenuations track which bits BP found certain or
uncertain.

### Numerical constants (facts, not expression)

- `max_saves`: 2 (snapshots taken at iterations 1 and 2 of BP).
- BP iteration count: same as sister spec (default 30).
- OSD min-distance initialization: order-0 codeword's weighted
  distance.
- OSD CRC gate: applied to the min-distance winner *after* the entire
  sweep completes; never applied per-trial.
- OSD MRB pool sizes (existing wsjtr presets, the OsdParams::from_depth
  table, included here for completeness of the orchestration spec):
  - Depth 0..=1: nord=0 (OSD disabled), pool=0.
  - Depth 2 (= ndeep=1): nord=1, pool=91 (K_LDPC), no preprocessing.
  - Depth 3 default mode: nord=2, pool=36, no preprocessing (the
    historical pancetta pool size).
  - Depth 3 deep mode: nord=2, pool=91 with npre1 preprocessing
    (nt=40, ntheta=10).
  - Depth 3 fast mode: nord=1, pool=91 with npre1 preprocessing
    (nt=40, ntheta=10) — corresponds to ndeep=2.
- npre1 (preprocessing rule 1) constants when enabled: nt=40 parity
  bits checked; ntheta=10 or 12 max errors plus flip-weight allowed
  before pruning.

### Edge cases

- **BP succeeds on iteration 0**: no snapshots are saved (snapshots
  are taken at iteration 1 and 2; iteration-0 is the pre-message-pass
  initialization). `saves` is empty. Orchestration returns the
  BP-success result; no OSD runs.
- **BP succeeds on iteration 1**: one snapshot is saved (snapshot 1).
  BP returns Some; orchestration returns it; the snapshot is
  discarded. Behaviour is correct.
- **BP fails before iteration 1**: empty `saves` vector. Orchestration
  falls through to channel-LLR OSD. Behaviour matches pre-zsum
  pancetta.
- **BP fails at iteration ≥ 2**: both snapshots are present.
  Orchestration tries them in order.
- **Both snapshots OSD-decode the same valid codeword**: orchestration
  returns the first one (snapshot 1's winner). Both are equivalent;
  no harm.
- **Snapshot 1's OSD finds a CRC-valid codeword that is not the true
  message**: this is the false-positive failure mode the paired
  distance-tracking fix is designed to suppress. With the fix, the
  probability of accepting a wrong codeword falls to near
  WSJT-X-equivalent levels.
- **Snapshot is identically zero**: would happen only if `zn[bit] = 0`
  for all bits and all iterations, which requires the input LLR to be
  identically zero. OSD's input-zero guard (`max_abs <= 1e-6`) rejects
  this case. Safe.
- **Numerical underflow in zsum accumulation**: with f64 accumulator
  and per-iteration `|zn[bit]| < ~10` typical magnitudes, the f64
  range is many orders of magnitude away from underflow even at
  iteration 30. Safe.

## Conflict with pancetta's existing mechanisms

Pancetta's `pancetta-ft8/src/osd.rs` `OsdDecoder::decode` currently
takes the channel LLRs directly and sorts by `|llrs[i]|`. There is no
snapshot mechanism, and the LLR scoring is independent of BP. The
existing `OsdDecoder::try_solution` (around line 433) is the
brute-force flip sweep; the implementer should audit whether it
currently uses the min-distance-track-then-CRC structure or the older
accept-first-CRC-match structure.

Conflicts and integration points:

1. **Pancetta has a neural OSD** (`pancetta-ft8/src/neural_osd.rs`,
   `neural_osd_weights.rs`) that overrides the reliability ranking
   using a learned predictor. The zsum snapshot mechanism is
   **orthogonal** to the neural ordering — both produce a reliability
   ranking. The integration question:
   - Option A: feed the zsum snapshot to the neural OSD as the input
     LLR, letting the neural ordering be computed against the smoothed
     LLR.
   - Option B: feed the channel LLRs to the neural OSD, and use zsum
     snapshots only for the non-neural fallback.
   - Option C: try both — run OSD with both orderings on each
     snapshot, keep min-distance winner.
   Without eval data, Option A is the simplest and most consistent
   with the wsjtr design; the neural OSD is trained on real LLR
   distributions, and BP-smoothed snapshots are still LLR-distributed.
2. **Distance-tracking fix**: pancetta's existing OSD should be
   audited; if `try_solution` currently accepts the first CRC-valid
   candidate, that is the false-positive failure mode wsjtr identified
   in Feb 2026. The fix is required for the snapshot mechanism to be
   safe at scale. Implementer must apply the fix even if zsum is the
   main goal, otherwise the snapshot recall lift will be paid for in
   FP regression.
3. **f64 BP coupling**: zsum is most accurate when accumulated in
   f64. If pancetta has not adopted the sister `spec-wsjtr-f64-tanh-bp.md`
   spec, the zsum accumulator can still be f64 (promote each f32
   `zn[bit]` on assignment); the f32 BP messages limit precision
   slightly, but the zsum-init mechanism still recovers most of its
   benefit. Order of implementation: zsum-init can go before the f64
   BP if the implementer wants to ship faster.
4. **Performance**: cost of zsum accumulation is one f64 add per bit
   per iteration = 174 × 30 = ~5,200 adds per BP call. Negligible.
   Cost of running OSD twice extra per failed BP call (snapshot 1 + 2)
   doubles or triples the OSD compute per failed candidate. On a
   crowded band with many failed candidates this matters:
   - On Slow tier (hb-216), keep OSD on channel LLRs only (skip
     snapshot OSD passes).
   - On Moderate tier, run snapshot 1 only (skip snapshot 2).
   - On Fast tier, run both snapshots plus channel LLRs as fallback.
5. **Interaction with the cached-bandpass downsampler spec**:
   complementary. The downsampler improves the channel LLR; zsum-init
   improves how those LLRs are presented to OSD. Both are net wins.
6. **Interaction with the FP-filter stack (hb-052, hb-058, hb-103,
   hb-217)**: orthogonal; all post-decode. The zsum-init mechanism
   does not change the LDPC contract.
7. **Bootstrap-CI graduation**: zsum + distance-fix together. Run
   hard-200 with old OSD baseline; run again with new OSD; bootstrap
   the recall delta and FP delta. Expected: clean win in recall
   (1-3% on lid_of_band-class corpora), no FP regression
   (distance-fix is precisely what protects against it).

## Estimated Rust port effort

- **Part A (zsum snapshots in BP)**: ~30 LOC added to the BP function
  (`pancetta-ft8/src/decoder.rs` `belief_propagation` around line
  5973). If implementing alongside the f64 BP from
  `spec-wsjtr-f64-tanh-bp.md`, this lands in the same edit.
- **Part B (orchestration)**: ~50 LOC for the `decode_bp_then_osd`
  wrapper. Likely a new function in
  `pancetta-ft8/src/osd.rs` or `decoder.rs`.
- **Distance-tracking fix in OSD**: ~30 LOC change to
  `OsdDecoder::try_solution` or its equivalent. Verify that pancetta's
  current code does or does not track min-distance regardless of CRC;
  fix if not.
- **OSD tier gate**: ~20 LOC.
- **Unit tests**: ~150 LOC. Cases:
  - Strong-signal BP convergence at iteration 0: empty snapshot vec;
    BP success returned; no OSD called.
  - Marginal BP failure: BP fails at iteration ≥ 2; two snapshots
    captured; OSD on snapshot 1 succeeds → BP-then-OSD returns the
    recovered message.
  - Same as above but only snapshot 2 succeeds.
  - Pure-noise input: BP fails; both snapshots fail OSD; channel-LLR
    OSD also fails (distance-fix ensures this); `None` returned.
  - Distance-fix-specific test: synthetic LLR where the old
    accept-first-CRC-match would incorrectly accept a non-true
    codeword; verify the fix rejects it.
- **Eval**: 1 session for hard-200 A/B + bootstrap-CI.
- Total: ~280 LOC, 2 iter sessions.

## Implementation notes for the implementer thread

- Implement Part A first as a one-shot edit to the BP routine: add a
  `&mut Vec<[f32; 174]>` parameter to a new BP-with-snapshots function
  (do not modify the existing BP function — keep both for backward
  compatibility). Snapshot at iteration 1 and 2 only, after the
  variable update step, before the check-node update step.
- Implement Part B as a separate orchestrator function. It calls the
  BP-with-snapshots function, handles the success-without-snapshots
  case, and dispatches OSD on snapshots then channel LLRs.
- Implement the distance-tracking fix in OSD first, then implement
  zsum. The distance fix is the precision guard; zsum is the recall
  lift. Reverse order ships a known-bad false-positive regression.
- Splice points:
  - Snapshot BP: `pancetta-ft8/src/decoder.rs:5973`
    `belief_propagation` is the entry; add a new
    `belief_propagation_with_zsum` adjacent to it.
  - OSD distance-track audit: `pancetta-ft8/src/osd.rs:433`
    `try_solution` is the inner loop. Confirm the current behaviour.
  - Orchestrator: anywhere in `decoder.rs` or `osd.rs` where the
    consumer calls "BP first, OSD on failure" sequentially today.
    Wrap it with the new orchestrator.
- The snapshot save is f32-converted from the f64 zsum accumulator.
  Pancetta consumers (neural OSD, classical OSD) all take f32 LLR
  arrays. No interface change required.
- Determinism: BP is deterministic; zsum is deterministic; OSD is
  deterministic. The full pipeline is therefore deterministic for a
  given input, on a given host. Required for bootstrap-CI.
- Unit-test the distance-fix in isolation: construct a synthetic
  scenario where the order-0 codeword is the true message with
  distance D, and an order-1 flip produces a CRC-valid (but wrong)
  codeword with distance D' > D. The fix should accept the
  order-0 codeword (since it has lower distance) and check CRC on
  it (which it passes). The old behaviour would have accepted the
  order-1 wrong codeword on first CRC match.
- Unit-test the snapshot mechanism: construct a synthetic LLR where
  BP fails after 5 iterations but OSD-on-snapshot-2 succeeds. (This
  may require building a borderline-failure fixture; expect to spend
  effort.)
- Eval signal on hard-200: with the paired distance-fix +
  zsum-init, expect:
  - Recall delta: +1–3% on the marginal-SNR bucket (the bucket
    where BP just barely fails and OSD has to recover the codeword
    from BP-softened LLRs).
  - FP delta: 0% (distance-fix protects) or small negative if the
    pancetta baseline already had the distance-fix.
- If pancetta's existing OSD already has the distance-fix (audit
  needed), the zsum-init can land without the FP guard concern;
  ship as a pure recall-lift mechanism.
- Cite as `wsjtr-inspired` in the journal entry; the underlying
  algorithm originates in WSJT-X's Fortran (`decode174_91.f90` and
  `osd174_91.f90`), the wsjtr source is the inspection target, the
  pancetta implementation is independent.
- Recommended sequencing:
  - Session 1: audit pancetta's OSD for the min-distance-tracking
    behaviour. Apply the fix if missing. Test on synthetic FP
    scenarios.
  - Session 2: implement Part A + Part B; unit-test on synthetic
    marginal-failure scenarios.
  - Session 3: hard-200 A/B eval; bootstrap-CI; integrate into
    tier gates if positive.
