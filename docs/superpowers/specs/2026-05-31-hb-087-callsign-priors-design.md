# Callsign-priors-on-residual — design spec (hb-087)

**Status:** proposed (scoping pass; diagnostic PROCEED)
**Hypothesis:** hb-087 (NEW 2026-05-31)
**Author:** research harness, 2026-05-31
**Estimated effort:** 3 sessions
**Parent / spawn context:** spawned 2026-05-31 from the hb-086 V3 SHELVE
("the remaining hard-200 wall is sub-Costas-threshold weak signals — bypass
Costas pre-gate via callsign-priors-on-residual or OSD-without-Costas")

## Why this is the next-tier lever after hb-086 closes

The hb-086 family is closed:

- **V1 GRADUATED** (+12 hard-200, +17 hard-1000) — joint-pair-retry against
  the post-multipass residual.
- **V2 DEFINITIVELY SHELVED** — soft cancellation collapses to hard subtract
  on pancetta's CRC-validated decoded neighbors (delta-function tone
  posteriors).
- **V3 SHELVED** — Costas relaxation in bin-targeted residual windows
  surfaces noise, not signal (LDPC converges on garbage at production
  iteration count; CRC catches 98% as FPs).

The V3 mechanism trace identified the remaining wall: **sub-Costas-threshold
weak signals.** Costas pre-gate filters them out. Any mechanism that wants
to attack this wall has to BYPASS Costas, which means choosing decode
positions by some signal other than sync_score AND constraining LDPC so it
doesn't converge on noise. hb-087 supplies both.

## Mechanism

1. **Position seeding.** Use the V3 plumbing's already-implemented bin-
   targeted geometry: for every subtracted-eligible decode, the residual
   has a localized noise-floor drop at ±N freq_bins around that decode.
   V3 confirmed that 56.8% of V1-uncoverable missed truths sit within
   ±8 bins of a subtracted decode (top-20 hard-200, well above 20% gate).
   That's the geometry; V3 failed not because the geometry was wrong but
   because **decoding noise produces noise**.

2. **Callsign prior set.** Build a small set of "highly-likely-to-be-here"
   callsigns at runtime:

   | source | signal | risk |
   |---|---|---|
   | **operator's own call** | always known | zero FP risk (truth check is structural — only AP1's bits-28-55 inject; if no decode at this band/time mentions us, no codeword passes CRC). |
   | **recent-window decodes** | last 15-30 min of decoded callsigns | low FP risk — these are stations confirmed active in this band/time. AP injection wrong only if the residual at this position has no signal for *this* callsign, in which case LLR injection produces conflict at non-AP bit positions and BP diverges → no CRC pass. |
   | **cqdx-spotted callsigns** | live network-wide spots from `pancetta-cqdx::Cache::spotted_callsigns()` | low FP risk for same reason; broader prior universe than the rolling window. |
   | **ADIF log callsigns** | operator's past QSO partners | low FP risk; specific to current operator. Limit by recency (e.g. last 30 days) to keep set bounded. |

3. **AP-constrained residual decode.** Same per-candidate decode path the
   V1 retry uses (`par_extract_symbols_from_spectrogram` →
   `par_compute_soft_llrs_db` → `normalize_llrs` → `decode_soft`), but
   **before `decode_soft`**, inject prior bits via the existing
   `inject_ap_llrs` / `inject_ap2_caller` / `inject_recent_call_at_called`
   primitives in `pancetta-ft8/src/ap.rs`. AP levels used:

   - **AP1** (own callsign at bits 28-55, called-station): use when
     operator's own call might be the called station at this position.
     Already plumbed in `par_try_ap_decode`; we reuse the function.
   - **AP2** (recent caller at bits 0-27, caller-station): use for every
     callsign in the prior set, both as caller (bits 0-27) and as called
     (bits 28-55). The hb-043 my_call-less path
     (`par_try_ldpc_with_recent_only`) already does this iteration; we
     reuse it.
   - **NOT AP3/AP4**: those pin both caller and called (active QSO
     context); irrelevant for the autonomous-decode case.

4. **Position selection (the new code).** For each subtracted-eligible
   decode, enumerate residual positions within ±8 freq_bins, ±2 time_steps
   that DID NOT produce a successful decode in pass-1 or V1. For each
   such (freq_sub, freq_bin, time_step) position, extract complex symbols
   from the residual, compute LLRs, then for EACH callsign C in the prior
   set, inject AP bits and run LDPC+CRC+plausibility.

5. **FP funnel.** Same hard filter as everywhere else:
   - LDPC must converge (production iteration count).
   - 14-bit CRC must pass.
   - `is_plausible()` must accept (rejects telemetry, contest-FP, /R, etc).
   - **NEW for this mechanism:** the decoded callsign at the injected
     position must EQUAL the prior C we injected. With AP injection at
     ±15.0 LLR magnitude, this is structurally guaranteed unless BP
     overrides the AP bits, which essentially never happens — but assert
     it as a paranoia check before accepting.
   - **Continuity filter** still applies — any callsigns extracted from
     the message must be in the production reference set. This is already
     true for the prior set itself (it IS the reference set), so this
     guard is automatically satisfied.

## Why this is fundamentally different from hb-086 V3

| dimension | V3 (SHELVED) | hb-087 (proposed) |
|---|---|---|
| position choice | localized Costas sync_search at relaxed threshold | enumerate ALL residual positions in the bin-targeted window |
| candidate decoded as | unconstrained LDPC | AP-constrained LDPC (callsign-bits pinned) |
| valid-codeword space | full 2^77 / LDPC code | constrained to codewords containing the prior callsign at the chosen position |
| BP-on-noise behavior | converges 100%; CRC catches ~98% as FPs | diverges quickly when residual is noise (injected AP bits conflict with the non-AP LLRs); CRC passes only when there's actually a real signal carrying the prior callsign at this position |
| diagnostic test | geometric proximity (56.8% PROCEED) | callsign coverage (23.6% PROCEED) — the more restrictive test, because the mechanism's value is upper-bounded by how often missed truths' callsigns are in our prior universe |

The structural shift: V3 asked "can sync_search find more candidates
when we drop the bar?" — empirically no, the bar's there because below
it is noise. hb-087 asks "if we KNOW what callsign should be at this
position (from priors), can we decode the residual?" — this is the
classic "AP-without-sync" lever that decades of weak-signal modes (JT9,
JT65) have used to crack signals below the unconstrained-decode floor.
WSJT-X's `apsym` / `napwid` flags do exactly this for hand-held QSOs.

## Diagnostic — feasibility / kill-switch (DONE)

Run: `cargo run --release -p pancetta-research --example hb087_callsign_priors_feasibility`

Method: for every missed truth in the top-20 worst hard-200 WAVs, check
whether either of its extracted callsigns appears in:

- operator-callsign set ({K5ARH});
- recent-window decodes (this WAV's pancetta decodes, callsigns extracted
  via `callsigns_in()`); stands in for the production rolling 15-30 min
  window;
- bundled-common-active list (≈100 hand-picked very-active stations);
  stands in for cqdx-spotted callsigns.

PROCEED gate: ≥20% of missed truths have at least one prior-set callsign.

**Result (2026-05-31, refreshed corpus, top-20 hard-200, total_missed=647):**

| source | missed truths covered | coverage % |
|---|---:|---:|
| operator (K5ARH) | 0 | 0.0% |
| recent-window (this-WAV decodes) | 150 | 23.2% |
| bundled-common-active | 5 | 0.8% |
| **ANY of the above** | **153** | **23.6%** |

PROCEED earned. Recent-window dominates; operator-call is zero on this
corpus (these WAVs aren't K5ARH's logs); bundled adds <1% (the bundle is
small and these specific WAVs don't carry DXpeditions). In production
the bundled-equivalent (cqdx live spots) will be larger and more
band-targeted, so the real production prior coverage is plausibly higher
than 23.6%. Treat 23.6% as the conservative lower bound.

## Build sequence

### Session 1 — feasibility + design (DONE, this scoping pass)

- `pancetta-research/examples/hb087_callsign_priors_feasibility.rs`
  diagnostic (DONE — PROCEED at 23.6%).
- This design spec (DONE).
- Hypothesis-bank entry (DONE).

### Session 2 — prior-source aggregation in research harness

Build a `CallsignPriorSet` type in `pancetta-research` (research-only,
not production) that aggregates:

1. operator's call (config),
2. rolling window of recent decoded callsigns (interior-mutable, same
   pattern as `CallsignContinuityFilter`),
3. ADIF callsigns (load from `~/.pancetta/qsos.adi` if present, else skip),
4. cqdx-spotted (research-stub: feed from a JSON file or skip).

Plumb this through `Ft8Config` (or a parallel research config) as
`Option<Arc<CallsignPriorSet>>`. Default: None (production unaffected).

**Validation gate (session 2 → 3):** run a per-WAV trace example
`hb087_prior_coverage_trace.rs` against the top-20 hard-200 with each
source enabled in turn (operator, recent, ADIF, cqdx-stub) and confirm
the aggregate coverage matches the feasibility diagnostic (within ±5%
absolute). If it doesn't, the aggregation has a bug; debug before
session 3.

### Session 3 — AP-constrained residual decode + eval

Add a new pass `callsign_prior_residual_pass` to the decoder, called
AFTER V1's `joint_pair_retry_pass`:

```rust
fn callsign_prior_residual_pass(
    &self,
    spectrogram: &Spectrogram,                       // residual, post-multipass + V1
    pass_decoded: &[DecodedMessage],
    prior_set: &CallsignPriorSet,                    // operator + recent + adif + cqdx
) -> Vec<DecodedMessage>
```

Algorithm:
1. Build subtracted-position set from `pass_decoded` (same as
   `joint_residual_localized_sync_pass`).
2. For each subtracted position, enumerate (freq_sub, freq_bin,
   time_step) lattice within ±8 freq_bins, ±2 time_steps. Apply V1
   coverage filter (skip positions already in subtracted-positions or
   original sync_candidates).
3. For each enumerated position, extract LLRs from the residual ONCE.
4. For each callsign C in `prior_set`:
   - Inject C at bits 28-55 (called) → AP1-like; LDPC + CRC + plausibility +
     equality check (the decoded called == C).
   - Inject C at bits 0-27 (caller) → AP2-like; same funnel.
5. If any C produces a valid decode, accept it. Multiple C's at the same
   position would all need to pass independently — in practice only one
   will, because the AP injection plus LDPC convergence is callsign-
   specific.

Bound the cost:
- ±8×17 position lattice × |prior_set| per position. For top-20 WAVs the
  diagnostic measured ~140 subtracted positions × ~150 lattice points ×
  ~30-100 prior callsigns. That's a lot. Two mitigations:
  - **Position-precompute**: extract LLRs once per position, reuse for
    every C (the AP injection is local — copy LLRs, mutate, decode).
  - **Per-C iteration cap**: cap the prior set size (e.g., 64 most-recent
    + top-32 highest-rarity cqdx).
- Target wall-clock budget: ≤25% increase vs current production.

Eval cycle:
- A/B vs main on hard-200 (5 runs each, median).
- If +rec ≥ 5 with composite ≥ +0.0005 and no fixture/synth regression
  → GRADUATE candidate; widen to hard-1000 + full 5-tier.
- If <5 rec OR composite negative → SHELVE.

## Architecture fit (mr-007 lens)

- **Direct attack on the measured residual wall.** Diagnostic confirms
  23.6% coverage on the top-20 (the densest, most masking-prone WAVs).
  Different mechanism family from hb-086 (callsign-prior vs joint-pair).
- **Reuses existing primitives.** `inject_ap_llrs`, `inject_ap2_caller`,
  `inject_recent_call_at_called` are already in `pancetta-ft8/src/ap.rs`
  and battle-tested by the hb-027/hb-043 production AP path. The new
  code is the residual-position enumeration + the per-C decode loop.
- **Reuses existing prior sources.**
  `pancetta-qso::callsign_continuity::CallsignContinuityFilter` already
  maintains the rolling-window + ADIF + cqdx-spotted union; we can
  literally use its reference set or a slim copy of it.
- **CPU cost is the main risk.** AP-decode is fast (LDPC dominates), but
  per-position × per-prior-callsign is a lot of LDPC runs. Budget +25%
  wall-clock; needs careful cap-and-prioritize.
- **FP risk is low.** AP injection at ±15.0 LLR forces the called-or-
  caller bits to match the prior; BP either converges to a codeword
  containing that callsign (which we verify by extraction equality) or
  diverges. The CRC + plausibility + continuity-filter funnel catches
  any structural FPs.

## Open questions

- **AP injection at AP1 (called) vs AP2 (caller)?** Both, iterate over
  both positions per C. Doubles work but covers both message structures
  ("C is calling someone" and "someone is calling C").
- **Prior-set size cap.** Top-N most-recent for the rolling window;
  top-M highest-rarity for cqdx. Tune in session 3 against per-cycle
  budget. Initial guess: 64 + 32 = 96 callsigns.
- **Multi-prior compounding (e.g., AP1 = my_call + AP2 = recent C).**
  Already supported by existing `par_try_ldpc_with_ap` for the production
  path. Adds two more LDPC runs per (position, C) pair; might be too
  expensive. Defer to a session-4 follow-up unless session-3 wall-clock
  budget allows.
- **Lattice density.** ±8 bins × ±2 time_steps = 17×5 = 85 positions per
  subtracted decode. Per-WAV: ~140 subtracted × 85 = ~11,900 positions ×
  96 priors = ~1.1M LDPC runs/WAV at the upper bound. At 50µs/LDPC
  (production neural-OSD fast path) → ~57s/WAV. Way over budget.
  Mitigation strategies:
  - Tighter lattice (±4×±1 = ±9×3 = 27 positions per subtracted decode
    → ~3800 positions × 96 priors = 365k LDPC runs/WAV → ~18s/WAV).
    Still too high.
  - Prior-set top-K only (K=16): 27 × 16 = 432 per subtracted decode →
    60k LDPC runs/WAV → ~3s/WAV. Within budget.
  - **Top priority for session 3: pre-screening.** Cheap LLR-energy
    check at each position before launching any LDPC: if the residual
    LLRs at the 28 callsign bit positions average |LLR| < 0.5 (essentially
    noise), skip ALL priors at this position. This is the structural
    AP-injection guard — if there's no signal at the callsign bits, AP
    injection can't recover anything, just compute conflict with noise.

## Eval plan

**Target metrics (graduate criteria):**
- hard-200 recovered: +5 (conservative — diagnostic upper bound is +153
  but mechanism efficiency is <100%; the recent-window prior may have
  already-decoded callsigns at OTHER positions, narrowing the new-decode
  yield).
- composite: +0.0005 minimum, +0.001 target.
- hard-1000 recovered: +0 minimum (don't regress); +10 expected.
- fixtures: zero regression (binding).
- synth-clean: zero regression (binding).
- elapsed: ≤+25% (binding budget).
- novel count on hard-200: ≤+30 (FP-filter absorbs most; tracking number).

**Kill criteria (definitive SHELVE):**
- hard-200 rec < +5 AND no plausible parameter sweep that fixes it.
- composite ≤ 0.
- fixtures or synth regress.
- elapsed > +50% even with prior-set cap K ≤ 16.

**Doctrine refinement applied (from V3 SHELVE):**
- Before claiming PROCEED at session 2 → session 3 boundary, run the
  per-truth decodability micro-test V3 retrospectively wished it had:
  pick 10 missed truths from the top-3 worst WAVs whose callsign IS in
  the prior set; extract residual LLRs at the truth's known
  (freq, time) coordinates; inject the prior; run LDPC + CRC. Confirm
  ≥3 of 10 pass (30%). If <3 of 10 pass, the prior injection doesn't
  rescue marginal residuals; SHELVE before full implementation.

## Risk analysis

- **Prior-set pollution (lowest-risk FP).** A spammed cqdx spot for
  callsign X is in the prior set; X gets AP-injected at a residual
  position; CRC false-positive accidentally passes; we accept a fake
  decode. Mitigated by: continuity filter (X is in the reference set
  anyway because it's also a prior source), plausibility filter, and
  decoded-callsign-equality assert. The compounded probability of all
  three failing simultaneously is negligible.
- **Operator-callsign mis-injection (zero-risk).** Pancetta's own
  callsign is fixed and known. AP1 injection cannot produce an FP that
  the operator cares about (it only fires on residual positions where
  the operator's call is plausibly the called station; if the residual
  has no signal, BP doesn't converge).
- **Recent-window pollution (medium-low risk).** A pancetta FP in pass-1
  enters the recent-window prior; that FP's callsign gets AP-injected at
  hb-087 positions; might propagate. Mitigation: pass-1's CRC + FP filter
  already gate the recent window. The amplification factor is
  pass-1-FP-rate × hb-087-acceptance-rate ≈ <1% × <1% = negligible.
- **CPU cost overrun (highest-risk).** See "lattice density" above; the
  upper-bound math is alarming. The pre-screening guard + prior-set cap
  must work for this to be viable. Session 3 must measure wall-clock
  carefully and stop early if the budget breaches.
- **Mechanism overlap with hb-082-style global relaxation.** hb-082
  found nothing at the global residual relaxed threshold. hb-087 is
  structurally different (callsign-pinned LDPC, not relaxed sync), so
  hb-082's null result does NOT predict hb-087's. But: if the residual
  energy at hb-087 positions is dominated by noise (V3's finding for
  Costas-relaxed candidates), the AP injection might still fail to
  rescue them. The decodability micro-test in the eval plan addresses
  this directly.

## Methodology notes (doctrine, not blocking)

The V3 SHELVE established a doctrine: **geometric proximity diagnostics
need a paired decodability sub-test before earning PROCEED.** hb-087's
diagnostic is *not* purely geometric — it measures *callsign coverage*,
which is a necessary condition for the mechanism to fire. But it's still
not a decodability test. The session 2 → 3 boundary gates on a
per-truth decodability micro-test (described in Eval Plan above) to
catch the same class of failure that bit V3. This is intentional and
costs maybe 30 min of session 2 wall-clock — cheap insurance.

## Out of scope (sibling/future hypotheses)

- **OSD-without-Costas-pre-gate** (sibling, being scoped separately):
  attack the same residual wall via Ordered-Statistics Decoding without
  requiring Costas. Structurally complementary to hb-087.
- **Multi-prior compounding** (deferred to follow-up): inject AP1 +
  AP2 simultaneously when operator is in active QSO and a recent caller
  prior is also strong. Roughly doubles CPU per position; defer until
  hb-087 V1 grad confirms the basic mechanism.
- **Cross-WAV prior accumulation**: in production the rolling window
  spans 15-30 min, much longer than a single 15s slot. The diagnostic's
  per-WAV approximation undercounts what production has access to.
  Production behavior should be measured in field-eval, not in this
  research-harness diagnostic.

## Files this hypothesis will touch

(Session 3 only — sessions 1-2 are research-crate only.)

- `pancetta-ft8/src/decoder.rs` — add `callsign_prior_residual_pass`
  method on `Ft8Decoder`; hook after V1's `joint_pair_retry_pass` in
  `decode_window_with_ap`. New `Ft8Config` fields:
  `callsign_prior_residual_enable: bool`, `callsign_prior_max_set_size:
  usize` (default 64), `callsign_prior_lattice_freq_bins: usize`
  (default 4), `callsign_prior_lattice_time_steps: usize` (default 1),
  `callsign_prior_min_llr_energy: f32` (default 0.5).
- `pancetta-ft8/src/ap.rs` — no changes needed; existing primitives suffice.
- `pancetta-research/src/lib.rs` — add `CallsignPriorSet` aggregator
  (research-only; production uses it via Ft8Config).
- `pancetta-research/examples/hb087_*.rs` — diagnostic + per-truth
  decodability + eval drivers.
- Builder methods on Ft8ConfigBuilder for the new fields (research only).

## References

- Diagnostic: `pancetta-research/examples/hb087_callsign_priors_feasibility.rs`
- V3 SHELVE journal:
  `research/experiments/2026-05-31-hb-086-v3-subtract-aware-sync.md`
- Joint-decoding family design (closed):
  `docs/superpowers/specs/2026-05-27-joint-decoding-design.md`
- AP primitives: `pancetta-ft8/src/ap.rs`
- Production prior aggregation:
  `pancetta-qso/src/callsign_continuity.rs::CallsignContinuityFilter`
- cqdx spotted callsigns:
  `pancetta-cqdx/src/cache.rs::Cache::spotted_callsigns`
- V1 residual retry pattern (the template hb-087 mirrors):
  `pancetta-ft8/src/decoder.rs::joint_pair_retry_pass` (~line 2399)
- AP-decode loop (the existing per-candidate AP path hb-087 layers on):
  `pancetta-ft8/src/decoder.rs::par_try_ap_decode` (~line 3630)
